use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::AuthCtx,
    error::{BusError, BusResult},
    model::{ClaimResult, TaskDetail, TaskEventInfo, TaskInfo, TaskList, ts, ts_opt},
};

const DEFAULT_LEASE_SECS: i64 = 900; // 15 minutes
const MAX_LEASE_SECS: i64 = 86_400;
const MAX_LIMIT: i64 = 200;

#[derive(sqlx::FromRow)]
struct TaskRow {
    key: String,
    title: String,
    description: Option<String>,
    status: String,
    depends_on: Vec<String>,
    blocked: bool,
    claimed_by: Option<String>,
    claimed_at: Option<chrono::DateTime<chrono::Utc>>,
    lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    result: Option<String>,
    metadata: serde_json::Value,
    created_by: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<TaskRow> for TaskInfo {
    fn from(r: TaskRow) -> Self {
        let lease_expired = r.status == "claimed"
            && r.lease_expires_at
                .map(|e| e < chrono::Utc::now())
                .unwrap_or(false);
        TaskInfo {
            key: r.key,
            title: r.title,
            description: r.description,
            status: r.status,
            depends_on: r.depends_on,
            blocked: r.blocked,
            claimed_by: r.claimed_by,
            claimed_at: ts_opt(r.claimed_at),
            lease_expires_at: ts_opt(r.lease_expires_at),
            lease_expired,
            result: r.result,
            metadata: r.metadata,
            created_by: r.created_by,
            created_at: ts(r.created_at),
            updated_at: ts(r.updated_at),
        }
    }
}

const TASK_SELECT: &str = r#"
    SELECT t.id,
           t.key,
           t.title,
           t.description,
           t.status,
           COALESCE(
               (SELECT array_agg(d.key ORDER BY d.key)
                FROM task_deps td JOIN tasks d ON d.id = td.blocked_by_task_id
                WHERE td.task_id = t.id),
               '{}'
           ) AS depends_on,
           EXISTS (
               SELECT 1
               FROM task_deps td JOIN tasks d ON d.id = td.blocked_by_task_id
               WHERE td.task_id = t.id AND d.status NOT IN ('done', 'cancelled')
           ) AS blocked,
           cb.name AS claimed_by,
           t.claimed_at,
           t.lease_expires_at,
           t.result,
           t.metadata,
           crb.name AS created_by,
           t.created_at,
           t.updated_at
    FROM tasks t
    LEFT JOIN agents cb  ON cb.id = t.claimed_by
    LEFT JOIN agents crb ON crb.id = t.created_by
"#;

async fn log_event(
    pool: &PgPool,
    task_id: Uuid,
    agent_id: Uuid,
    event: &str,
    detail: Option<&str>,
) -> BusResult<()> {
    sqlx::query("INSERT INTO task_events (task_id, agent_id, event, detail) VALUES ($1,$2,$3,$4)")
        .bind(task_id)
        .bind(agent_id)
        .bind(event)
        .bind(detail)
        .execute(pool)
        .await?;
    Ok(())
}

fn normalize_key(key: &str) -> BusResult<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(BusError::invalid("task key cannot be empty"));
    }
    if key.len() > 128 {
        return Err(BusError::invalid("task key is limited to 128 characters"));
    }
    Ok(key.to_owned())
}

// ------------------------------------------------------------------ create --

pub struct CreateInput {
    pub key: String,
    pub title: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Keys of existing tasks this one depends on. The task cannot be claimed
    /// until every dependency is done or cancelled.
    pub depends_on: Vec<String>,
}

pub async fn create_task(pool: &PgPool, auth: &AuthCtx, input: CreateInput) -> BusResult<TaskInfo> {
    let key = normalize_key(&input.key)?;
    let title = input.title.trim();
    if title.is_empty() {
        return Err(BusError::invalid("task title cannot be empty"));
    }
    let metadata = input
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));

    let existing: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM tasks WHERE team_id = $1 AND key = $2")
            .bind(auth.team_id)
            .bind(&key)
            .fetch_optional(pool)
            .await?;
    if existing.is_some() {
        return Err(BusError::conflict(format!(
            "task '{key}' already exists; use get_task to inspect it"
        )));
    }

    // Resolve dependency keys before inserting anything, so a typo fails the
    // whole call instead of leaving a half-registered task.
    let mut dep_ids: Vec<Uuid> = Vec::with_capacity(input.depends_on.len());
    for dep_key in &input.depends_on {
        let dep_key = normalize_key(dep_key)?;
        if dep_key == key {
            return Err(BusError::invalid("a task cannot depend on itself"));
        }
        let dep: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM tasks WHERE team_id = $1 AND key = $2")
                .bind(auth.team_id)
                .bind(&dep_key)
                .fetch_optional(pool)
                .await?;
        match dep {
            Some((id,)) => dep_ids.push(id),
            None => {
                return Err(BusError::not_found(format!(
                    "dependency '{dep_key}' does not exist; create it first"
                )));
            }
        }
    }
    dep_ids.sort();
    dep_ids.dedup();

    let (id,): (Uuid,) = sqlx::query_as(
        r#"
        INSERT INTO tasks (team_id, key, title, description, metadata, created_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id
        "#,
    )
    .bind(auth.team_id)
    .bind(&key)
    .bind(title)
    .bind(input.description.as_deref())
    .bind(&metadata)
    .bind(auth.agent_id)
    .fetch_one(pool)
    .await?;

    // Dependencies only point at pre-existing tasks and this task is brand
    // new, so no cycle is possible by construction.
    for dep_id in &dep_ids {
        sqlx::query("INSERT INTO task_deps (task_id, blocked_by_task_id) VALUES ($1, $2)")
            .bind(id)
            .bind(dep_id)
            .execute(pool)
            .await?;
    }

    log_event(pool, id, auth.agent_id, "created", Some(title)).await?;
    fetch_task(pool, auth, &key).await
}

async fn fetch_task(pool: &PgPool, auth: &AuthCtx, key: &str) -> BusResult<TaskInfo> {
    let row: Option<TaskRow> = sqlx::query_as(&format!(
        "{TASK_SELECT} WHERE t.team_id = $1 AND t.key = $2"
    ))
    .bind(auth.team_id)
    .bind(key)
    .fetch_optional(pool)
    .await?;
    row.map(Into::into)
        .ok_or_else(|| BusError::not_found(format!("task '{key}'")))
}

pub async fn get_task(pool: &PgPool, auth: &AuthCtx, key: &str) -> BusResult<TaskDetail> {
    let task = fetch_task(pool, auth, key).await?;
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        r#"
            SELECT e.event, a.name, e.detail, e.created_at
            FROM task_events e
            LEFT JOIN agents a ON a.id = e.agent_id
            JOIN tasks t ON t.id = e.task_id
            WHERE t.team_id = $1 AND t.key = $2
            ORDER BY e.id
            "#,
    )
    .bind(auth.team_id)
    .bind(key.trim())
    .fetch_all(pool)
    .await?;

    Ok(TaskDetail {
        task,
        history: rows
            .into_iter()
            .map(|(event, agent, detail, created_at)| TaskEventInfo {
                event,
                agent,
                detail,
                created_at: ts(created_at),
            })
            .collect(),
    })
}

// -------------------------------------------------------------------- list --

pub async fn list_tasks(
    pool: &PgPool,
    auth: &AuthCtx,
    status: Option<String>,
    mine_only: bool,
    limit: i64,
) -> BusResult<TaskList> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let status = status
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s != "any");
    if let Some(s) = &status
        && !["open", "claimed", "done", "cancelled"].contains(&s.as_str())
    {
        return Err(BusError::invalid(
            "status must be one of: open, claimed, done, cancelled, any",
        ));
    }

    let rows: Vec<TaskRow> = sqlx::query_as(&format!(
        r#"{TASK_SELECT}
           WHERE t.team_id = $1
             AND ($2::text IS NULL OR t.status = $2)
             AND (NOT $3::bool OR t.claimed_by = $4)
           ORDER BY
             CASE t.status WHEN 'claimed' THEN 0 WHEN 'open' THEN 1 ELSE 2 END,
             t.updated_at DESC
           LIMIT $5"#
    ))
    .bind(auth.team_id)
    .bind(status.as_deref())
    .bind(mine_only)
    .bind(auth.agent_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let (open, claimed): (i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*) FILTER (WHERE status = 'open'),
               count(*) FILTER (WHERE status = 'claimed')
        FROM tasks WHERE team_id = $1
        "#,
    )
    .bind(auth.team_id)
    .fetch_one(pool)
    .await?;

    Ok(TaskList {
        tasks: rows.into_iter().map(Into::into).collect(),
        open,
        claimed,
    })
}

// ------------------------------------------------------------------- claim --

/// Claim a specific task. Succeeds when the task is open, when its lease has
/// already expired, or when the caller already holds it (idempotent re-claim).
pub async fn claim_task(
    pool: &PgPool,
    auth: &AuthCtx,
    key: &str,
    lease_seconds: Option<i64>,
) -> BusResult<ClaimResult> {
    let key = normalize_key(key)?;
    let lease = lease_seconds
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(30, MAX_LEASE_SECS);

    let updated: Option<(Uuid,)> = sqlx::query_as(
        r#"
        UPDATE tasks
        SET status = 'claimed',
            claimed_by = $1,
            claimed_at = now(),
            lease_expires_at = now() + make_interval(secs => $2),
            updated_at = now()
        WHERE team_id = $3
          AND key = $4
          AND status IN ('open', 'claimed')
          AND (status = 'open'
               OR claimed_by = $1
               OR lease_expires_at IS NULL
               OR lease_expires_at < now())
          AND NOT EXISTS (
              SELECT 1
              FROM task_deps td JOIN tasks d ON d.id = td.blocked_by_task_id
              WHERE td.task_id = tasks.id AND d.status NOT IN ('done', 'cancelled')
          )
        RETURNING id
        "#,
    )
    .bind(auth.agent_id)
    .bind(lease as f64)
    .bind(auth.team_id)
    .bind(&key)
    .fetch_optional(pool)
    .await?;

    match updated {
        Some((id,)) => {
            log_event(pool, id, auth.agent_id, "claimed", None).await?;
            Ok(ClaimResult {
                claimed: true,
                task: Some(fetch_task(pool, auth, &key).await?),
                reason: None,
            })
        }
        None => {
            // Distinguish "does not exist" from "someone else holds it".
            let current = fetch_task(pool, auth, &key).await?;
            let reason = if current.blocked {
                format!(
                    "blocked by unfinished dependencies: {}",
                    current.depends_on.join(", ")
                )
            } else {
                match current.status.as_str() {
                    "claimed" => format!(
                        "held by {} until {}",
                        current.claimed_by.clone().unwrap_or_else(|| "?".into()),
                        current
                            .lease_expires_at
                            .clone()
                            .unwrap_or_else(|| "unknown".into())
                    ),
                    other => format!("task is {other}"),
                }
            };
            Ok(ClaimResult {
                claimed: false,
                task: Some(current),
                reason: Some(reason),
            })
        }
    }
}

/// Claim the oldest available task. Uses `SKIP LOCKED` so several agents can
/// call this concurrently without handing the same task to two of them.
pub async fn claim_next_task(
    pool: &PgPool,
    auth: &AuthCtx,
    lease_seconds: Option<i64>,
) -> BusResult<ClaimResult> {
    let lease = lease_seconds
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(30, MAX_LEASE_SECS);

    let picked: Option<(Uuid, String)> = sqlx::query_as(
        r#"
        WITH candidate AS (
            SELECT id
            FROM tasks
            WHERE team_id = $1
              AND (status = 'open'
                   OR (status = 'claimed' AND lease_expires_at < now()))
              AND NOT EXISTS (
                  SELECT 1
                  FROM task_deps td JOIN tasks d ON d.id = td.blocked_by_task_id
                  WHERE td.task_id = tasks.id AND d.status NOT IN ('done', 'cancelled')
              )
            ORDER BY created_at
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        UPDATE tasks t
        SET status = 'claimed',
            claimed_by = $2,
            claimed_at = now(),
            lease_expires_at = now() + make_interval(secs => $3),
            updated_at = now()
        FROM candidate c
        WHERE t.id = c.id
        RETURNING t.id, t.key
        "#,
    )
    .bind(auth.team_id)
    .bind(auth.agent_id)
    .bind(lease as f64)
    .fetch_optional(pool)
    .await?;

    match picked {
        Some((id, key)) => {
            log_event(
                pool,
                id,
                auth.agent_id,
                "claimed",
                Some("via claim_next_task"),
            )
            .await?;
            Ok(ClaimResult {
                claimed: true,
                task: Some(fetch_task(pool, auth, &key).await?),
                reason: None,
            })
        }
        None => Ok(ClaimResult {
            claimed: false,
            task: None,
            reason: Some("no unclaimed task available".into()),
        }),
    }
}

pub async fn renew_lease(
    pool: &PgPool,
    auth: &AuthCtx,
    key: &str,
    lease_seconds: Option<i64>,
) -> BusResult<TaskInfo> {
    let key = normalize_key(key)?;
    let lease = lease_seconds
        .unwrap_or(DEFAULT_LEASE_SECS)
        .clamp(30, MAX_LEASE_SECS);

    let updated: Option<(Uuid,)> = sqlx::query_as(
        r#"
        UPDATE tasks
        SET lease_expires_at = now() + make_interval(secs => $1),
            updated_at = now()
        WHERE team_id = $2 AND key = $3 AND claimed_by = $4 AND status = 'claimed'
        RETURNING id
        "#,
    )
    .bind(lease as f64)
    .bind(auth.team_id)
    .bind(&key)
    .bind(auth.agent_id)
    .fetch_optional(pool)
    .await?;

    if updated.is_none() {
        return Err(BusError::conflict(format!(
            "you do not hold an active claim on '{key}'"
        )));
    }
    fetch_task(pool, auth, &key).await
}

pub async fn release_task(pool: &PgPool, auth: &AuthCtx, key: &str) -> BusResult<TaskInfo> {
    let key = normalize_key(key)?;
    let updated: Option<(Uuid,)> = sqlx::query_as(
        r#"
        UPDATE tasks
        SET status = 'open',
            claimed_by = NULL,
            claimed_at = NULL,
            lease_expires_at = NULL,
            updated_at = now()
        WHERE team_id = $1 AND key = $2 AND claimed_by = $3 AND status = 'claimed'
        RETURNING id
        "#,
    )
    .bind(auth.team_id)
    .bind(&key)
    .bind(auth.agent_id)
    .fetch_optional(pool)
    .await?;

    match updated {
        Some((id,)) => {
            log_event(pool, id, auth.agent_id, "released", None).await?;
            fetch_task(pool, auth, &key).await
        }
        None => Err(BusError::conflict(format!(
            "you do not hold an active claim on '{key}'"
        ))),
    }
}

pub async fn complete_task(
    pool: &PgPool,
    auth: &AuthCtx,
    key: &str,
    result: Option<String>,
) -> BusResult<TaskInfo> {
    let key = normalize_key(key)?;
    let updated: Option<(Uuid,)> = sqlx::query_as(
        r#"
        UPDATE tasks
        SET status = 'done',
            result = $1,
            lease_expires_at = NULL,
            updated_at = now()
        WHERE team_id = $2 AND key = $3 AND status IN ('open', 'claimed')
        RETURNING id
        "#,
    )
    .bind(result.as_deref())
    .bind(auth.team_id)
    .bind(&key)
    .fetch_optional(pool)
    .await?;

    match updated {
        Some((id,)) => {
            log_event(pool, id, auth.agent_id, "completed", result.as_deref()).await?;
            fetch_task(pool, auth, &key).await
        }
        None => {
            let current = fetch_task(pool, auth, &key).await?;
            Err(BusError::conflict(format!(
                "task '{key}' is already {}",
                current.status
            )))
        }
    }
}
