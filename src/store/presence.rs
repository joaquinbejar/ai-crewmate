use sqlx::PgPool;

use crate::{
    auth::AuthCtx,
    error::{BusError, BusResult},
    model::{AgentInfo, AgentList, ts_opt},
};

const DEFAULT_TTL_SECS: i64 = 600; // 10 minutes
const MAX_TTL_SECS: i64 = 86_400;

pub struct HeartbeatInput {
    pub status: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub activity: Option<String>,
    pub ttl_seconds: Option<i64>,
}

pub async fn heartbeat(
    pool: &PgPool,
    auth: &AuthCtx,
    input: HeartbeatInput,
) -> BusResult<AgentInfo> {
    let status = input
        .status
        .map(|s| s.trim().to_lowercase())
        .unwrap_or_else(|| "active".into());
    if !["active", "idle", "busy", "blocked"].contains(&status.as_str()) {
        return Err(BusError::invalid(
            "status must be one of: active, idle, busy, blocked",
        ));
    }
    let ttl = input
        .ttl_seconds
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(30, MAX_TTL_SECS);

    sqlx::query(
        r#"
        INSERT INTO agent_presence (agent_id, status, repo, branch, activity, updated_at, expires_at)
        VALUES ($1, $2, $3, $4, $5, now(), now() + make_interval(secs => $6))
        ON CONFLICT (agent_id) DO UPDATE SET
            status     = EXCLUDED.status,
            -- keep the previous value when the caller omits a field
            repo       = COALESCE(EXCLUDED.repo, agent_presence.repo),
            branch     = COALESCE(EXCLUDED.branch, agent_presence.branch),
            activity   = COALESCE(EXCLUDED.activity, agent_presence.activity),
            updated_at = now(),
            expires_at = EXCLUDED.expires_at
        "#,
    )
    .bind(auth.agent_id)
    .bind(&status)
    .bind(input.repo.as_deref())
    .bind(input.branch.as_deref())
    .bind(input.activity.as_deref())
    .bind(ttl as f64)
    .execute(pool)
    .await?;

    let list = list_agents(pool, auth, false).await?;
    list.agents
        .into_iter()
        .find(|a| a.name == auth.agent_name)
        .ok_or_else(|| BusError::not_found("own presence row"))
}

pub async fn list_agents(pool: &PgPool, auth: &AuthCtx, online_only: bool) -> BusResult<AgentList> {
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    )> = sqlx::query_as(
        r#"
        SELECT a.name,
               a.display_name,
               p.status,
               p.repo,
               p.branch,
               p.activity,
               p.updated_at,
               COALESCE(p.expires_at > now(), false) AS online
        FROM agents a
        LEFT JOIN agent_presence p ON p.agent_id = a.id
        WHERE a.team_id = $1
          AND a.disabled_at IS NULL
          AND (NOT $2::bool OR COALESCE(p.expires_at > now(), false))
        ORDER BY COALESCE(p.expires_at > now(), false) DESC, a.name
        "#,
    )
    .bind(auth.team_id)
    .bind(online_only)
    .fetch_all(pool)
    .await?;

    let agents: Vec<AgentInfo> = rows
        .into_iter()
        .map(
            |(name, display_name, status, repo, branch, activity, updated_at, online)| AgentInfo {
                name,
                display_name,
                status: if online {
                    status.unwrap_or_else(|| "active".into())
                } else {
                    "offline".into()
                },
                repo,
                branch,
                activity,
                last_seen: ts_opt(updated_at),
                online,
            },
        )
        .collect();

    let online_count = agents.iter().filter(|a| a.online).count();
    Ok(AgentList {
        agents,
        online_count,
    })
}
