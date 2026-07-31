use std::time::Duration;

use anyhow::Context;
use axum::{Router, extract::State, routing::get};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use crate::{auth, dashboard, events::EventHub, tools::Bus, webhooks};

pub struct ServeOptions {
    pub bind: String,
    /// Hostnames accepted in the `Host` header. Empty disables the check, which
    /// is what you want behind a proxy that already validates it.
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
}

async fn health(State(pool): State<PgPool>) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    match sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&pool).await {
        Ok(_) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({ "status": "ok", "database": "up" })),
        ),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({ "status": "degraded", "database": "down" })),
            )
        }
    }
}

pub fn build_router(pool: PgPool, opts: &ServeOptions, ct: CancellationToken) -> Router {
    // One LISTEN connection feeds every in-process consumer: wait_for_updates
    // long-polls and the webhook dispatcher.
    let hub = EventHub::new();
    tokio::spawn(crate::events::run_pg_listener(
        pool.clone(),
        hub.clone(),
        ct.clone(),
    ));
    tokio::spawn(webhooks::run_dispatcher(pool.clone(), hub.clone(), ct.clone()));

    let mut config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        // Sessions are gone in the current MCP protocol revision; running
        // stateless means any instance can serve any request, so the service
        // scales horizontally behind a plain load balancer.
        .with_legacy_session_mode(false)
        .with_sse_keep_alive(Some(Duration::from_secs(30)))
        .with_cancellation_token(ct);

    config = if opts.allowed_hosts.is_empty() {
        config.disable_allowed_hosts()
    } else {
        config.with_allowed_hosts(opts.allowed_hosts.clone())
    };
    if !opts.allowed_origins.is_empty() {
        config = config.with_allowed_origins(opts.allowed_origins.clone());
    }

    let mcp: StreamableHttpService<Bus, LocalSessionManager> = StreamableHttpService::new(
        {
            let pool = pool.clone();
            let hub = hub.clone();
            move || Ok(Bus::new(pool.clone(), hub.clone()))
        },
        Default::default(),
        config,
    );

    let mcp_routes = Router::new()
        .nest_service("/mcp", mcp)
        // Every /mcp request must carry a valid bearer token; the middleware
        // injects the resolved AuthCtx that tool handlers read.
        .layer(axum::middleware::from_fn_with_state(
            pool.clone(),
            auth::require_bearer,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/dashboard", get(dashboard::render))
        .merge(mcp_routes)
        .layer(TraceLayer::new_for_http())
        .with_state(pool)
}

pub async fn run(pool: PgPool, opts: ServeOptions) -> anyhow::Result<()> {
    let ct = CancellationToken::new();
    let app = build_router(pool, &opts, ct.child_token());

    let listener = tokio::net::TcpListener::bind(&opts.bind)
        .await
        .with_context(|| format!("failed to bind {}", opts.bind))?;
    let addr = listener.local_addr()?;
    tracing::info!(%addr, "ai-crewmate listening; MCP endpoint at /mcp");

    let shutdown = {
        let ct = ct.clone();
        async move {
            let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
            #[cfg(unix)]
            let term = async {
                if let Ok(mut s) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                {
                    s.recv().await;
                }
            };
            #[cfg(not(unix))]
            let term = std::future::pending::<()>();

            tokio::select! {
                _ = ctrl_c => {},
                _ = term => {},
            }
            tracing::info!("shutdown signal received");
            ct.cancel();
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("server error")?;
    Ok(())
}
