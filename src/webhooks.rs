//! Outgoing webhooks: forward team-visible bus events to Slack, Discord or a
//! generic JSON endpoint, so humans see what the agents are doing without
//! opening anything.
//!
//! Privacy rule: direct messages are NEVER forwarded, only channel messages
//! and team-wide events (tasks, locks, notes). That guard lives in the
//! database trigger that enqueues deliveries — the only place one can be
//! created — rather than in each consumer.
//!
//! Delivery is an outbox, not a fan-out from the event stream. A trigger
//! enqueues one row per (event, matching webhook) when the change commits,
//! which happens once however many replicas are running; each replica then
//! claims rows with `FOR UPDATE SKIP LOCKED`, the same primitive task claims
//! use. The guarantee is **at-least-once** with bounded retries: a receiver
//! that 500s or times out is retried with backoff, and a receiver that keeps
//! failing is parked as `failed` for an operator to see, never silently
//! dropped.

use std::time::Duration;

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::EventHub;

/// Attempts before a delivery is parked as permanently failed.
const MAX_ATTEMPTS: i32 = 6;
/// How many deliveries one replica claims per round.
const BATCH: i64 = 32;
/// In-flight POSTs per replica. Bounded so one slow receiver cannot starve
/// the others, and so a burst cannot open unbounded sockets.
const CONCURRENCY: usize = 8;
/// Backstop poll when no notification arrives — also what makes retries due.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// How often retention runs, measured in elapsed time rather than in idle
/// rounds — a busy dispatcher has more to prune, not less.
const PRUNE_INTERVAL: Duration = Duration::from_secs(600);
/// Successful deliveries are evidence for a while, then noise.
const KEEP_SENT: Duration = Duration::from_secs(24 * 3600);
/// Failures stay long enough for someone to notice and investigate.
const KEEP_FAILED: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(sqlx::FromRow)]
struct Delivery {
    id: i64,
    url: String,
    kind: String,
    summary: String,
    payload: serde_json::Value,
    attempts: i32,
}

/// Claim a batch of due deliveries for this replica.
///
/// `FOR UPDATE SKIP LOCKED` is what makes N replicas safe: two dispatchers
/// racing for the same row means one of them takes it and the other moves on,
/// rather than both sending it.
async fn claim(pool: &PgPool) -> Result<Vec<Delivery>, sqlx::Error> {
    sqlx::query_as(
        r#"
        WITH due AS (
            SELECT d.id
            FROM webhook_deliveries d
            WHERE d.status = 'pending' AND d.next_attempt_at <= now()
            ORDER BY d.next_attempt_at, d.id
            FOR UPDATE SKIP LOCKED
            LIMIT $1
        )
        UPDATE webhook_deliveries d
        SET attempts = d.attempts + 1,
            -- Hold the row for the duration of the attempt: if this replica
            -- dies mid-POST, the row becomes due again instead of vanishing.
            next_attempt_at = now() + interval '60 seconds',
            updated_at = now()
        FROM due, webhooks w
        WHERE d.id = due.id AND w.id = d.webhook_id
        RETURNING d.id, w.url, w.kind, d.summary, d.payload, d.attempts
        "#,
    )
    .bind(BATCH)
    .fetch_all(pool)
    .await
}

/// Slack and Discord want their own envelope; everything else gets the event.
fn body_for(kind: &str, summary: &str, payload: &serde_json::Value) -> serde_json::Value {
    match kind {
        "slack" => serde_json::json!({ "text": summary }),
        "discord" => serde_json::json!({ "content": summary }),
        _ => payload.clone(),
    }
}

async fn mark_sent(pool: &PgPool, id: i64) {
    // Losing this UPDATE means a delivered webhook is sent again later. That
    // is survivable — the guarantee is at-least-once — but an operator
    // debugging duplicates needs to see it.
    if let Err(e) = sqlx::query(
        "UPDATE webhook_deliveries SET status = 'sent', last_error = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    {
        tracing::warn!(delivery = id, error = %e, "could not record a successful delivery");
    }
}

/// Record a failed attempt: schedule a retry with exponential backoff, or park
/// the delivery once it has used its attempts.
async fn mark_failed(pool: &PgPool, id: i64, attempts: i32, error: &str) {
    // 2s, 4s, 8s … capped, so a receiver that is down for a minute is retried
    // a handful of times rather than hammered.
    let backoff = 2i64.saturating_pow(attempts.clamp(1, 8) as u32).min(600);
    let terminal = attempts >= MAX_ATTEMPTS;
    if terminal {
        tracing::warn!(
            delivery = id,
            attempts,
            error,
            "webhook delivery permanently failed"
        );
    }
    let recorded = sqlx::query(
        r#"
        UPDATE webhook_deliveries
        SET status = CASE WHEN $2 THEN 'failed' ELSE 'pending' END,
            next_attempt_at = now() + make_interval(secs => $3),
            last_error = left($4, 500),
            updated_at = now()
        WHERE id = $1
        "#,
    )
    .bind(id)
    .bind(terminal)
    .bind(backoff as f64)
    .bind(error)
    .execute(pool)
    .await;
    if let Err(e) = recorded {
        // The row keeps the 60s hold from claim() and becomes due again, so
        // nothing is lost — but without this line the retry schedule looks
        // inexplicable.
        tracing::warn!(delivery = id, error = %e, "could not record a failed delivery");
    }
}

/// Send one batch, bounded to `CONCURRENCY` in-flight requests.
async fn send_batch(pool: &PgPool, http: &reqwest::Client, batch: Vec<Delivery>) {
    let mut queue = batch.into_iter();
    let mut running = tokio::task::JoinSet::new();

    loop {
        while running.len() < CONCURRENCY {
            let Some(d) = queue.next() else { break };
            let http = http.clone();
            let pool = pool.clone();
            running.spawn(async move {
                let body = body_for(&d.kind, &d.summary, &d.payload);
                match http.post(&d.url).json(&body).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        // Drain before dropping: hyper only reuses a
                        // connection whose body was consumed, and a webhook
                        // dispatcher talks to the same few hosts all day.
                        let _ = resp.bytes().await;
                        mark_sent(&pool, d.id).await;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        // 4xx other than 408/429 will not improve on retry;
                        // spend the attempts on things that might.
                        let permanent = status.is_client_error()
                            && status != reqwest::StatusCode::REQUEST_TIMEOUT
                            && status != reqwest::StatusCode::TOO_MANY_REQUESTS;
                        let attempts = if permanent { MAX_ATTEMPTS } else { d.attempts };
                        let _ = resp.bytes().await;
                        mark_failed(&pool, d.id, attempts, &format!("HTTP {status}")).await;
                    }
                    Err(e) => mark_failed(&pool, d.id, d.attempts, &e.to_string()).await,
                }
            });
        }
        match running.join_next().await {
            None => break,
            Some(Ok(())) => {}
            // A delivery task should never panic; if one does, the row stays
            // pending and retries forever with no clue why. Say so.
            Some(Err(e)) => tracing::error!(error = %e, "webhook delivery task failed"),
        }
    }
}

/// Drop deliveries nobody will look at again.
async fn prune(pool: &PgPool) {
    let _ = sqlx::query(
        r#"
        DELETE FROM webhook_deliveries
        WHERE (status = 'sent'   AND updated_at < now() - make_interval(secs => $1))
           OR (status = 'failed' AND updated_at < now() - make_interval(secs => $2))
        "#,
    )
    .bind(KEEP_SENT.as_secs() as f64)
    .bind(KEEP_FAILED.as_secs() as f64)
    .execute(pool)
    .await;
}

/// Claim, send and retire deliveries until cancelled.
///
/// Woken by the in-process event hub (any bus event may have enqueued work)
/// and by the poll interval, which is also what makes a backed-off retry due.
/// A missed notification therefore costs latency, never a delivery.
pub async fn run_dispatcher(pool: PgPool, hub: EventHub, ct: CancellationToken) {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "webhook dispatcher could not build HTTP client");
            return;
        }
    };
    let mut rx = hub.subscribe();

    let mut last_prune = tokio::time::Instant::now();

    loop {
        // Retention runs on elapsed time, not on idle rounds. Counting rounds
        // meant a dispatcher that always found work never pruned at all —
        // exactly the deployment where the table grows fastest.
        if last_prune.elapsed() >= PRUNE_INTERVAL {
            last_prune = tokio::time::Instant::now();
            prune(&pool).await;
        }

        match claim(&pool).await {
            Ok(batch) if !batch.is_empty() => {
                send_batch(&pool, &http, batch).await;
                continue; // there may be more due right now
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "claiming webhook deliveries failed"),
        }

        // Idle: wait for an event or the poll interval, whichever comes first.
        tokio::select! {
            _ = ct.cancelled() => return,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            recv = rx.recv() => {
                if matches!(recv, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    return;
                }
                // Lagging is harmless now: the work is in the outbox, and the
                // next claim finds it regardless of what the stream missed.
            }
        }
    }
}

// -------------------------------------------------------------- admin CLI --

pub async fn webhook_add(
    pool: &PgPool,
    team: &str,
    url: &str,
    kind: &str,
    events: &str,
    channel: Option<String>,
) -> anyhow::Result<()> {
    let kind = kind.trim().to_lowercase();
    if !["slack", "discord", "generic"].contains(&kind.as_str()) {
        anyhow::bail!("kind must be slack, discord or generic");
    }
    let events: Vec<String> = events
        .split(',')
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    for e in &events {
        if !["message", "task", "lock", "note"].contains(&e.as_str()) {
            anyhow::bail!("unknown event kind '{e}' (valid: message, task, lock, note)");
        }
    }
    if events.is_empty() {
        anyhow::bail!("at least one event kind is required");
    }

    let team_id: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM teams WHERE slug = $1")
        .bind(team)
        .fetch_optional(pool)
        .await?;
    let Some((team_id,)) = team_id else {
        anyhow::bail!("no team with slug '{team}'");
    };

    let (id,): (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO webhooks (team_id, url, kind, events, channel_filter)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
    )
    .bind(team_id)
    .bind(url)
    .bind(&kind)
    .bind(&events)
    .bind(channel.as_deref().map(str::to_lowercase))
    .fetch_one(pool)
    .await?;

    println!(
        "webhook {id} registered ({kind}, events: {})",
        events.join(",")
    );
    Ok(())
}

pub async fn webhook_list(pool: &PgPool, team: &str) -> anyhow::Result<()> {
    let rows: Vec<(Uuid, String, String, Vec<String>, Option<String>, bool)> = sqlx::query_as(
        r#"
        SELECT w.id, w.url, w.kind, w.events, w.channel_filter, w.enabled
        FROM webhooks w JOIN teams t ON t.id = w.team_id
        WHERE t.slug = $1
        ORDER BY w.created_at
        "#,
    )
    .bind(team)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        println!("(no webhooks for team '{team}')");
    }
    for (id, url, kind, events, channel, enabled) in rows {
        let chan = channel.map(|c| format!(" #{c}")).unwrap_or_default();
        let state = if enabled { "" } else { " [disabled]" };
        println!("{id}  {kind:<8} {}{chan}{state}  {url}", events.join(","));
    }
    Ok(())
}

pub async fn webhook_remove(pool: &PgPool, id: Uuid) -> anyhow::Result<()> {
    let res = sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("no webhook with id {id}");
    }
    println!("webhook {id} removed");
    Ok(())
}
