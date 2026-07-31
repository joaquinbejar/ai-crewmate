use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::AuthCtx,
    error::{BusError, BusResult},
    model::{Ack, NoteInfo, NoteList, NoteRef, ts},
};

const MAX_VALUE_BYTES: usize = 256 * 1024;
const MAX_LIMIT: i64 = 200;

#[derive(sqlx::FromRow)]
struct NoteRow {
    scope: String,
    key: String,
    value: String,
    tags: Vec<String>,
    updated_by: Option<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<NoteRow> for NoteInfo {
    fn from(r: NoteRow) -> Self {
        NoteInfo {
            scope: r.scope,
            key: r.key,
            value: r.value,
            tags: r.tags,
            updated_by: r.updated_by,
            updated_at: ts(r.updated_at),
        }
    }
}

const NOTE_SELECT: &str = r#"
    SELECT n.scope, n.key, n.value, n.tags, a.name AS updated_by, n.updated_at
    FROM notes n
    LEFT JOIN agents a ON a.id = n.updated_by
"#;

fn normalize_scope(scope: Option<String>) -> String {
    scope
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "global".into())
}

pub struct SetInput {
    pub scope: Option<String>,
    pub key: String,
    pub value: String,
    pub tags: Option<Vec<String>>,
}

pub async fn set_note(pool: &PgPool, auth: &AuthCtx, input: SetInput) -> BusResult<NoteInfo> {
    let scope = normalize_scope(input.scope);
    let key = input.key.trim().to_owned();
    if key.is_empty() {
        return Err(BusError::invalid("note key cannot be empty"));
    }
    if input.value.len() > MAX_VALUE_BYTES {
        return Err(BusError::invalid(format!(
            "note value is limited to {MAX_VALUE_BYTES} bytes"
        )));
    }
    let tags: Vec<String> = input
        .tags
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();

    let (id,): (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO notes (team_id, scope, key, value, tags, updated_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (team_id, scope, key) DO UPDATE SET
            value = EXCLUDED.value,
            tags = EXCLUDED.tags,
            updated_by = EXCLUDED.updated_by,
            updated_at = now()
        RETURNING id
        "#,
    )
    .bind(auth.team_id)
    .bind(&scope)
    .bind(&key)
    .bind(&input.value)
    .bind(&tags)
    .bind(auth.agent_id)
    .fetch_one(pool)
    .await?;

    // Keep an append-only trail so a bad overwrite is recoverable.
    sqlx::query("INSERT INTO note_revisions (note_id, value, updated_by) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(&input.value)
        .bind(auth.agent_id)
        .execute(pool)
        .await?;

    get_note(pool, auth, Some(scope), &key)
        .await?
        .note
        .ok_or_else(|| BusError::not_found("note just written"))
}

pub async fn get_note(
    pool: &PgPool,
    auth: &AuthCtx,
    scope: Option<String>,
    key: &str,
) -> BusResult<NoteRef> {
    let scope = normalize_scope(scope);
    let key = key.trim().to_owned();

    let row: Option<NoteRow> = sqlx::query_as(&format!(
        "{NOTE_SELECT} WHERE n.team_id = $1 AND n.scope = $2 AND n.key = $3"
    ))
    .bind(auth.team_id)
    .bind(&scope)
    .bind(&key)
    .fetch_optional(pool)
    .await?;

    Ok(NoteRef {
        scope,
        key,
        found: row.is_some(),
        note: row.map(Into::into),
    })
}

pub async fn list_notes(
    pool: &PgPool,
    auth: &AuthCtx,
    scope: Option<String>,
    tag: Option<String>,
    limit: i64,
) -> BusResult<NoteList> {
    let limit = limit.clamp(1, MAX_LIMIT);
    // `scope = None` means "every scope", so it is not normalised to 'global'.
    let scope = scope.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    let tag = tag.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty());

    let rows: Vec<NoteRow> = sqlx::query_as(&format!(
        r#"{NOTE_SELECT}
           WHERE n.team_id = $1
             AND ($2::text IS NULL OR n.scope = $2)
             AND ($3::text IS NULL OR $3 = ANY(n.tags))
           ORDER BY n.scope, n.key
           LIMIT $4"#
    ))
    .bind(auth.team_id)
    .bind(scope.as_deref())
    .bind(tag.as_deref())
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(NoteList {
        notes: rows.into_iter().map(Into::into).collect(),
    })
}

pub async fn search_notes(
    pool: &PgPool,
    auth: &AuthCtx,
    query: &str,
    scope: Option<String>,
    limit: i64,
) -> BusResult<NoteList> {
    let query = query.trim();
    if query.is_empty() {
        return Err(BusError::invalid("search query cannot be empty"));
    }
    let limit = limit.clamp(1, MAX_LIMIT);
    let scope = scope.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

    let rows: Vec<NoteRow> = sqlx::query_as(&format!(
        r#"{NOTE_SELECT}
           WHERE n.team_id = $1
             AND ($2::text IS NULL OR n.scope = $2)
             AND to_tsvector('simple', n.key || ' ' || n.value)
                 @@ plainto_tsquery('simple', $3)
           ORDER BY n.updated_at DESC
           LIMIT $4"#
    ))
    .bind(auth.team_id)
    .bind(scope.as_deref())
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(NoteList {
        notes: rows.into_iter().map(Into::into).collect(),
    })
}

pub async fn delete_note(
    pool: &PgPool,
    auth: &AuthCtx,
    scope: Option<String>,
    key: &str,
) -> BusResult<Ack> {
    let scope = normalize_scope(scope);
    let key = key.trim();

    let res = sqlx::query("DELETE FROM notes WHERE team_id = $1 AND scope = $2 AND key = $3")
        .bind(auth.team_id)
        .bind(&scope)
        .bind(key)
        .execute(pool)
        .await?;

    Ok(Ack {
        ok: res.rows_affected() > 0,
        detail: if res.rows_affected() > 0 {
            format!("deleted note {scope}/{key}")
        } else {
            format!("no note at {scope}/{key}")
        },
    })
}
