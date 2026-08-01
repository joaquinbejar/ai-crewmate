//! Read-only HTML dashboard for humans: who is online, what is claimed, what
//! the channels are saying. Served by the bus itself at `/dashboard`.
//!
//! Auth: any agent token of the team, passed as `?token=acs_...` or as an
//! `Authorization: Bearer` header. Query-param tokens can end up in proxy
//! logs — acceptable for an internal tool, documented in the README.
//!
//! Design notes (from the dataviz method): this page is stat tiles + tables,
//! not charts, so no categorical palette is involved. Status colors are the
//! reserved status palette and never appear without an icon + text label;
//! values wear text tokens, never status hues. Light and dark surfaces are the
//! validated reference pair.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use sqlx::PgPool;

use crate::auth::{AuthCtx, resolve_token};

#[derive(Deserialize)]
pub struct DashboardQuery {
    token: Option<String>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn ago(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

pub async fn render(
    State(pool): State<PgPool>,
    Query(q): Query<DashboardQuery>,
    headers: HeaderMap,
) -> Response {
    // Token from query param or bearer header.
    let raw = q.token.or_else(|| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(|s| s.trim().to_owned())
    });
    let Some(raw) = raw else {
        return (
            StatusCode::UNAUTHORIZED,
            Html("<h1>401</h1><p>Pass your agent token: <code>/dashboard?token=acs_…</code></p>"),
        )
            .into_response();
    };
    let auth = match resolve_token(&pool, &raw).await {
        Ok(a) => a,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Html("<h1>401</h1><p>invalid token</p>"),
            )
                .into_response();
        }
    };

    match build_page(&pool, &auth).await {
        Ok(page) => Html(page).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "dashboard query failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Html("<h1>500</h1>")).into_response()
        }
    }
}

async fn build_page(pool: &PgPool, auth: &AuthCtx) -> Result<String, sqlx::Error> {
    // Stat-tile numbers.
    let (agents_online,): (i64,) = sqlx::query_as(
        r#"SELECT count(*) FROM agents a JOIN agent_presence p ON p.agent_id = a.id
           WHERE a.team_id = $1 AND p.expires_at > now()"#,
    )
    .bind(auth.team_id)
    .fetch_one(pool)
    .await?;
    let (open_tasks, claimed_tasks): (i64, i64) = sqlx::query_as(
        r#"SELECT count(*) FILTER (WHERE status='open'),
                  count(*) FILTER (WHERE status='claimed')
           FROM tasks WHERE team_id = $1"#,
    )
    .bind(auth.team_id)
    .fetch_one(pool)
    .await?;
    let (messages_24h,): (i64,) = sqlx::query_as(
        r#"SELECT count(*) FROM messages
           WHERE team_id = $1 AND channel_id IS NOT NULL
             AND created_at > now() - interval '24 hours'"#,
    )
    .bind(auth.team_id)
    .fetch_one(pool)
    .await?;

    // Presence table.
    let agents: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    )> = sqlx::query_as(
        r#"SELECT a.name, p.status, p.repo, p.branch, p.activity, p.updated_at,
                      COALESCE(p.expires_at > now(), false)
               FROM agents a LEFT JOIN agent_presence p ON p.agent_id = a.id
               WHERE a.team_id = $1 AND a.disabled_at IS NULL
               ORDER BY COALESCE(p.expires_at > now(), false) DESC, a.name"#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    // Tasks: claimed and open first, then latest done.
    let tasks: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        bool,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"SELECT t.key, t.title, t.status, cb.name, t.result,
                      EXISTS (SELECT 1 FROM task_deps td
                              JOIN tasks d ON d.id = td.blocked_by_task_id
                              WHERE td.task_id = t.id
                                AND d.status NOT IN ('done','cancelled')) AS blocked,
                      t.updated_at
               FROM tasks t LEFT JOIN agents cb ON cb.id = t.claimed_by
               WHERE t.team_id = $1
               ORDER BY CASE t.status WHEN 'claimed' THEN 0 WHEN 'open' THEN 1 ELSE 2 END,
                        t.updated_at DESC
               LIMIT 30"#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    // Channel tail (team-visible only; DMs never appear here).
    let messages: Vec<(i64, String, String, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            r#"SELECT m.id, ch.name, s.name, left(m.body, 240), m.created_at
           FROM messages m
           JOIN channels ch ON ch.id = m.channel_id
           JOIN agents s ON s.id = m.sender_agent_id
           WHERE m.team_id = $1
           ORDER BY m.id DESC LIMIT 20"#,
        )
        .bind(auth.team_id)
        .fetch_all(pool)
        .await?;

    let locks: Vec<(
        String,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"SELECT l.name, a.name, l.purpose, l.expires_at
           FROM locks l JOIN agents a ON a.id = l.holder_agent_id
           WHERE l.team_id = $1 AND l.expires_at > now() ORDER BY l.name"#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    let notes: Vec<(
        String,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"SELECT n.scope, n.key, a.name, n.updated_at
           FROM notes n LEFT JOIN agents a ON a.id = n.updated_by
           WHERE n.team_id = $1 ORDER BY n.updated_at DESC LIMIT 15"#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    // ---- render ----
    let mut agent_rows = String::new();
    for (name, status, repo, branch, activity, updated, online) in &agents {
        // Status: icon + label, never color alone.
        let (dot, label) = if *online {
            match status.as_deref() {
                Some("blocked") => (
                    "<span class=\"st st-serious\">●</span> ⛔ blocked",
                    "blocked",
                ),
                Some("busy") => ("<span class=\"st st-warning\">●</span> ⚙ busy", "busy"),
                Some("idle") => ("<span class=\"st st-muted\">●</span> ◌ idle", "idle"),
                _ => ("<span class=\"st st-good\">●</span> ✓ active", "active"),
            }
        } else {
            ("<span class=\"st st-muted\">●</span> ○ offline", "offline")
        };
        let _ = label;
        let place = match (repo, branch) {
            (Some(r), Some(b)) => format!("{}@{}", esc(r), esc(b)),
            (Some(r), None) => esc(r),
            _ => String::new(),
        };
        agent_rows.push_str(&format!(
            "<tr><td><strong>{}</strong></td><td>{}</td><td>{}</td><td>{}</td><td class=\"muted\">{}</td></tr>",
            esc(name),
            dot,
            place,
            esc(activity.as_deref().unwrap_or("")),
            updated.map(ago).unwrap_or_default(),
        ));
    }

    let mut task_rows = String::new();
    for (key, title, status, holder, result, blocked, updated) in &tasks {
        let badge = match status.as_str() {
            "done" => "<span class=\"st st-good\">●</span> ✓ done".to_string(),
            "claimed" => format!(
                "<span class=\"st st-warning\">●</span> ⚙ claimed by {}",
                esc(holder.as_deref().unwrap_or("?"))
            ),
            "cancelled" => "<span class=\"st st-muted\">●</span> ✕ cancelled".to_string(),
            _ if *blocked => "<span class=\"st st-serious\">●</span> ⛔ blocked".to_string(),
            _ => "<span class=\"st st-muted\">●</span> ◌ open".to_string(),
        };
        let detail = result.as_deref().unwrap_or("");
        task_rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td class=\"muted\">{}</td></tr>",
            esc(key),
            esc(title),
            badge,
            esc(detail),
            ago(*updated),
        ));
    }

    let mut msg_rows = String::new();
    for (_, channel, sender, body, at) in messages.iter().rev() {
        msg_rows.push_str(&format!(
            "<tr><td class=\"muted\">{}</td><td>#{}</td><td><strong>{}</strong></td><td>{}</td></tr>",
            ago(*at),
            esc(channel),
            esc(sender),
            esc(body),
        ));
    }

    let mut lock_rows = String::new();
    for (name, holder, purpose, expires) in &locks {
        lock_rows.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td class=\"muted\">expires {}</td></tr>",
            esc(name),
            esc(holder),
            esc(purpose.as_deref().unwrap_or("")),
            ago(*expires).replace(" ago", ""),
        ));
    }
    if locks.is_empty() {
        lock_rows.push_str("<tr><td colspan=4 class=\"muted\">no locks held</td></tr>");
    }

    let mut note_rows = String::new();
    for (scope, key, by, at) in &notes {
        note_rows.push_str(&format!(
            "<tr><td class=\"muted\">{}</td><td><code>{}/{}</code></td><td>{}</td></tr>",
            ago(*at),
            esc(scope),
            esc(key),
            esc(by.as_deref().unwrap_or("")),
        ));
    }

    Ok(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="15">
<title>ai-crew-sync · {team}</title>
<style>
:root {{
  --surface: #fcfcfb; --card: #ffffff; --border: #e4e3df;
  --ink: #0b0b0b; --ink-2: #52514e; --ink-3: #8a8984;
  --good: #0ca30c; --warning: #fab219; --serious: #ec835a; --critical: #d03b3b;
}}
@media (prefers-color-scheme: dark) {{
  :root {{
    --surface: #1a1a19; --card: #232322; --border: #3a3936;
    --ink: #ffffff; --ink-2: #c3c2b7; --ink-3: #8a8984;
  }}
}}
* {{ box-sizing: border-box; }}
body {{
  margin: 0; padding: 24px; background: var(--surface); color: var(--ink);
  font: 14px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif;
}}
h1 {{ font-size: 18px; margin: 0 0 4px; }}
h2 {{ font-size: 13px; margin: 28px 0 8px; color: var(--ink-2);
     text-transform: uppercase; letter-spacing: .06em; }}
.sub {{ color: var(--ink-3); margin-bottom: 20px; }}
.tiles {{ display: flex; gap: 12px; flex-wrap: wrap; }}
.tile {{
  background: var(--card); border: 1px solid var(--border); border-radius: 10px;
  padding: 14px 18px; min-width: 130px;
}}
.tile .n {{ font-size: 26px; font-weight: 650; }}
.tile .l {{ color: var(--ink-2); font-size: 12px; }}
table {{ width: 100%; border-collapse: collapse; background: var(--card);
        border: 1px solid var(--border); border-radius: 10px; overflow: hidden; }}
td {{ padding: 7px 12px; border-top: 1px solid var(--border); vertical-align: top; }}
tr:first-child td {{ border-top: none; }}
code {{ background: transparent; color: inherit; }}
.muted {{ color: var(--ink-3); white-space: nowrap; }}
.st {{ font-size: 10px; vertical-align: 1px; }}
.st-good {{ color: var(--good); }}
.st-warning {{ color: var(--warning); }}
.st-serious {{ color: var(--serious); }}
.st-muted {{ color: var(--ink-3); }}
</style>
</head>
<body>
<h1>ai-crew-sync · {team}</h1>
<div class="sub">viewed as {viewer} · refreshes every 15s · direct messages never shown</div>

<div class="tiles">
  <div class="tile"><div class="n">{agents_online}</div><div class="l">agents online</div></div>
  <div class="tile"><div class="n">{claimed_tasks}</div><div class="l">tasks in progress</div></div>
  <div class="tile"><div class="n">{open_tasks}</div><div class="l">tasks open</div></div>
  <div class="tile"><div class="n">{messages_24h}</div><div class="l">channel msgs · 24h</div></div>
</div>

<h2>Agents</h2>
<table>{agent_rows}</table>

<h2>Tasks</h2>
<table>{task_rows}</table>

<h2>Locks</h2>
<table>{lock_rows}</table>

<h2>Latest channel messages</h2>
<table>{msg_rows}</table>

<h2>Recently updated notes</h2>
<table>{note_rows}</table>
</body>
</html>"##,
        team = esc(&auth.team_slug),
        viewer = esc(&auth.agent_name),
    ))
}
