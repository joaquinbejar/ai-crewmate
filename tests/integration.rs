//! End-to-end tests against a real Postgres and the real HTTP surface.
//!
//! Two MCP clients ("joaquin" and "marta") connect over Streamable HTTP with
//! their own bearer tokens, exactly as two teammates' Claude Code instances
//! would, and are checked for the properties that actually matter: isolation
//! between teams, identity that cannot be spoofed, and task claims that do not
//! hand the same work to two agents.
//!
//! Requires `TEST_DATABASE_URL` (or `DATABASE_URL`); skipped when unset.

use std::sync::Arc;

use ai_crewmate::{
    MIGRATOR,
    auth::{generate_token, hash_token, token_prefix},
    serve::{ServeOptions, build_router},
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Each test gets its own schema so they can run concurrently without
/// tripping over each other's rows.
struct Harness {
    pool: PgPool,
    base: String,
    _ct: CancellationToken,
}

async fn setup(schema: &str) -> Option<Harness> {
    let url = db_url()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect({
            let schema = schema.to_owned();
            move |conn, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}"))
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&url)
        .await
        .expect("connect");

    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    MIGRATOR.run(&pool).await.expect("migrate");

    let ct = CancellationToken::new();
    let app = build_router(
        pool.clone(),
        &ServeOptions {
            bind: String::new(),
            allowed_hosts: vec![],
            allowed_origins: vec![],
        },
        ct.child_token(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Some(Harness {
        pool,
        base: format!("http://{addr}"),
        _ct: ct,
    })
}

/// Create a team + agent + token straight in the database, the way the CLI does.
async fn seed_agent(pool: &PgPool, team: &str, agent: &str) -> String {
    let team_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO teams (slug, name) VALUES ($1, $1)
         ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name RETURNING id",
    )
    .bind(team)
    .fetch_one(pool)
    .await
    .unwrap();

    let agent_id: (Uuid,) =
        sqlx::query_as("INSERT INTO agents (team_id, name) VALUES ($1, $2) RETURNING id")
            .bind(team_id.0)
            .bind(agent)
            .fetch_one(pool)
            .await
            .unwrap();

    let raw = generate_token();
    sqlx::query("INSERT INTO api_tokens (agent_id, token_hash, prefix) VALUES ($1, $2, $3)")
        .bind(agent_id.0)
        .bind(hash_token(&raw))
        .bind(token_prefix(&raw))
        .execute(pool)
        .await
        .unwrap();
    raw
}

type Client = RunningService<rmcp::RoleClient, ClientInfo>;

async fn connect(base: &str, token: &str) -> Client {
    let mut config = StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"));
    config.auth_header = Some(token.to_string());
    config.allow_stateless = true;
    let transport = StreamableHttpClientTransport::from_config(config);
    ClientInfo::default()
        .serve(transport)
        .await
        .expect("mcp handshake")
}

/// Call a tool and return its structured output.
async fn call(client: &Client, name: &str, args: Value) -> Value {
    let args: serde_json::Map<String, Value> = serde_json::from_value(args).unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args))
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e}"));
    assert_ne!(
        result.is_error,
        Some(true),
        "{name} returned an error: {result:?}"
    );
    result
        .structured_content
        .clone()
        .unwrap_or_else(|| panic!("{name} returned no structured content: {result:?}"))
}

/// Call a tool expecting the server to reject it.
async fn call_expect_error(client: &Client, name: &str, args: Value) -> String {
    let args: serde_json::Map<String, Value> = serde_json::from_value(args).unwrap();
    match client
        .call_tool(CallToolRequestParams::new(name.to_string()).with_arguments(args))
        .await
    {
        Err(e) => e.to_string(),
        Ok(result) => {
            assert_eq!(
                result.is_error,
                Some(true),
                "{name} unexpectedly succeeded: {result:?}"
            );
            format!("{:?}", result.content)
        }
    }
}

macro_rules! require_db {
    ($schema:expr) => {
        match setup($schema).await {
            Some(h) => h,
            None => {
                eprintln!("skipping: TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = require_db!("t_auth");
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/mcp", h.base))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "no token must be rejected");

    let resp = client
        .post(format!("{}/mcp", h.base))
        .header("Authorization", "Bearer acm_deadbeef")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "bogus token must be rejected");

    // Health is deliberately open so load balancers can probe it.
    let resp = client
        .get(format!("{}/health", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn revoked_token_stops_working() {
    let h = require_db!("t_revoke");
    let token = seed_agent(&h.pool, "acme", "joaquin").await;
    let client = connect(&h.base, &token).await;
    call(&client, "whoami", json!({})).await;
    let _ = client.cancel().await;

    sqlx::query("UPDATE api_tokens SET revoked_at = now()")
        .execute(&h.pool)
        .await
        .unwrap();

    let resp = reqwest::Client::new()
        .post(format!("{}/mcp", h.base))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn tools_are_advertised_with_schemas() {
    let h = require_db!("t_tools");
    let token = seed_agent(&h.pool, "acme", "joaquin").await;
    let client = connect(&h.base, &token).await;

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "whoami",
        "post_message",
        "read_messages",
        "list_channels",
        "create_channel",
        "search_messages",
        "ask_agent",
        "create_task",
        "claim_task",
        "claim_next_task",
        "complete_task",
        "release_task",
        "renew_task_lease",
        "list_tasks",
        "get_task",
        "heartbeat",
        "list_agents",
        "set_note",
        "get_note",
        "list_notes",
        "search_notes",
        "delete_note",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected} in {names:?}"
        );
    }
    for tool in &tools {
        assert!(
            tool.description.as_ref().is_some_and(|d| d.len() > 20),
            "tool {} needs a usable description",
            tool.name
        );
    }
    let _ = client.cancel().await;
}

#[tokio::test]
async fn direct_messages_and_channels_flow_between_two_agents() {
    let h = require_db!("t_msg");
    let joaquin_token = seed_agent(&h.pool, "acme", "joaquin").await;
    let marta_token = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &joaquin_token).await;
    let marta = connect(&h.base, &marta_token).await;

    // Identity comes from the token, not from an argument.
    let me = call(&joaquin, "whoami", json!({})).await;
    assert_eq!(me["agent"], "joaquin");
    assert_eq!(me["team"], "acme");

    // Direct message: only the recipient sees it in their inbox.
    call(
        &joaquin,
        "post_message",
        json!({"to": "marta", "body": "the auth refactor touches your billing module"}),
    )
    .await;

    let inbox = call(&marta, "read_messages", json!({"scope": "inbox"})).await;
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["messages"][0]["from"], "joaquin");
    assert_eq!(inbox["messages"][0]["to"], "marta");

    // The read cursor advanced, so a second read returns nothing new.
    let again = call(&marta, "read_messages", json!({"scope": "inbox"})).await;
    assert_eq!(again["messages"].as_array().unwrap().len(), 0);

    // ...unless we explicitly ask for history.
    let history = call(
        &marta,
        "read_messages",
        json!({"scope": "inbox", "only_new": false}),
    )
    .await;
    assert_eq!(history["messages"].as_array().unwrap().len(), 1);

    // The sender's own inbox stays empty.
    let joaquin_inbox = call(&joaquin, "read_messages", json!({"scope": "inbox"})).await;
    assert_eq!(joaquin_inbox["messages"].as_array().unwrap().len(), 0);

    // Channels are shared by the whole team.
    call(
        &marta,
        "create_channel",
        json!({"name": "#Deploys", "topic": "what is going out"}),
    )
    .await;
    let channels = call(&joaquin, "list_channels", json!({})).await;
    assert_eq!(
        channels["channels"][0]["name"], "deploys",
        "name normalised"
    );

    call(
        &marta,
        "post_message",
        json!({"channel": "deploys", "body": "staging is on 1.4.2"}),
    )
    .await;
    let read = call(&joaquin, "read_messages", json!({"scope": "deploys"})).await;
    assert_eq!(read["messages"][0]["body"], "staging is on 1.4.2");
    assert_eq!(read["messages"][0]["from"], "marta");

    // Full-text search finds it without disturbing cursors.
    let found = call(&joaquin, "search_messages", json!({"query": "staging"})).await;
    assert_eq!(found["messages"].as_array().unwrap().len(), 1);

    // Sending to an unknown agent is a clean, explanatory error.
    let err = call_expect_error(
        &joaquin,
        "post_message",
        json!({"to": "nobody", "body": "hi"}),
    )
    .await;
    assert!(
        err.contains("nobody"),
        "error should name the missing agent: {err}"
    );

    // A message must have exactly one target.
    let err = call_expect_error(&joaquin, "post_message", json!({"body": "hi"})).await;
    assert!(err.to_lowercase().contains("channel"), "got: {err}");

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn teams_are_isolated_from_each_other() {
    let h = require_db!("t_isolation");
    let acme = seed_agent(&h.pool, "acme", "joaquin").await;
    let other = seed_agent(&h.pool, "globex", "intruder").await;
    let acme_client = connect(&h.base, &acme).await;
    let other_client = connect(&h.base, &other).await;

    call(&acme_client, "create_channel", json!({"name": "secrets"})).await;
    call(
        &acme_client,
        "post_message",
        json!({"channel": "secrets", "body": "the api key is in vault"}),
    )
    .await;
    call(
        &acme_client,
        "set_note",
        json!({"scope": "api", "key": "vault-path", "value": "secret/prod/api"}),
    )
    .await;
    call(
        &acme_client,
        "create_task",
        json!({"key": "rotate-keys", "title": "rotate the prod keys"}),
    )
    .await;

    // The other team sees none of it.
    let channels = call(&other_client, "list_channels", json!({})).await;
    assert_eq!(channels["channels"].as_array().unwrap().len(), 0);

    let msgs = call(&other_client, "read_messages", json!({"scope": "all"})).await;
    assert_eq!(msgs["messages"].as_array().unwrap().len(), 0);

    let notes = call(&other_client, "list_notes", json!({})).await;
    assert_eq!(notes["notes"].as_array().unwrap().len(), 0);

    let tasks = call(&other_client, "list_tasks", json!({})).await;
    assert_eq!(tasks["tasks"].as_array().unwrap().len(), 0);

    // Not even by name.
    let found = call(&other_client, "search_messages", json!({"query": "vault"})).await;
    assert_eq!(found["messages"].as_array().unwrap().len(), 0);

    let err = call_expect_error(&other_client, "get_task", json!({"key": "rotate-keys"})).await;
    assert!(err.contains("not found"), "got: {err}");

    let agents = call(&other_client, "list_agents", json!({})).await;
    let names: Vec<&str> = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["intruder"]);

    let _ = acme_client.cancel().await;
    let _ = other_client.cancel().await;
}

#[tokio::test]
async fn a_claimed_task_cannot_be_claimed_by_someone_else() {
    let h = require_db!("t_claim");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &joaquin,
        "create_task",
        json!({"key": "refactor-auth", "title": "rewrite the token refresh flow"}),
    )
    .await;

    let claim = call(
        &joaquin,
        "claim_task",
        json!({"key": "refactor-auth", "lease_seconds": 600}),
    )
    .await;
    assert_eq!(claim["claimed"], true);
    assert_eq!(claim["task"]["claimed_by"], "joaquin");

    // Marta is refused, and told why rather than getting a bare failure.
    let denied = call(&marta, "claim_task", json!({"key": "refactor-auth"})).await;
    assert_eq!(denied["claimed"], false);
    assert!(
        denied["reason"].as_str().unwrap().contains("joaquin"),
        "reason should name the holder: {denied:?}"
    );

    // Re-claiming your own task is idempotent, not an error.
    let again = call(&joaquin, "claim_task", json!({"key": "refactor-auth"})).await;
    assert_eq!(again["claimed"], true);

    // Marta cannot renew or release a lease she does not hold.
    let err = call_expect_error(&marta, "release_task", json!({"key": "refactor-auth"})).await;
    assert!(err.contains("do not hold"), "got: {err}");
    let err = call_expect_error(&marta, "renew_task_lease", json!({"key": "refactor-auth"})).await;
    assert!(err.contains("do not hold"), "got: {err}");

    // Once released, it is up for grabs again.
    call(&joaquin, "release_task", json!({"key": "refactor-auth"})).await;
    let retry = call(&marta, "claim_task", json!({"key": "refactor-auth"})).await;
    assert_eq!(retry["claimed"], true);
    assert_eq!(retry["task"]["claimed_by"], "marta");

    // Completing records the result and closes the task.
    let done = call(
        &marta,
        "complete_task",
        json!({"key": "refactor-auth", "result": "merged in #421"}),
    )
    .await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["result"], "merged in #421");

    let err = call_expect_error(&joaquin, "complete_task", json!({"key": "refactor-auth"})).await;
    assert!(err.contains("already done"), "got: {err}");

    // The history is a full audit trail.
    let detail = call(&joaquin, "get_task", json!({"key": "refactor-auth"})).await;
    let events: Vec<&str> = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        vec![
            "created",
            "claimed",
            "claimed",
            "released",
            "claimed",
            "completed"
        ]
    );

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn an_expired_lease_can_be_taken_over() {
    let h = require_db!("t_lease");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &joaquin,
        "create_task",
        json!({"key": "long-job", "title": "reindex everything"}),
    )
    .await;
    call(&joaquin, "claim_task", json!({"key": "long-job"})).await;

    // Simulate an agent that died mid-task: the lease lapses.
    sqlx::query("UPDATE tasks SET lease_expires_at = now() - interval '1 minute'")
        .execute(&h.pool)
        .await
        .unwrap();

    let listed = call(&marta, "list_tasks", json!({})).await;
    assert_eq!(listed["tasks"][0]["lease_expired"], true);

    let stolen = call(&marta, "claim_task", json!({"key": "long-job"})).await;
    assert_eq!(
        stolen["claimed"], true,
        "an expired lease must be reclaimable"
    );
    assert_eq!(stolen["task"]["claimed_by"], "marta");

    // Renewing pushes the expiry back out.
    let renewed = call(
        &marta,
        "renew_task_lease",
        json!({"key": "long-job", "lease_seconds": 3600}),
    )
    .await;
    assert_eq!(renewed["lease_expired"], false);

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn concurrent_claim_next_never_hands_out_the_same_task_twice() {
    let h = require_db!("t_race");
    let mut clients = Vec::new();
    for i in 0..4 {
        let token = seed_agent(&h.pool, "acme", &format!("agent{i}")).await;
        clients.push(Arc::new(connect(&h.base, &token).await));
    }

    // Four tasks, four agents, all grabbing at once.
    for i in 0..4 {
        call(
            &clients[0],
            "create_task",
            json!({"key": format!("job-{i}"), "title": format!("job {i}")}),
        )
        .await;
    }

    let mut handles = Vec::new();
    for client in &clients {
        let client = Arc::clone(client);
        handles.push(tokio::spawn(async move {
            call(&client, "claim_next_task", json!({})).await
        }));
    }
    let mut keys = Vec::new();
    for handle in handles {
        let result = handle.await.unwrap();
        assert_eq!(result["claimed"], true);
        keys.push(result["task"]["key"].as_str().unwrap().to_owned());
    }
    keys.sort();
    keys.dedup();
    assert_eq!(
        keys.len(),
        4,
        "each agent must get a distinct task: {keys:?}"
    );

    // With nothing left, the pool is empty rather than erroring.
    let empty = call(&clients[0], "claim_next_task", json!({})).await;
    assert_eq!(empty["claimed"], false);
    assert!(empty["reason"].as_str().unwrap().contains("no unclaimed"));
}

#[tokio::test]
async fn presence_expires_and_is_visible_to_the_team() {
    let h = require_db!("t_presence");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &joaquin,
        "heartbeat",
        json!({"repo": "acme/api", "branch": "feat/auth", "activity": "rewriting token refresh"}),
    )
    .await;

    let seen = call(&marta, "list_agents", json!({"online_only": true})).await;
    assert_eq!(seen["online_count"], 1);
    assert_eq!(seen["agents"][0]["name"], "joaquin");
    assert_eq!(seen["agents"][0]["activity"], "rewriting token refresh");
    assert_eq!(seen["agents"][0]["repo"], "acme/api");

    // A later heartbeat that omits a field keeps the previous value.
    call(&joaquin, "heartbeat", json!({"status": "blocked"})).await;
    let seen = call(&marta, "list_agents", json!({"online_only": true})).await;
    assert_eq!(seen["agents"][0]["status"], "blocked");
    assert_eq!(seen["agents"][0]["repo"], "acme/api", "repo should persist");

    // When the lease lapses the agent reads as offline, not as stale-but-active.
    sqlx::query("UPDATE agent_presence SET expires_at = now() - interval '1 minute'")
        .execute(&h.pool)
        .await
        .unwrap();
    let seen = call(&marta, "list_agents", json!({})).await;
    assert_eq!(seen["online_count"], 0);
    let joaquin_row = seen["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "joaquin")
        .unwrap();
    assert_eq!(joaquin_row["status"], "offline");

    let err = call_expect_error(&joaquin, "heartbeat", json!({"status": "vibing"})).await;
    assert!(err.contains("active"), "got: {err}");

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn notes_are_shared_memory_with_history() {
    let h = require_db!("t_notes");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &joaquin,
        "set_note",
        json!({
            "scope": "api",
            "key": "why-no-redis",
            "value": "we dropped redis in march; the cache lives in postgres now",
            "tags": ["Infra", "decision"]
        }),
    )
    .await;

    // Marta reads what Joaquin wrote, tags normalised.
    let note = call(
        &marta,
        "get_note",
        json!({"scope": "api", "key": "why-no-redis"}),
    )
    .await;
    assert_eq!(note["found"], true);
    assert_eq!(note["note"]["updated_by"], "joaquin");
    assert_eq!(note["note"]["tags"][0], "infra");

    // Missing notes report found=false instead of erroring.
    let missing = call(&marta, "get_note", json!({"key": "nope"})).await;
    assert_eq!(missing["found"], false);
    assert!(missing["note"].is_null());

    // Overwrites keep a revision trail.
    call(
        &marta,
        "set_note",
        json!({"scope": "api", "key": "why-no-redis", "value": "correction: valkey, not redis"}),
    )
    .await;
    let note = call(
        &joaquin,
        "get_note",
        json!({"scope": "api", "key": "why-no-redis"}),
    )
    .await;
    assert_eq!(note["note"]["updated_by"], "marta");
    let (revisions,): (i64,) = sqlx::query_as("SELECT count(*) FROM note_revisions")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(revisions, 2, "both versions retained");

    // Scope and tag filtering.
    call(
        &joaquin,
        "set_note",
        json!({"scope": "web", "key": "build", "value": "vite, not webpack", "tags": ["infra"]}),
    )
    .await;
    let api_only = call(&marta, "list_notes", json!({"scope": "api"})).await;
    assert_eq!(api_only["notes"].as_array().unwrap().len(), 1);
    let all = call(&marta, "list_notes", json!({})).await;
    assert_eq!(all["notes"].as_array().unwrap().len(), 2);
    let tagged = call(&marta, "list_notes", json!({"tag": "infra"})).await;
    assert_eq!(tagged["notes"].as_array().unwrap().len(), 1);

    let found = call(&marta, "search_notes", json!({"query": "valkey"})).await;
    assert_eq!(found["notes"].as_array().unwrap().len(), 1);

    let del = call(
        &marta,
        "delete_note",
        json!({"scope": "api", "key": "why-no-redis"}),
    )
    .await;
    assert_eq!(del["ok"], true);
    let del_again = call(
        &marta,
        "delete_note",
        json!({"scope": "api", "key": "why-no-redis"}),
    )
    .await;
    assert_eq!(del_again["ok"], false, "deleting twice is not an error");

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn whoami_reports_pending_work() {
    let h = require_db!("t_whoami");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &marta,
        "post_message",
        json!({"to": "joaquin", "body": "ping"}),
    )
    .await;
    call(
        &marta,
        "post_message",
        json!({"to": "joaquin", "body": "ping again"}),
    )
    .await;
    call(
        &joaquin,
        "create_task",
        json!({"key": "t1", "title": "something"}),
    )
    .await;
    call(&joaquin, "claim_task", json!({"key": "t1"})).await;

    let me = call(&joaquin, "whoami", json!({})).await;
    assert_eq!(me["unread_direct_messages"], 2);
    assert_eq!(me["open_claimed_tasks"], 1);

    call(&joaquin, "read_messages", json!({"scope": "inbox"})).await;
    let me = call(&joaquin, "whoami", json!({})).await;
    assert_eq!(me["unread_direct_messages"], 0);

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

// ------------------------------------------------------------ v0.2 features --

#[tokio::test]
async fn wait_for_updates_wakes_on_a_teammates_message() {
    let h = require_db!("t_wait");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;
    call(&joaquin, "create_channel", json!({"name": "dev"})).await;

    // Joaquin blocks waiting; Marta posts shortly after.
    let waiter = {
        let base = h.base.clone();
        let token = a.clone();
        tokio::spawn(async move {
            let client = connect(&base, &token).await;
            let started = std::time::Instant::now();
            let result = call(&client, "wait_for_updates", json!({"timeout_seconds": 20})).await;
            let _ = client.cancel().await;
            (result, started.elapsed())
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    call(
        &marta,
        "post_message",
        json!({"channel": "dev", "body": "he subido el fix del parser"}),
    )
    .await;

    let (result, elapsed) = waiter.await.unwrap();
    assert_eq!(result["woke"], true, "must wake on the message: {result:?}");
    assert_eq!(result["timed_out"], false);
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "woke by event, not by timeout (took {elapsed:?})"
    );
    let summaries = result["events"].as_array().unwrap();
    assert!(
        summaries
            .iter()
            .any(|e| e["summary"].as_str().unwrap().contains("marta")),
        "event should name the sender: {summaries:?}"
    );

    // With unread messages already pending, the wait returns immediately.
    let instant = call(&joaquin, "wait_for_updates", json!({"timeout_seconds": 30})).await;
    assert_eq!(instant["woke"], true);

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn ask_agent_returns_the_teammates_answer() {
    let h = require_db!("t_ask");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    // Joaquin asks and blocks; Marta reads the question and replies to it.
    let asker = {
        let base = h.base.clone();
        let token = a.clone();
        tokio::spawn(async move {
            let client = connect(&base, &token).await;
            let started = std::time::Instant::now();
            let result = call(
                &client,
                "ask_agent",
                json!({"to": "marta", "question": "does staging run pg16?", "timeout_seconds": 20}),
            )
            .await;
            let _ = client.cancel().await;
            (result, started.elapsed())
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let inbox = call(&marta, "read_messages", json!({"scope": "inbox"})).await;
    let question = inbox["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        question["metadata"]["question"], true,
        "the question DM is marked as such: {question:?}"
    );
    call(
        &marta,
        "post_message",
        json!({"to": "joaquin", "body": "yes, since yesterday", "reply_to": question["id"]}),
    )
    .await;

    let (result, elapsed) = asker.await.unwrap();
    assert_eq!(result["answered"], true, "{result:?}");
    assert_eq!(result["answer"]["from"], "marta");
    assert_eq!(result["answer"]["body"], "yes, since yesterday");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "answered by event, not by timeout (took {elapsed:?})"
    );

    // Timeout path: no answer in time, then resume picks up a late answer
    // that was sent without reply_to (lenient matching).
    let timed = call(
        &joaquin,
        "ask_agent",
        json!({"to": "marta", "question": "and prod?", "timeout_seconds": 5}),
    )
    .await;
    assert_eq!(timed["answered"], false, "{timed:?}");
    let qid = timed["question_message_id"].as_i64().unwrap();
    assert!(
        timed["suggestion"]
            .as_str()
            .unwrap()
            .contains(&qid.to_string()),
        "timeout suggestion tells how to resume: {timed:?}"
    );

    call(
        &marta,
        "post_message",
        json!({"to": "joaquin", "body": "prod is still on pg15"}),
    )
    .await;
    let resumed = call(
        &joaquin,
        "ask_agent",
        json!({"to": "marta", "resume_message_id": qid, "timeout_seconds": 5}),
    )
    .await;
    assert_eq!(resumed["answered"], true, "{resumed:?}");
    assert_eq!(resumed["answer"]["body"], "prod is still on pg15");

    // Asking yourself is refused.
    let err = call_expect_error(
        &joaquin,
        "ask_agent",
        json!({"to": "joaquin", "question": "hi"}),
    )
    .await;
    assert!(err.contains("yourself"), "{err}");

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn blocked_tasks_wait_for_their_dependencies() {
    let h = require_db!("t_deps");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(
        &joaquin,
        "create_task",
        json!({"key": "migrate-schema", "title": "migrate the users schema"}),
    )
    .await;
    let dependent = call(
        &joaquin,
        "create_task",
        json!({
            "key": "update-clients",
            "title": "update the API clients",
            "depends_on": ["migrate-schema"]
        }),
    )
    .await;
    assert_eq!(dependent["blocked"], true);
    assert_eq!(dependent["depends_on"][0], "migrate-schema");

    // A dependency that does not exist is a clean error.
    let err = call_expect_error(
        &joaquin,
        "create_task",
        json!({"key": "x", "title": "x", "depends_on": ["nope"]}),
    )
    .await;
    assert!(err.contains("nope"), "got: {err}");

    // The blocked task cannot be claimed, with an explanatory reason.
    let denied = call(&marta, "claim_task", json!({"key": "update-clients"})).await;
    assert_eq!(denied["claimed"], false);
    assert!(
        denied["reason"]
            .as_str()
            .unwrap()
            .contains("migrate-schema"),
        "reason should name the blocker: {denied:?}"
    );

    // claim_next_task skips it and hands out the dependency instead.
    let next = call(&marta, "claim_next_task", json!({})).await;
    assert_eq!(next["claimed"], true);
    assert_eq!(next["task"]["key"], "migrate-schema");

    // Finishing the dependency unblocks the dependent task.
    call(&marta, "complete_task", json!({"key": "migrate-schema"})).await;
    let now_free = call(&joaquin, "claim_task", json!({"key": "update-clients"})).await;
    assert_eq!(
        now_free["claimed"], true,
        "unblocked after dep done: {now_free:?}"
    );
    assert_eq!(now_free["task"]["blocked"], false);

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn locks_are_exclusive_expiring_and_visible() {
    let h = require_db!("t_locks");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    let got = call(
        &joaquin,
        "acquire_lock",
        json!({"name": "Deploy:Staging", "ttl_seconds": 120, "purpose": "rolling out 1.4.2"}),
    )
    .await;
    assert_eq!(got["acquired"], true);
    assert_eq!(got["lock"]["name"], "deploy:staging", "name normalised");

    // Second acquirer is refused and told who holds it.
    let denied = call(&marta, "acquire_lock", json!({"name": "deploy:staging"})).await;
    assert_eq!(denied["acquired"], false);
    assert!(denied["reason"].as_str().unwrap().contains("joaquin"));

    // Re-acquiring your own lock extends it, not an error.
    let extended = call(
        &joaquin,
        "acquire_lock",
        json!({"name": "deploy:staging", "ttl_seconds": 600}),
    )
    .await;
    assert_eq!(extended["acquired"], true);

    // Visible to the whole team.
    let listed = call(&marta, "list_locks", json!({})).await;
    assert_eq!(listed["locks"][0]["holder"], "joaquin");
    assert_eq!(listed["locks"][0]["purpose"], "rolling out 1.4.2");

    // You cannot release someone else's lock.
    let err = call_expect_error(&marta, "release_lock", json!({"name": "deploy:staging"})).await;
    assert!(err.contains("joaquin"), "got: {err}");

    // Release frees it for the next agent.
    call(&joaquin, "release_lock", json!({"name": "deploy:staging"})).await;
    let now = call(&marta, "acquire_lock", json!({"name": "deploy:staging"})).await;
    assert_eq!(now["acquired"], true);

    // Expired locks are silently taken over.
    sqlx::query("UPDATE locks SET expires_at = now() - interval '1 second'")
        .execute(&h.pool)
        .await
        .unwrap();
    let stolen = call(&joaquin, "acquire_lock", json!({"name": "deploy:staging"})).await;
    assert_eq!(stolen["acquired"], true, "expired lock must be stealable");

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn team_digest_summarises_recent_activity() {
    let h = require_db!("t_digest");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;
    let marta = connect(&h.base, &b).await;

    call(&joaquin, "create_channel", json!({"name": "deploys"})).await;
    call(
        &joaquin,
        "post_message",
        json!({"channel": "deploys", "body": "staging lleva la 1.4.2"}),
    )
    .await;
    call(
        &marta,
        "create_task",
        json!({"key": "hotfix", "title": "hotfix the parser"}),
    )
    .await;
    call(&marta, "claim_task", json!({"key": "hotfix"})).await;
    call(
        &marta,
        "complete_task",
        json!({"key": "hotfix", "result": "merged in #99"}),
    )
    .await;
    call(
        &joaquin,
        "set_note",
        json!({"scope": "api", "key": "deploy-runbook", "value": "step 1..."}),
    )
    .await;
    call(&marta, "heartbeat", json!({"activity": "reviewing PRs"})).await;
    // A DM that must NOT leak into the digest.
    call(
        &joaquin,
        "post_message",
        json!({"to": "marta", "body": "esto es privado"}),
    )
    .await;

    let digest = call(&joaquin, "team_digest", json!({"hours": 24})).await;
    assert_eq!(digest["channels"][0]["name"], "deploys");
    assert_eq!(digest["channels"][0]["message_count"], 1);
    let tasks: Vec<&str> = digest["tasks_moved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["key"].as_str().unwrap())
        .collect();
    assert!(tasks.contains(&"hotfix"));
    assert_eq!(digest["notes_updated"][0]["key"], "deploy-runbook");
    assert!(
        digest["agents_seen"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "marta" && a["online"] == true)
    );
    let serialized = serde_json::to_string(&digest).unwrap();
    assert!(
        !serialized.contains("privado"),
        "digest must never contain direct messages"
    );

    let _ = joaquin.cancel().await;
    let _ = marta.cancel().await;
}

#[tokio::test]
async fn webhooks_forward_channel_messages_but_never_dms() {
    let h = require_db!("t_webhooks");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let b = seed_agent(&h.pool, "acme", "marta").await;
    let joaquin = connect(&h.base, &a).await;

    // A local catcher standing in for Slack.
    let received: std::sync::Arc<tokio::sync::Mutex<Vec<Value>>> = Default::default();
    let catcher = {
        let received = received.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
                let received = received.clone();
                async move {
                    received.lock().await.push(v);
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    };

    ai_crewmate::webhooks::webhook_add(
        &h.pool,
        "acme",
        &format!("http://{catcher}/hook"),
        "slack",
        "message,task",
        None,
    )
    .await
    .unwrap();

    call(&joaquin, "create_channel", json!({"name": "deploys"})).await;
    call(
        &joaquin,
        "post_message",
        json!({"channel": "deploys", "body": "canary verde"}),
    )
    .await;
    call(
        &joaquin,
        "post_message",
        json!({"to": "marta", "body": "secreto entre nosotros"}),
    )
    .await;
    call(
        &joaquin,
        "create_task",
        json!({"key": "rotate", "title": "rotate keys"}),
    )
    .await;
    let _ = b; // marta only needs to exist as a DM target

    // Give LISTEN/NOTIFY + dispatch a moment.
    let mut tries = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let got = received.lock().await;
        if got.len() >= 2 || tries > 20 {
            break;
        }
        drop(got);
        tries += 1;
    }

    let got = received.lock().await;
    let texts: Vec<String> = got
        .iter()
        .map(|v| v["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("#deploys") && t.contains("canary verde")),
        "channel message must be forwarded in Slack format: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("rotate")),
        "task event must be forwarded: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.contains("secreto")),
        "a DM must NEVER reach a webhook: {texts:?}"
    );

    let _ = joaquin.cancel().await;
}

#[tokio::test]
async fn dashboard_requires_a_token_and_renders_team_state() {
    let h = require_db!("t_dash");
    let a = seed_agent(&h.pool, "acme", "joaquin").await;
    let joaquin = connect(&h.base, &a).await;
    call(&joaquin, "heartbeat", json!({"activity": "smoke testing"})).await;
    call(&joaquin, "create_channel", json!({"name": "dev"})).await;
    call(
        &joaquin,
        "post_message",
        json!({"channel": "dev", "body": "<script>alert(1)</script> hola"}),
    )
    .await;

    let http = reqwest::Client::new();
    let base = &h.base;

    let resp = http.get(format!("{base}/dashboard")).send().await.unwrap();
    assert_eq!(resp.status(), 401, "no token → 401");

    let resp = http
        .get(format!("{base}/dashboard?token=acm_bogus"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "bad token → 401");

    let resp = http
        .get(format!("{base}/dashboard?token={a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("joaquin"), "shows the agent");
    assert!(body.contains("smoke testing"), "shows the activity");
    assert!(
        body.contains("#dev") || body.contains("dev"),
        "shows the channel"
    );
    assert!(
        !body.contains("<script>alert(1)</script>"),
        "message bodies must be HTML-escaped"
    );
    assert!(body.contains("&lt;script&gt;"), "escaped form present");

    let _ = joaquin.cancel().await;
}
