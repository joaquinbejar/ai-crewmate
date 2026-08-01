//! Operator-facing commands. These bypass MCP entirely and talk to Postgres
//! directly, so they are the only way to mint credentials.

use anyhow::{Context, bail};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{generate_token, hash_token, token_prefix};

async fn team_id(pool: &PgPool, slug: &str) -> anyhow::Result<Uuid> {
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM teams WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await?;
    row.map(|r| r.0)
        .with_context(|| format!("no team with slug '{slug}'"))
}

pub async fn team_create(pool: &PgPool, slug: &str, name: Option<String>) -> anyhow::Result<()> {
    let slug = slug.trim().to_lowercase();
    if slug.is_empty() {
        bail!("team slug cannot be empty");
    }
    let name = name.unwrap_or_else(|| slug.clone());
    sqlx::query("INSERT INTO teams (slug, name) VALUES ($1, $2) ON CONFLICT (slug) DO NOTHING")
        .bind(&slug)
        .bind(&name)
        .execute(pool)
        .await?;
    println!("team '{slug}' ready");
    Ok(())
}

pub async fn team_list(pool: &PgPool) -> anyhow::Result<()> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        r#"
        SELECT t.slug, t.name, (SELECT count(*) FROM agents a WHERE a.team_id = t.id)
        FROM teams t ORDER BY t.slug
        "#,
    )
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        println!("(no teams yet — create one with `team create --slug <slug>`)");
    }
    for (slug, name, agents) in rows {
        println!("{slug:<20} {name:<30} {agents} agent(s)");
    }
    Ok(())
}

pub async fn agent_add(
    pool: &PgPool,
    team: &str,
    name: &str,
    display_name: Option<String>,
    issue_token: bool,
) -> anyhow::Result<()> {
    let tid = team_id(pool, team).await?;
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        bail!("agent name cannot be empty");
    }

    sqlx::query(
        r#"
        INSERT INTO agents (team_id, name, display_name)
        VALUES ($1, $2, $3)
        ON CONFLICT (team_id, name) DO UPDATE
            SET display_name = COALESCE(EXCLUDED.display_name, agents.display_name),
                disabled_at = NULL
        "#,
    )
    .bind(tid)
    .bind(&name)
    .bind(display_name)
    .execute(pool)
    .await?;
    println!("agent '{name}' ready in team '{team}'");

    if issue_token {
        token_issue(pool, team, &name, None).await?;
    }
    Ok(())
}

pub async fn agent_list(pool: &PgPool, team: &str) -> anyhow::Result<()> {
    let tid = team_id(pool, team).await?;
    let rows: Vec<(String, Option<String>, bool, i64)> = sqlx::query_as(
        r#"
        SELECT a.name,
               a.display_name,
               (a.disabled_at IS NOT NULL) AS disabled,
               (SELECT count(*) FROM api_tokens t
                 WHERE t.agent_id = a.id AND t.revoked_at IS NULL)
        FROM agents a WHERE a.team_id = $1 ORDER BY a.name
        "#,
    )
    .bind(tid)
    .fetch_all(pool)
    .await?;
    for (name, display, disabled, tokens) in rows {
        let flag = if disabled { " [disabled]" } else { "" };
        println!(
            "{name:<24} {:<28} {tokens} active token(s){flag}",
            display.unwrap_or_default()
        );
    }
    Ok(())
}

pub async fn agent_disable(pool: &PgPool, team: &str, name: &str) -> anyhow::Result<()> {
    let tid = team_id(pool, team).await?;
    let res = sqlx::query("UPDATE agents SET disabled_at = now() WHERE team_id = $1 AND name = $2")
        .bind(tid)
        .bind(name.trim())
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        bail!("no agent '{name}' in team '{team}'");
    }
    println!("agent '{name}' disabled; its tokens no longer authenticate");
    Ok(())
}

pub async fn token_issue(
    pool: &PgPool,
    team: &str,
    agent: &str,
    label: Option<String>,
) -> anyhow::Result<()> {
    let tid = team_id(pool, team).await?;
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM agents WHERE team_id = $1 AND name = $2")
            .bind(tid)
            .bind(agent.trim())
            .fetch_optional(pool)
            .await?;
    let Some((agent_id,)) = row else {
        bail!("no agent '{agent}' in team '{team}' — add it with `agent add` first");
    };

    let raw = generate_token();
    sqlx::query(
        "INSERT INTO api_tokens (agent_id, token_hash, prefix, label) VALUES ($1, $2, $3, $4)",
    )
    .bind(agent_id)
    .bind(hash_token(&raw))
    .bind(token_prefix(&raw))
    .bind(label)
    .execute(pool)
    .await?;

    println!();
    println!("Token for {agent}@{team} — shown once, store it now:");
    println!();
    println!("  {raw}");
    println!();
    Ok(())
}

pub async fn token_list(pool: &PgPool, team: &str) -> anyhow::Result<()> {
    let tid = team_id(pool, team).await?;
    let rows: Vec<(
        Uuid,
        String,
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    )> = sqlx::query_as(
        r#"
        SELECT t.id, a.name, t.prefix, t.label, t.last_used_at,
               (t.revoked_at IS NOT NULL) AS revoked
        FROM api_tokens t
        JOIN agents a ON a.id = t.agent_id
        WHERE a.team_id = $1
        ORDER BY a.name, t.created_at
        "#,
    )
    .bind(tid)
    .fetch_all(pool)
    .await?;
    for (id, agent, prefix, label, last_used, revoked) in rows {
        let used = last_used
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_else(|| "never".into());
        let flag = if revoked { " [revoked]" } else { "" };
        println!(
            "{id}  {agent:<20} {prefix}…  last used {used}  {}{flag}",
            label.unwrap_or_default()
        );
    }
    Ok(())
}

pub async fn token_revoke(pool: &PgPool, id: Uuid) -> anyhow::Result<()> {
    let res = sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        bail!("no token with id {id}");
    }
    println!("token {id} revoked");
    Ok(())
}

/// Print the exact `.mcp.json` block a teammate drops into their repo.
pub fn print_mcp_config(url: &str, token: &str) {
    let cfg = serde_json::json!({
        "mcpServers": {
            "ai-crew-sync": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") }
            }
        }
    });
    println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
}
