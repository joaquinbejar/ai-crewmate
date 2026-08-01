use sqlx::PgPool;
use uuid::Uuid;

use super::{agent_id_by_name, channel_id_by_name, normalize_channel};
use crate::{
    auth::AuthCtx,
    error::{BusError, BusResult},
    model::{ChannelInfo, ChannelList, MessageInfo, MessageList, PostMessageResult, ts},
};

const MAX_LIMIT: i64 = 200;
const MAX_BODY_BYTES: usize = 64 * 1024;

/// A joined message row as it comes back from Postgres.
#[derive(sqlx::FromRow)]
struct MessageRow {
    id: i64,
    sender: String,
    channel: Option<String>,
    recipient: Option<String>,
    body: String,
    reply_to: Option<i64>,
    metadata: serde_json::Value,
    attachments: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<MessageRow> for MessageInfo {
    fn from(r: MessageRow) -> Self {
        MessageInfo {
            id: r.id,
            from: r.sender,
            channel: r.channel,
            to: r.recipient,
            body: r.body,
            reply_to: r.reply_to,
            metadata: r.metadata,
            attachments: serde_json::from_value(r.attachments).unwrap_or_default(),
            created_at: ts(r.created_at),
        }
    }
}

const MESSAGE_SELECT: &str = r#"
    SELECT m.id,
           s.name  AS sender,
           ch.name AS channel,
           r.name  AS recipient,
           m.body,
           m.reply_to,
           m.metadata,
           COALESCE(
               (SELECT json_agg(json_build_object(
                           'id', a.id, 'filename', a.filename,
                           'content_type', a.content_type, 'size_bytes', a.size_bytes)
                       ORDER BY a.id)
                FROM attachments a WHERE a.message_id = m.id),
               '[]'::json
           ) AS attachments,
           m.created_at
    FROM messages m
    JOIN agents s        ON s.id = m.sender_agent_id
    LEFT JOIN channels ch ON ch.id = m.channel_id
    LEFT JOIN agents r    ON r.id = m.recipient_agent_id
"#;

// ---------------------------------------------------------------- channels --

pub async fn create_channel(
    pool: &PgPool,
    auth: &AuthCtx,
    name: &str,
    topic: Option<String>,
) -> BusResult<ChannelInfo> {
    let name = normalize_channel(name);
    if name.is_empty() {
        return Err(BusError::invalid("channel name cannot be empty"));
    }
    if name.len() > 64 {
        return Err(BusError::invalid(
            "channel name is limited to 64 characters",
        ));
    }

    let row: (Uuid, String, Option<String>, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        r#"
        INSERT INTO channels (team_id, name, topic, created_by)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (team_id, name) DO UPDATE
            SET topic = COALESCE(EXCLUDED.topic, channels.topic)
        RETURNING id, name, topic, created_at
        "#,
    )
    .bind(auth.team_id)
    .bind(&name)
    .bind(topic)
    .bind(auth.agent_id)
    .fetch_one(pool)
    .await?;

    Ok(ChannelInfo {
        name: row.1,
        topic: row.2,
        message_count: 0,
        created_at: ts(row.3),
    })
}

pub async fn list_channels(pool: &PgPool, auth: &AuthCtx) -> BusResult<ChannelList> {
    let rows: Vec<(String, Option<String>, i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        r#"
        SELECT c.name,
               c.topic,
               (SELECT count(*) FROM messages m WHERE m.channel_id = c.id) AS message_count,
               c.created_at
        FROM channels c
        WHERE c.team_id = $1
        ORDER BY c.name
        "#,
    )
    .bind(auth.team_id)
    .fetch_all(pool)
    .await?;

    Ok(ChannelList {
        channels: rows
            .into_iter()
            .map(|(name, topic, message_count, created_at)| ChannelInfo {
                name,
                topic,
                message_count,
                created_at: ts(created_at),
            })
            .collect(),
    })
}

// ---------------------------------------------------------------- posting --

pub struct PostInput {
    pub channel: Option<String>,
    pub to: Option<String>,
    pub body: String,
    pub reply_to: Option<i64>,
    pub metadata: Option<serde_json::Value>,
    /// Already decoded and size-checked; inserted in the same transaction as
    /// the message so readers never see the message without its files.
    pub attachments: Vec<super::attachments::NewAttachment>,
}

pub async fn post_message(
    pool: &PgPool,
    auth: &AuthCtx,
    input: PostInput,
) -> BusResult<PostMessageResult> {
    let body = input.body.trim().to_owned();
    if body.is_empty() {
        return Err(BusError::invalid("message body cannot be empty"));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(BusError::invalid(format!(
            "message body is limited to {MAX_BODY_BYTES} bytes"
        )));
    }

    let (channel_id, recipient_id, delivered_to) = match (&input.channel, &input.to) {
        (Some(_), Some(_)) => {
            return Err(BusError::invalid(
                "set either `channel` or `to`, not both: a message is either broadcast or direct",
            ));
        }
        (None, None) => {
            return Err(BusError::invalid(
                "set `channel` to broadcast, or `to` to send a direct message",
            ));
        }
        (Some(channel), None) => {
            let id = channel_id_by_name(pool, auth.team_id, channel).await?;
            // Everyone in the team can read a channel, so report the roster.
            let names: Vec<(String,)> = sqlx::query_as(
                "SELECT name FROM agents WHERE team_id = $1 AND disabled_at IS NULL ORDER BY name",
            )
            .bind(auth.team_id)
            .fetch_all(pool)
            .await?;
            (
                Some(id),
                None,
                names.into_iter().map(|r| r.0).collect::<Vec<_>>(),
            )
        }
        (None, Some(to)) => {
            let id = agent_id_by_name(pool, auth.team_id, to).await?;
            (None, Some(id), vec![to.trim().to_owned()])
        }
    };

    // A reply must point at a message in the same team.
    if let Some(reply_to) = input.reply_to {
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM messages WHERE id = $1 AND team_id = $2")
                .bind(reply_to)
                .bind(auth.team_id)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            return Err(BusError::not_found(format!("message {reply_to}")));
        }
    }

    super::check_metadata("message", input.metadata.as_ref())?;
    let metadata = input
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    // Message + attachments commit together, so the NOTIFY that wakes
    // teammates only fires once everything is readable.
    let mut tx = pool.begin().await?;

    let (id,): (i64,) = sqlx::query_as(
        r#"
        INSERT INTO messages
            (team_id, channel_id, recipient_agent_id, sender_agent_id, body, reply_to, metadata)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(auth.team_id)
    .bind(channel_id)
    .bind(recipient_id)
    .bind(auth.agent_id)
    .bind(&body)
    .bind(input.reply_to)
    .bind(&metadata)
    .fetch_one(&mut *tx)
    .await?;

    for att in &input.attachments {
        super::attachments::insert_for_message(&mut tx, auth.team_id, id, auth.agent_id, att)
            .await?;
    }

    let row: MessageRow = sqlx::query_as(&format!("{MESSAGE_SELECT} WHERE m.id = $1"))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(PostMessageResult {
        message: row.into(),
        delivered_to,
    })
}

// ----------------------------------------------------------------- asking --

/// The answer to a question DM: their explicit reply if there is one, else
/// their first direct message to the asker after the question was sent.
pub async fn find_answer(
    pool: &PgPool,
    auth: &AuthCtx,
    target_id: Uuid,
    question_id: i64,
) -> BusResult<Option<MessageInfo>> {
    let row: Option<MessageRow> = sqlx::query_as(&format!(
        r#"{MESSAGE_SELECT}
           WHERE m.sender_agent_id = $1
             AND m.recipient_agent_id = $2
             AND m.id > $3
           ORDER BY (m.reply_to = $3) DESC NULLS LAST, m.id
           LIMIT 1"#
    ))
    .bind(target_id)
    .bind(auth.agent_id)
    .bind(question_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

/// A resumed ask must point at a question the caller actually sent to the
/// target; anything else means a mixed-up id.
pub async fn verify_question(
    pool: &PgPool,
    auth: &AuthCtx,
    target_id: Uuid,
    question_id: i64,
) -> BusResult<()> {
    let exists: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM messages
         WHERE id = $1 AND sender_agent_id = $2 AND recipient_agent_id = $3",
    )
    .bind(question_id)
    .bind(auth.agent_id)
    .bind(target_id)
    .fetch_optional(pool)
    .await?;
    if exists.is_none() {
        return Err(BusError::invalid(format!(
            "message {question_id} is not a question you sent to this agent; \
             pass the question_message_id returned by ask_agent"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------- reading --

/// Normalised read scope plus the cursor key used to remember the read position.
enum Scope {
    All,
    Inbox,
    Channel { id: Uuid, name: String },
}

impl Scope {
    fn cursor_key(&self) -> String {
        match self {
            Scope::All => "all".into(),
            Scope::Inbox => "inbox".into(),
            Scope::Channel { id, .. } => format!("channel:{id}"),
        }
    }
    fn label(&self) -> String {
        match self {
            Scope::All => "all".into(),
            Scope::Inbox => "inbox".into(),
            Scope::Channel { name, .. } => format!("#{name}"),
        }
    }
}

async fn resolve_scope(pool: &PgPool, auth: &AuthCtx, raw: &str) -> BusResult<Scope> {
    match raw.trim().to_lowercase().as_str() {
        "" | "all" => Ok(Scope::All),
        "inbox" | "dm" | "direct" => Ok(Scope::Inbox),
        other => {
            let name = normalize_channel(other);
            let id = channel_id_by_name(pool, auth.team_id, &name).await?;
            Ok(Scope::Channel { id, name })
        }
    }
}

pub struct ReadInput {
    pub scope: String,
    pub only_new: bool,
    pub limit: i64,
}

pub async fn read_messages(
    pool: &PgPool,
    auth: &AuthCtx,
    input: ReadInput,
) -> BusResult<MessageList> {
    let scope = resolve_scope(pool, auth, &input.scope).await?;
    let limit = input.limit.clamp(1, MAX_LIMIT);
    let cursor_key = scope.cursor_key();

    let since: i64 = if input.only_new {
        sqlx::query_as::<_, (i64,)>(
            "SELECT last_message_id FROM read_cursors WHERE agent_id = $1 AND scope = $2",
        )
        .bind(auth.agent_id)
        .bind(&cursor_key)
        .fetch_optional(pool)
        .await?
        .map(|r| r.0)
        .unwrap_or(0)
    } else {
        0
    };

    // Ordered ascending so the agent reads the conversation in chronological
    // order; when not filtering by cursor we take the newest `limit` and then
    // flip them back, so "the last N messages" is what you get.
    let rows: Vec<MessageRow> = match &scope {
        Scope::All => {
            sqlx::query_as(&format!(
                r#"{MESSAGE_SELECT}
                   WHERE m.team_id = $1
                     AND m.id > $2
                     AND (m.channel_id IS NOT NULL
                          OR m.recipient_agent_id = $3
                          OR m.sender_agent_id = $3)
                   ORDER BY m.id DESC
                   LIMIT $4"#
            ))
            .bind(auth.team_id)
            .bind(since)
            .bind(auth.agent_id)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        Scope::Inbox => {
            sqlx::query_as(&format!(
                r#"{MESSAGE_SELECT}
                   WHERE m.recipient_agent_id = $1 AND m.id > $2
                   ORDER BY m.id DESC
                   LIMIT $3"#
            ))
            .bind(auth.agent_id)
            .bind(since)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        Scope::Channel { id, .. } => {
            sqlx::query_as(&format!(
                r#"{MESSAGE_SELECT}
                   WHERE m.channel_id = $1 AND m.id > $2
                   ORDER BY m.id DESC
                   LIMIT $3"#
            ))
            .bind(*id)
            .bind(since)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    let truncated = rows.len() as i64 == limit;
    let mut messages: Vec<MessageInfo> = rows.into_iter().map(Into::into).collect();
    messages.reverse();

    let new_cursor = messages.iter().map(|m| m.id).max().unwrap_or(since);
    if input.only_new && new_cursor > since {
        sqlx::query(
            r#"
            INSERT INTO read_cursors (agent_id, scope, last_message_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (agent_id, scope) DO UPDATE
                SET last_message_id = GREATEST(read_cursors.last_message_id, EXCLUDED.last_message_id),
                    updated_at = now()
            "#,
        )
        .bind(auth.agent_id)
        .bind(&cursor_key)
        .bind(new_cursor)
        .execute(pool)
        .await?;
    }

    Ok(MessageList {
        messages,
        scope: scope.label(),
        cursor: new_cursor,
        truncated,
    })
}

pub async fn search_messages(
    pool: &PgPool,
    auth: &AuthCtx,
    query: &str,
    limit: i64,
) -> BusResult<MessageList> {
    let query = query.trim();
    if query.is_empty() {
        return Err(BusError::invalid("search query cannot be empty"));
    }
    let limit = limit.clamp(1, MAX_LIMIT);

    let rows: Vec<MessageRow> = sqlx::query_as(&format!(
        r#"{MESSAGE_SELECT}
           WHERE m.team_id = $1
             AND (m.channel_id IS NOT NULL
                  OR m.recipient_agent_id = $2
                  OR m.sender_agent_id = $2)
             AND to_tsvector('simple', m.body) @@ plainto_tsquery('simple', $3)
           ORDER BY m.id DESC
           LIMIT $4"#
    ))
    .bind(auth.team_id)
    .bind(auth.agent_id)
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let truncated = rows.len() as i64 == limit;
    let messages: Vec<MessageInfo> = rows.into_iter().map(Into::into).collect();
    let cursor = messages.iter().map(|m| m.id).max().unwrap_or(0);

    Ok(MessageList {
        messages,
        scope: format!("search:{query}"),
        cursor,
        truncated,
    })
}
