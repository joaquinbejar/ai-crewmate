use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

pub const TOKEN_PREFIX: &str = "acs_";

/// Identity resolved from the bearer token, injected into the HTTP request
/// extensions so tool handlers can read it. Every tool call is scoped to this.
#[derive(Clone, Debug)]
pub struct AuthCtx {
    pub agent_id: Uuid,
    pub agent_name: String,
    pub team_id: Uuid,
    pub team_slug: String,
}

/// Generate a fresh opaque token. Returned once, never stored in the clear.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{TOKEN_PREFIX}{}", hex::encode(bytes))
}

pub fn hash_token(raw: &str) -> Vec<u8> {
    Sha256::digest(raw.trim().as_bytes()).to_vec()
}

/// First 12 characters, kept in plaintext purely so humans can tell tokens
/// apart in `ai-crew-sync token list`.
pub fn token_prefix(raw: &str) -> String {
    raw.chars().take(12).collect()
}

struct AuthRow {
    token_id: Uuid,
    agent_id: Uuid,
    agent_name: String,
    agent_disabled: bool,
    team_id: Uuid,
    team_slug: String,
}

pub async fn resolve_token(pool: &PgPool, raw: &str) -> Result<AuthCtx, AuthError> {
    if !raw.starts_with(TOKEN_PREFIX) {
        return Err(AuthError::Invalid);
    }
    let hash = hash_token(raw);

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
            Uuid,
            String,
        ),
    >(
        r#"
        SELECT t.id, a.id, a.name, a.disabled_at, tm.id, tm.slug
        FROM api_tokens t
        JOIN agents a ON a.id = t.agent_id
        JOIN teams tm ON tm.id = a.team_id
        WHERE t.token_hash = $1 AND t.revoked_at IS NULL
        "#,
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "token lookup failed");
        AuthError::Internal
    })?;

    let Some((token_id, agent_id, agent_name, disabled_at, team_id, team_slug)) = row else {
        return Err(AuthError::Invalid);
    };
    let row = AuthRow {
        token_id,
        agent_id,
        agent_name,
        agent_disabled: disabled_at.is_some(),
        team_id,
        team_slug,
    };

    if row.agent_disabled {
        return Err(AuthError::Disabled);
    }

    // Best-effort: record usage without blocking the request path on failure.
    let _ = sqlx::query("UPDATE api_tokens SET last_used_at = now() WHERE id = $1")
        .bind(row.token_id)
        .execute(pool)
        .await;

    Ok(AuthCtx {
        agent_id: row.agent_id,
        agent_name: row.agent_name,
        team_id: row.team_id,
        team_slug: row.team_slug,
    })
}

#[derive(Debug)]
pub enum AuthError {
    Missing,
    Invalid,
    Disabled,
    Internal,
    /// Too many requests for this token; carries the seconds to wait.
    Throttled(u64),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let retry_after = match self {
            AuthError::Throttled(secs) => Some(secs),
            _ => None,
        };
        // The consumer is a language model: say what to do, not just what
        // went wrong.
        let (status, msg) = match self {
            AuthError::Missing => (StatusCode::UNAUTHORIZED, "missing bearer token".to_owned()),
            AuthError::Invalid => (
                StatusCode::UNAUTHORIZED,
                "invalid or revoked token".to_owned(),
            ),
            AuthError::Disabled => (StatusCode::FORBIDDEN, "agent is disabled".to_owned()),
            AuthError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            ),
            AuthError::Throttled(secs) => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "rate limit exceeded for this token; retry in {secs}s. \
                     If you are polling, use wait_for_updates (it blocks until \
                     something happens) instead of calling in a loop."
                ),
            ),
        };
        let body = serde_json::json!({ "error": msg });
        let mut resp = (status, axum::Json(body)).into_response();
        if matches!(self, AuthError::Missing | AuthError::Invalid) {
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        if let Some(secs) = retry_after
            && let Ok(value) = axum::http::HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        resp
    }
}

/// State for [`require_bearer`]: the pool plus the optional rate limiter.
#[derive(Clone)]
pub struct AuthState {
    pub pool: PgPool,
    pub limiter: Option<crate::ratelimit::RateLimiter>,
}

/// Axum middleware: validates the bearer token, charges the per-token rate
/// limit, and inserts the resulting [`AuthCtx`] into the request extensions,
/// where rmcp tool handlers read it.
pub async fn require_bearer(
    State(state): State<AuthState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AuthError> {
    let raw = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(AuthError::Missing)?
        .to_owned();

    // Charge the bucket on the token's hash before touching the database, so
    // a flood of invalid tokens costs no query either.
    if let Some(limiter) = &state.limiter
        && let Err(throttled) = limiter.check(&hex::encode(hash_token(&raw)))
    {
        return Err(AuthError::Throttled(throttled.retry_after_secs));
    }

    let ctx = resolve_token(&state.pool, &raw).await?;
    tracing::debug!(agent = %ctx.agent_name, team = %ctx.team_slug, "authenticated");
    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}
