//! Per-team storage quotas and retention.
//!
//! Attachments live in Postgres as bytea — deliberately, so the deployment
//! stays one binary and one database — which makes the database the object
//! store. Nothing bounded its growth, so one team could fill the volume every
//! other team shares.
//!
//! Quotas are opt-in per team: `NULL` means unlimited, so an existing
//! deployment behaves exactly as before until an operator sets one.

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{BusError, BusResult};

/// What a team is currently using. Counts and bytes only — never content, so
/// this is safe to print to an operator who is not on the team.
#[derive(Debug, sqlx::FromRow)]
pub struct Usage {
    pub attachment_bytes: i64,
    pub attachment_count: i64,
    pub attachment_bytes_limit: Option<i64>,
    pub messages: i64,
    pub note_revisions: i64,
    pub task_events: i64,
    pub oldest_message: Option<chrono::DateTime<chrono::Utc>>,
}

impl Usage {
    /// Percentage of the quota in use, when there is one.
    pub fn percent_used(&self) -> Option<f64> {
        self.attachment_bytes_limit
            .filter(|l| *l > 0)
            .map(|limit| (self.attachment_bytes as f64 / limit as f64) * 100.0)
    }
}

pub async fn usage(pool: &PgPool, team_id: Uuid) -> BusResult<Usage> {
    let usage = sqlx::query_as::<_, Usage>(
        r#"
        SELECT
            COALESCE((SELECT sum(size_bytes) FROM attachments WHERE team_id = $1), 0)::bigint
                AS attachment_bytes,
            (SELECT count(*) FROM attachments WHERE team_id = $1) AS attachment_count,
            (SELECT attachment_bytes_limit FROM teams WHERE id = $1) AS attachment_bytes_limit,
            (SELECT count(*) FROM messages WHERE team_id = $1) AS messages,
            (SELECT count(*) FROM note_revisions r
               JOIN notes n ON n.id = r.note_id WHERE n.team_id = $1) AS note_revisions,
            (SELECT count(*) FROM task_events e
               JOIN tasks t ON t.id = e.task_id WHERE t.team_id = $1) AS task_events,
            (SELECT min(created_at) FROM messages WHERE team_id = $1) AS oldest_message
        "#,
    )
    .bind(team_id)
    .fetch_one(pool)
    .await?;
    Ok(usage)
}

/// Reject an upload that would take the team past its quota.
///
/// Runs inside the caller's transaction and takes a row lock on the team, so
/// two uploads racing to the last megabyte are serialised: without the lock
/// both would read the same total, both would see room, and both would
/// commit.
pub async fn check_attachment_quota(
    conn: &mut sqlx::PgConnection,
    team_id: Uuid,
    incoming_bytes: i64,
) -> BusResult<()> {
    let limit: Option<i64> =
        sqlx::query_scalar("SELECT attachment_bytes_limit FROM teams WHERE id = $1 FOR UPDATE")
            .bind(team_id)
            .fetch_one(&mut *conn)
            .await?;
    let Some(limit) = limit else {
        return Ok(()); // unlimited
    };

    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(size_bytes), 0)::bigint FROM attachments WHERE team_id = $1",
    )
    .bind(team_id)
    .fetch_one(&mut *conn)
    .await?;

    if used + incoming_bytes > limit {
        return Err(BusError::invalid(format!(
            "attachment quota exceeded: this team stores {used} of {limit} bytes and this \
             file adds {incoming_bytes}. Delete the task or message carrying an old \
             attachment, or ask an operator to raise the quota."
        )));
    }
    Ok(())
}

/// What a prune would remove, or removed.
#[derive(Debug, Default)]
pub struct PruneReport {
    pub messages: u64,
    pub note_revisions: u64,
    pub task_events: u64,
    pub attachments_freed_bytes: i64,
    pub dry_run: bool,
}

/// Delete team data older than `older_than_days`.
///
/// Runs in one transaction so a failure leaves nothing half-pruned, and rolls
/// back deliberately when `dry_run` is set — the counts are then exactly what
/// a real run would remove, measured rather than estimated.
///
/// Referential integrity does the rest: attachments cascade from their
/// message, note revisions from their note. Notes and tasks themselves are
/// never pruned — they are the team's durable memory, and only their history
/// is trimmed.
pub async fn prune(
    pool: &PgPool,
    team_id: Uuid,
    older_than_days: i64,
    dry_run: bool,
) -> BusResult<PruneReport> {
    if older_than_days < 1 {
        return Err(BusError::invalid(
            "older_than_days must be at least 1; pruning everything is not a retention policy",
        ));
    }
    let mut tx = pool.begin().await?;

    // Measured before the delete, because the rows are gone afterwards.
    let freed: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(sum(a.size_bytes), 0)::bigint
        FROM attachments a
        JOIN messages m ON m.id = a.message_id
        WHERE a.team_id = $1 AND m.created_at < now() - make_interval(days => $2)
        "#,
    )
    .bind(team_id)
    .bind(older_than_days as i32)
    .fetch_one(&mut *tx)
    .await?;

    // Note revisions: keep the current value (that lives on `notes`), trim the
    // history behind it.
    let note_revisions = sqlx::query(
        r#"
        DELETE FROM note_revisions r
        USING notes n
        WHERE r.note_id = n.id AND n.team_id = $1
          AND r.created_at < now() - make_interval(days => $2)
        "#,
    )
    .bind(team_id)
    .bind(older_than_days as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let task_events = sqlx::query(
        r#"
        DELETE FROM task_events e
        USING tasks t
        WHERE e.task_id = t.id AND t.team_id = $1
          AND e.created_at < now() - make_interval(days => $2)
        "#,
    )
    .bind(team_id)
    .bind(older_than_days as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // Attachments cascade from the message, so this frees their bytes too.
    let messages = sqlx::query(
        "DELETE FROM messages WHERE team_id = $1 AND created_at < now() - make_interval(days => $2)",
    )
    .bind(team_id)
    .bind(older_than_days as i32)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let report = PruneReport {
        messages,
        note_revisions,
        task_events,
        attachments_freed_bytes: freed,
        dry_run,
    };

    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(report)
}
