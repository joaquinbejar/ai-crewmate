pub mod digest;
pub mod locks;
pub mod messaging;
pub mod notes;
pub mod presence;
pub mod quota;
pub mod tasks;

pub mod attachments;

use sqlx::PgPool;
use uuid::Uuid;

use crate::{auth::AuthCtx, error::BusError, error::BusResult, model::WhoAmI};

/// Channel names are normalised so `#dev`, `dev` and `DEV` all address the same
/// channel. Keeps the model from creating near-duplicate channels.
pub fn normalize_channel(name: &str) -> String {
    name.trim().trim_start_matches('#').trim().to_lowercase()
}

/// `metadata` is a structured pointer — an id, a flag, a short label — not a
/// document. Bodies, notes and attachments are the places built to carry size.
pub const MAX_METADATA_BYTES: usize = 16 * 1024;

/// Reject an oversized `metadata` object before it reaches the database. The
/// error names the field and the limit so an LLM caller can trim and retry.
pub fn check_metadata(field: &str, metadata: Option<&serde_json::Value>) -> BusResult<()> {
    let Some(value) = metadata else {
        return Ok(());
    };
    // Serialising is the only honest measure of what gets stored.
    let size = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
    if size > MAX_METADATA_BYTES {
        return Err(BusError::invalid(format!(
            "{field} metadata is {size} bytes; the limit is {MAX_METADATA_BYTES}. \
             Keep metadata to identifiers and short labels — put the payload in \
             the body, a note, or an attachment."
        )));
    }
    Ok(())
}

/// Trim a free-text field and reject it when it exceeds `max` bytes.
/// Returns the trimmed value so callers store exactly what was validated.
pub fn check_text(field: &str, value: &str, max: usize) -> BusResult<String> {
    let trimmed = value.trim();
    if trimmed.len() > max {
        return Err(BusError::invalid(format!(
            "{field} is {} bytes; the limit is {max}",
            trimmed.len()
        )));
    }
    Ok(trimmed.to_owned())
}

pub async fn agent_id_by_name(pool: &PgPool, team_id: Uuid, name: &str) -> BusResult<Uuid> {
    let name = name.trim();
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM agents WHERE team_id = $1 AND name = $2")
            .bind(team_id)
            .bind(name)
            .fetch_optional(pool)
            .await?;
    row.map(|r| r.0)
        .ok_or_else(|| BusError::not_found(format!("no agent named '{name}' in this team")))
}

pub async fn channel_id_by_name(pool: &PgPool, team_id: Uuid, name: &str) -> BusResult<Uuid> {
    let name = normalize_channel(name);
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM channels WHERE team_id = $1 AND name = $2")
            .bind(team_id)
            .bind(&name)
            .fetch_optional(pool)
            .await?;
    row.map(|r| r.0).ok_or_else(|| {
        BusError::not_found(format!(
            "no channel named '{name}'; call list_channels or create it with create_channel"
        ))
    })
}

pub async fn whoami(pool: &PgPool, auth: &AuthCtx) -> BusResult<WhoAmI> {
    let (unread,): (i64,) = sqlx::query_as(
        r#"
        SELECT count(*)
        FROM messages m
        LEFT JOIN read_cursors c
               ON c.agent_id = $1 AND c.scope = 'inbox'
        WHERE m.recipient_agent_id = $1
          AND m.id > COALESCE(c.last_message_id, 0)
        "#,
    )
    .bind(auth.agent_id)
    .fetch_one(pool)
    .await?;

    let (claimed,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM tasks WHERE claimed_by = $1 AND status = 'claimed'")
            .bind(auth.agent_id)
            .fetch_one(pool)
            .await?;

    Ok(WhoAmI {
        agent: auth.agent_name.clone(),
        agent_id: auth.agent_id.to_string(),
        team: auth.team_slug.clone(),
        team_id: auth.team_id.to_string(),
        unread_direct_messages: unread,
        open_claimed_tasks: claimed,
    })
}
