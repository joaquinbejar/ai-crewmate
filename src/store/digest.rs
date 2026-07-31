use sqlx::PgPool;

use crate::{
    auth::AuthCtx,
    error::BusResult,
    model::{
        DigestChannel, DigestMessage, DigestNote, DigestResult, DigestTask, LockInfo, ts, ts_opt,
    },
};

/// A team-wide summary of the last `hours` hours: what moved, who did it and
/// where things stand. Everything in it is team-visible (no direct messages).
pub async fn team_digest(pool: &PgPool, auth: &AuthCtx, hours: i64) -> BusResult<DigestResult> {
    let hours = hours.clamp(1, 24 * 14);

    // Channel activity: volume plus the tail of the conversation.
    let channel_rows: Vec<(String, i64)> = sqlx::query_as(
        r#"
        SELECT c.name, count(m.id)
        FROM channels c
        JOIN messages m ON m.channel_id = c.id
        WHERE c.team_id = $1 AND m.created_at > now() - make_interval(hours => $2)
        GROUP BY c.name
        ORDER BY count(m.id) DESC
        "#,
    )
    .bind(auth.team_id)
    .bind(hours as i32)
    .fetch_all(pool)
    .await?;

    let mut channels = Vec::with_capacity(channel_rows.len());
    for (name, message_count) in channel_rows {
        let tail: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            r#"
            SELECT a.name, left(m.body, 300), m.created_at
            FROM messages m
            JOIN channels c ON c.id = m.channel_id
            JOIN agents a ON a.id = m.sender_agent_id
            WHERE c.team_id = $1 AND c.name = $2
              AND m.created_at > now() - make_interval(hours => $3)
            ORDER BY m.id DESC
            LIMIT 5
            "#,
        )
        .bind(auth.team_id)
        .bind(&name)
        .bind(hours as i32)
        .fetch_all(pool)
        .await?;

        channels.push(DigestChannel {
            name,
            message_count,
            last_messages: tail
                .into_iter()
                .rev()
                .map(|(from, body, at)| DigestMessage {
                    from,
                    body,
                    at: ts(at),
                })
                .collect(),
        });
    }

    // Tasks that moved in the window.
    let task_rows: Vec<(
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"
        SELECT t.key, t.title, t.status, cb.name, t.result, t.updated_at
        FROM tasks t
        LEFT JOIN agents cb ON cb.id = t.claimed_by
        WHERE t.team_id = $1 AND t.updated_at > now() - make_interval(hours => $2)
        ORDER BY t.updated_at DESC
        LIMIT 50
        "#,
    )
    .bind(auth.team_id)
    .bind(hours as i32)
    .fetch_all(pool)
    .await?;

    let tasks_moved: Vec<DigestTask> = task_rows
        .into_iter()
        .map(
            |(key, title, status, claimed_by, result, updated_at)| DigestTask {
                key,
                title,
                status,
                claimed_by,
                result,
                updated_at: ts(updated_at),
            },
        )
        .collect();

    let (open_tasks, claimed_tasks): (i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*) FILTER (WHERE status = 'open'),
               count(*) FILTER (WHERE status = 'claimed')
        FROM tasks WHERE team_id = $1
        "#,
    )
    .bind(auth.team_id)
    .fetch_one(pool)
    .await?;

    // Notes touched in the window.
    let note_rows: Vec<(
        String,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"
            SELECT n.scope, n.key, a.name, n.updated_at
            FROM notes n
            LEFT JOIN agents a ON a.id = n.updated_by
            WHERE n.team_id = $1 AND n.updated_at > now() - make_interval(hours => $2)
            ORDER BY n.updated_at DESC
            LIMIT 50
            "#,
    )
    .bind(auth.team_id)
    .bind(hours as i32)
    .fetch_all(pool)
    .await?;

    let notes_updated: Vec<DigestNote> = note_rows
        .into_iter()
        .map(|(scope, key, updated_by, updated_at)| DigestNote {
            scope,
            key,
            updated_by,
            updated_at: ts(updated_at),
        })
        .collect();

    // Who was around.
    let agent_rows: Vec<(
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    )> = sqlx::query_as(
        r#"
            SELECT a.name, p.activity, p.updated_at, COALESCE(p.expires_at > now(), false)
            FROM agents a
            LEFT JOIN agent_presence p ON p.agent_id = a.id
            WHERE a.team_id = $1 AND a.disabled_at IS NULL
            ORDER BY p.updated_at DESC NULLS LAST
            "#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    let agents_seen = agent_rows
        .into_iter()
        .map(
            |(name, activity, last_seen, online)| crate::model::DigestAgent {
                name,
                activity,
                last_seen: ts_opt(last_seen),
                online,
            },
        )
        .collect();

    // Active locks right now.
    let lock_rows: Vec<(
        String,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"
            SELECT l.name, a.name, l.purpose, l.acquired_at, l.expires_at
            FROM locks l JOIN agents a ON a.id = l.holder_agent_id
            WHERE l.team_id = $1 AND l.expires_at > now()
            ORDER BY l.name
            "#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    let active_locks = lock_rows
        .into_iter()
        .map(
            |(name, holder, purpose, acquired_at, expires_at)| LockInfo {
                name,
                holder,
                purpose,
                acquired_at: ts(acquired_at),
                expires_at: ts(expires_at),
            },
        )
        .collect();

    Ok(DigestResult {
        hours,
        channels,
        tasks_moved,
        open_tasks,
        claimed_tasks,
        notes_updated,
        agents_seen,
        active_locks,
    })
}
