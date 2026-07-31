//! Outgoing webhooks: forward team-visible bus events to Slack, Discord or a
//! generic JSON endpoint, so humans see what the agents are doing without
//! opening anything.
//!
//! Privacy rule: direct messages are NEVER forwarded, only channel messages
//! and team-wide events (tasks, locks, notes).

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::events::{BusEvent, EventHub};

#[derive(sqlx::FromRow)]
struct WebhookRow {
    url: String,
    kind: String,
    channel_filter: Option<String>,
}

/// Human-readable line for an event, resolving ids to names. Returns `None`
/// for events that should not be forwarded (DMs, unknown kinds).
async fn render_event(pool: &PgPool, event: &BusEvent) -> Option<(String, serde_json::Value, Option<String>)> {
    match event.kind() {
        "message" => {
            if event.is_direct_message() {
                return None; // never forward DMs
            }
            let id = event.message_id()?;
            let row: (String, String, String) = sqlx::query_as(
                r#"
                SELECT s.name, ch.name, left(m.body, 500)
                FROM messages m
                JOIN agents s ON s.id = m.sender_agent_id
                JOIN channels ch ON ch.id = m.channel_id
                WHERE m.id = $1
                "#,
            )
            .bind(id)
            .fetch_optional(pool)
            .await
            .ok()??;
            let (sender, channel, body) = row;
            let text = format!("💬 #{channel} · {sender}: {body}");
            let raw = serde_json::json!({
                "kind": "message", "channel": channel, "from": sender, "body": body,
            });
            Some((text, raw, Some(channel)))
        }
        "task" => {
            let key = event.0.get("key").and_then(|v| v.as_str())?;
            let status = event.0.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let holder = match event.0.get("claimed_by").and_then(|v| v.as_str()) {
                Some(uuid) => sqlx::query_scalar::<_, String>(
                    "SELECT name FROM agents WHERE id = $1::uuid",
                )
                .bind(uuid)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten(),
                None => None,
            };
            let icon = match status {
                "done" => "✅",
                "claimed" => "🔧",
                "cancelled" => "🚫",
                _ => "🗒️",
            };
            let who = holder
                .as_ref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            let text = format!("{icon} task `{key}` → {status}{who}");
            let raw = serde_json::json!({
                "kind": "task", "key": key, "status": status, "claimed_by": holder,
            });
            Some((text, raw, None))
        }
        "lock" => {
            let name = event.0.get("name").and_then(|v| v.as_str())?;
            let what = event.0.get("event").and_then(|v| v.as_str()).unwrap_or("changed");
            let holder = match event.0.get("holder_agent_id").and_then(|v| v.as_str()) {
                Some(uuid) => sqlx::query_scalar::<_, String>(
                    "SELECT name FROM agents WHERE id = $1::uuid",
                )
                .bind(uuid)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten(),
                None => None,
            };
            let who = holder.map(|h| format!(" by {h}")).unwrap_or_default();
            let text = format!("🔒 lock `{name}` {what}{who}");
            let raw = serde_json::json!({ "kind": "lock", "name": name, "event": what });
            Some((text, raw, None))
        }
        "note" => {
            let scope = event.0.get("scope").and_then(|v| v.as_str()).unwrap_or("global");
            let key = event.0.get("key").and_then(|v| v.as_str())?;
            let by = match event.0.get("updated_by").and_then(|v| v.as_str()) {
                Some(uuid) => sqlx::query_scalar::<_, String>(
                    "SELECT name FROM agents WHERE id = $1::uuid",
                )
                .bind(uuid)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten(),
                None => None,
            };
            let who = by.map(|h| format!(" by {h}")).unwrap_or_default();
            let text = format!("📝 note `{scope}/{key}` updated{who}");
            let raw = serde_json::json!({ "kind": "note", "scope": scope, "key": key });
            Some((text, raw, None))
        }
        _ => None,
    }
}

async fn dispatch(pool: &PgPool, http: &reqwest::Client, event: &BusEvent) {
    let Some(team_id) = event.team_id() else { return };
    let kind = event.kind().to_owned();

    let hooks: Vec<WebhookRow> = match sqlx::query_as(
        r#"
        SELECT url, kind, channel_filter
        FROM webhooks
        WHERE team_id = $1 AND enabled AND $2 = ANY(events)
        "#,
    )
    .bind(team_id)
    .bind(&kind)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "webhook lookup failed");
            return;
        }
    };
    if hooks.is_empty() {
        return;
    }

    let Some((text, raw, channel)) = render_event(pool, event).await else {
        return;
    };

    for hook in hooks {
        // Channel filter only constrains message events.
        if kind == "message" {
            if let (Some(filter), Some(chan)) = (&hook.channel_filter, &channel) {
                if filter != chan {
                    continue;
                }
            }
        }
        let payload = match hook.kind.as_str() {
            "slack" => serde_json::json!({ "text": text }),
            "discord" => serde_json::json!({ "content": text }),
            _ => raw.clone(),
        };
        let url = hook.url.clone();
        match http.post(&url).json(&payload).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(url = %url, status = %resp.status(), "webhook rejected");
            }
            Err(e) => tracing::warn!(url = %url, error = %e, "webhook delivery failed"),
            _ => {}
        }
    }
}

/// Consume the event hub and forward matching events until cancelled.
pub async fn run_dispatcher(pool: PgPool, hub: EventHub, ct: CancellationToken) {
    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "webhook dispatcher could not build HTTP client");
            return;
        }
    };
    let mut rx = hub.subscribe();
    loop {
        tokio::select! {
            _ = ct.cancelled() => return,
            recv = rx.recv() => match recv {
                Ok(event) => dispatch(&pool, &http, &event).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "webhook dispatcher lagged; some events not forwarded");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
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

    println!("webhook {id} registered ({kind}, events: {})", events.join(","));
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
