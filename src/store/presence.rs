use sqlx::PgPool;

use crate::{
    auth::AuthCtx,
    error::{BusError, BusResult},
    model::{AgentInfo, AgentList, AgentSession, ts_opt},
};

const DEFAULT_TTL_SECS: i64 = 600; // 10 minutes
const MAX_TTL_SECS: i64 = 86_400;
/// Repo, branch and activity are a status line, not a log.
const MAX_PRESENCE_FIELD_BYTES: usize = 256;

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

    // Presence is a status line, not a log: bounded so a heartbeat loop
    // cannot grow the row without limit.
    let repo = match input.repo.as_deref() {
        Some(v) => Some(super::check_text(
            "presence repo",
            v,
            MAX_PRESENCE_FIELD_BYTES,
        )?),
        None => None,
    };
    let branch = match input.branch.as_deref() {
        Some(v) => Some(super::check_text(
            "presence branch",
            v,
            MAX_PRESENCE_FIELD_BYTES,
        )?),
        None => None,
    };
    let activity = match input.activity.as_deref() {
        Some(v) => Some(super::check_text(
            "presence activity",
            v,
            MAX_PRESENCE_FIELD_BYTES,
        )?),
        None => None,
    };

    // Upsert on (agent_id, session), and report back the row just written.
    // Reading it from list_agents instead would pick whichever session came
    // first alphabetically once an agent has more than one.
    let row: (
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    ) = sqlx::query_as(
        r#"
        WITH up AS (
            INSERT INTO agent_presence
                (agent_id, session, status, repo, branch, activity, updated_at, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, now(), now() + make_interval(secs => $7))
            ON CONFLICT (agent_id, session) DO UPDATE SET
                status     = EXCLUDED.status,
                -- keep the previous value when the caller omits a field
                repo       = COALESCE(EXCLUDED.repo, agent_presence.repo),
                branch     = COALESCE(EXCLUDED.branch, agent_presence.branch),
                activity   = COALESCE(EXCLUDED.activity, agent_presence.activity),
                updated_at = now(),
                expires_at = EXCLUDED.expires_at
            RETURNING agent_id, status, repo, branch, activity, updated_at, expires_at
        )
        SELECT a.name,
               a.display_name,
               up.status,
               up.repo,
               up.branch,
               up.activity,
               up.updated_at,
               up.expires_at > now() AS online
        FROM up
        JOIN agents a ON a.id = up.agent_id
        "#,
    )
    .bind(auth.agent_id)
    .bind(&auth.session)
    .bind(&status)
    .bind(repo.as_deref())
    .bind(branch.as_deref())
    .bind(activity.as_deref())
    .bind(ttl as f64)
    .fetch_one(pool)
    .await?;

    let (name, display_name, status, repo, branch, activity, updated_at, online) = row;
    Ok(AgentInfo {
        name,
        display_name,
        session: super::session_label(auth),
        status,
        repo,
        branch,
        activity,
        last_seen: ts_opt(updated_at),
        online,
        // The heartbeat reports the session it just wrote, not a survey of the
        // agent's other contexts; list_agents is where that belongs.
        sessions: Vec::new(),
    })
}

pub async fn list_agents(pool: &PgPool, auth: &AuthCtx, online_only: bool) -> BusResult<AgentList> {
    // One row per (agent, session). An agent working in three repositories has
    // three presence rows and is still one person, so the rows are folded back
    // into one entry per agent below.
    let rows: Vec<(
        String,
        Option<String>,
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
               p.session,
               p.status,
               p.repo,
               p.branch,
               p.activity,
               p.updated_at,
               COALESCE(p.expires_at > now(), false) AS online
        FROM agents a
        -- One presence row per agent, not one per session. An agent can now
        -- have several, and joining on agent_id alone would emit a duplicate
        -- entry per session and inflate online_count with them. Reporting the
        -- sessions properly, grouped under their agent, is the next change in
        -- the stack; until then this picks the one that matters — live first,
        -- then most recent — so the output stays exactly the shape every
        -- existing client parses.
        LEFT JOIN LATERAL (
            SELECT pp.status, pp.repo, pp.branch, pp.activity, pp.updated_at, pp.expires_at
            FROM agent_presence pp
            WHERE pp.agent_id = a.id
            ORDER BY (pp.expires_at > now()) DESC, pp.updated_at DESC
            LIMIT 1
        ) p ON true
        WHERE a.team_id = $1
          AND a.disabled_at IS NULL
          AND (NOT $2::bool OR COALESCE(p.expires_at > now(), false))
        -- Within an agent: live sessions first, then the most recent.
        ORDER BY a.name,
                 COALESCE(p.expires_at > now(), false) DESC,
                 p.updated_at DESC NULLS LAST
        "#,
    )
    .bind(auth.team_id)
    .bind(online_only)
    .fetch_all(pool)
    .await?;

    // Rows arrive grouped by agent and already in the order sessions should be
    // reported in, so one pass is enough.
    let mut agents: Vec<AgentInfo> = Vec::new();
    for (name, display_name, session, status, repo, branch, activity, updated_at, online) in rows {
        let entry = AgentSession {
            // '' in the database, null on the wire: the shared session has no
            // name, and reporting one would invent a context that is not there.
            session: session.filter(|s| !s.is_empty()),
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
        };

        match agents.last_mut() {
            // The lead session — the first row for this agent — is the one the
            // top-level fields describe.
            Some(agent) if agent.name == name => {
                agent.online |= entry.online;
                agent.sessions.push(entry);
            }
            _ => agents.push(AgentInfo {
                name,
                display_name,
                session: entry.session.clone(),
                status: entry.status.clone(),
                repo: entry.repo.clone(),
                branch: entry.branch.clone(),
                activity: entry.activity.clone(),
                last_seen: entry.last_seen.clone(),
                online: entry.online,
                sessions: vec![entry],
            }),
        }
    }

    // With a single session the top-level fields say everything; repeating it
    // as a one-element list is noise, and hiding it keeps the output identical
    // to what every existing client already parses.
    for agent in &mut agents {
        if agent.sessions.len() < 2 {
            agent.sessions.clear();
        }
    }

    // People, not sessions: someone with three live sessions is one teammate
    // online.
    let online_count = agents.iter().filter(|a| a.online).count();
    // Online agents first, as before; the SQL ordered by name so that grouping
    // could be a single pass.
    agents.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    Ok(AgentList {
        agents,
        online_count,
    })
}
