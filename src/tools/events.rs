use std::time::Duration;

use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::{Bus, auth_of};
use crate::{
    auth::AuthCtx,
    events::BusEvent,
    model::{WaitEvent, WaitResult},
};

const DEFAULT_TIMEOUT_SECS: i64 = 25;
const MAX_TIMEOUT_SECS: i64 = 55;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitArgs {
    /// How long to wait before giving up, in seconds (5-55, default 25).
    /// Kept under a minute so HTTP intermediaries do not cut the call.
    #[serde(default)]
    pub timeout_seconds: Option<i64>,
    /// Restrict to certain event kinds: any of "message", "task", "lock",
    /// "note". Omit to wake on anything relevant to you.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

async fn unread_dms(pool: &PgPool, agent_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM messages m
        LEFT JOIN read_cursors c ON c.agent_id = $1 AND c.scope = 'inbox'
        WHERE m.recipient_agent_id = $1
          AND m.id > COALESCE(c.last_message_id, 0)
        "#,
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await
}

async fn unread_anything(pool: &PgPool, auth: &AuthCtx) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM messages m
        LEFT JOIN read_cursors c ON c.agent_id = $1 AND c.scope = 'all'
        WHERE m.team_id = $2
          AND m.id > COALESCE(c.last_message_id, 0)
          AND m.sender_agent_id <> $1
          AND (m.channel_id IS NOT NULL OR m.recipient_agent_id = $1)
        "#,
    )
    .bind(auth.agent_id)
    .bind(auth.team_id)
    .fetch_one(pool)
    .await
}

/// Turn a raw bus event into a one-line summary, resolving ids to names.
async fn describe(pool: &PgPool, event: &BusEvent) -> Option<WaitEvent> {
    match event.kind() {
        "message" => {
            let id = event.message_id()?;
            let row: (String, Option<String>, Option<String>, String) =
                sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(
                    r#"
                    SELECT s.name, ch.name, r.name, left(m.body, 200)
                    FROM messages m
                    JOIN agents s ON s.id = m.sender_agent_id
                    LEFT JOIN channels ch ON ch.id = m.channel_id
                    LEFT JOIN agents r ON r.id = m.recipient_agent_id
                    WHERE m.id = $1
                    "#,
                )
                .bind(id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()?;
            let (sender, channel, recipient, body) = row;
            let target = channel
                .map(|c| format!("#{c}"))
                .or(recipient.map(|r| format!("@{r}")))
                .unwrap_or_default();
            Some(WaitEvent {
                kind: "message".into(),
                summary: format!("{sender} → {target}: {body}"),
            })
        }
        "task" => {
            let key = event.0.get("key").and_then(|v| v.as_str())?;
            let status = event
                .0
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let holder = match event.0.get("claimed_by").and_then(|v| v.as_str()) {
                Some(uuid) => {
                    sqlx::query_scalar::<_, String>("SELECT name FROM agents WHERE id = $1::uuid")
                        .bind(uuid)
                        .fetch_optional(pool)
                        .await
                        .ok()
                        .flatten()
                        .map(|n| format!(" by {n}"))
                        .unwrap_or_default()
                }
                None => String::new(),
            };
            Some(WaitEvent {
                kind: "task".into(),
                summary: format!("task '{key}' is now {status}{holder}"),
            })
        }
        "lock" => {
            let name = event.0.get("name").and_then(|v| v.as_str())?;
            let what = event
                .0
                .get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("changed");
            Some(WaitEvent {
                kind: "lock".into(),
                summary: format!("lock '{name}' {what}"),
            })
        }
        "note" => {
            let scope = event
                .0
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("global");
            let key = event.0.get("key").and_then(|v| v.as_str())?;
            Some(WaitEvent {
                kind: "note".into(),
                summary: format!("note {scope}/{key} was updated"),
            })
        }
        _ => None,
    }
}

fn suggestion_for(events: &[WaitEvent], unread: i64) -> String {
    if events.iter().any(|e| e.kind == "message") || unread > 0 {
        "Call read_messages to fetch the new messages.".into()
    } else if events.iter().any(|e| e.kind == "task") {
        "Call list_tasks (or get_task) to see what changed.".into()
    } else if events.iter().any(|e| e.kind == "lock") {
        "Call list_locks (or retry acquire_lock) now.".into()
    } else if events.iter().any(|e| e.kind == "note") {
        "Call get_note to read the updated note.".into()
    } else {
        "Nothing happened; do other work or wait again.".into()
    }
}

#[tool_router(router = events_router, vis = "pub")]
impl Bus {
    #[tool(
        description = "Block until something happens on the bus that concerns you (a message \
                       arrives, a task changes state, a lock is released, a note is updated) or \
                       the timeout elapses. Use this instead of polling read_messages in a loop: \
                       call it when you are waiting on teammates and idle. Returns immediately \
                       if you already have unread messages."
    )]
    async fn wait_for_updates(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<Json<WaitResult>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let timeout = args
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(5, MAX_TIMEOUT_SECS);
        let kind_filter: Option<Vec<String>> = args
            .kinds
            .map(|ks| ks.into_iter().map(|k| k.trim().to_lowercase()).collect());
        let wants = |kind: &str| {
            kind_filter
                .as_ref()
                .map(|ks| ks.iter().any(|k| k == kind))
                .unwrap_or(true)
        };

        // Subscribe before checking the database so nothing slips between the
        // check and the wait.
        let mut rx = self.hub.subscribe();

        let pending = unread_anything(&self.db, &auth).await.map_err(|e| {
            tracing::error!(error = %e, "unread check failed");
            ErrorData::internal_error("database error", None)
        })?;
        if pending > 0 && wants("message") {
            let dms = unread_dms(&self.db, auth.agent_id).await.unwrap_or(0);
            return Ok(Json(WaitResult {
                woke: true,
                timed_out: false,
                events: vec![WaitEvent {
                    kind: "message".into(),
                    summary: format!("{pending} unread message(s) already waiting"),
                }],
                unread_direct_messages: dms,
                suggestion: "Call read_messages to fetch them.".into(),
            }));
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
        let mut events: Vec<WaitEvent> = Vec::new();

        while events.is_empty() {
            let event = tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                recv = rx.recv() => match recv {
                    Ok(ev) => ev,
                    // Lagged: we missed events; report a generic wake so the
                    // caller re-syncs from the database.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        events.push(WaitEvent {
                            kind: "unknown".into(),
                            summary: "event stream lagged; re-check the bus".into(),
                        });
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };

            if !event.visible_to(auth.team_id, auth.agent_id) || !wants(event.kind()) {
                continue;
            }
            // Your own messages are not news to you.
            if event.kind() == "message" && event.sender_agent_id() == Some(auth.agent_id) {
                continue;
            }
            if let Some(described) = describe(&self.db, &event).await {
                events.push(described);
                // Grace window: batch events that arrive together.
                let grace = tokio::time::Instant::now() + Duration::from_millis(150);
                while let Ok(Ok(more)) = tokio::time::timeout_at(grace, rx.recv()).await {
                    if more.visible_to(auth.team_id, auth.agent_id)
                        && wants(more.kind())
                        && !(more.kind() == "message"
                            && more.sender_agent_id() == Some(auth.agent_id))
                        && let Some(d) = describe(&self.db, &more).await
                    {
                        events.push(d);
                    }
                }
            }
        }

        let unread = unread_dms(&self.db, auth.agent_id).await.unwrap_or(0);
        let woke = !events.is_empty();
        Ok(Json(WaitResult {
            woke,
            timed_out: !woke,
            suggestion: suggestion_for(&events, unread),
            events,
            unread_direct_messages: unread,
        }))
    }
}
