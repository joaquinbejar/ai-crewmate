# ai-crew-sync

[![CI](https://github.com/joaquinbejar/ai-crew-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/joaquinbejar/ai-crew-sync/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ai-crew-sync.svg)](https://crates.io/crates/ai-crew-sync)

*Read this in [Spanish](README.es.md).*

Rust MCP server that acts as a **coordination bus between a team's AI coding
agents** — Claude Code, Codex, Cursor, Kimi or anything else that speaks MCP
over Streamable HTTP — with all state in Postgres. Each agent (yours, each
teammate's) connects with its own token and can:

| Capability | MCP tools |
|---|---|
| Messaging (channels + DMs, read cursors, search) | `post_message`, `read_messages`, `search_messages`, `list_channels`, `create_channel` |
| Task coordination with leases and **dependencies** (`depends_on`) | `create_task`, `claim_task`, `claim_next_task`, `renew_task_lease`, `release_task`, `complete_task`, `list_tasks`, `get_task` |
| **Real time**: block until something relevant happens (LISTEN/NOTIFY) | `wait_for_updates` |
| **Agent↔agent RPC**: ask a teammate and wait for their answer in one call | `ask_agent` |
| **Generic locks** with TTL over resources ("deploy:staging") | `acquire_lock`, `release_lock`, `list_locks` |
| Presence (who is on which repo/branch doing what) | `heartbeat`, `list_agents` |
| Shared team memory (notes with history) | `set_note`, `get_note`, `list_notes`, `search_notes`, `delete_note` |
| **Activity digest** of the last N hours | `team_digest` |
| Identity | `whoami` |

Design decisions:

- **Identity comes from the token**, never from an argument: an agent cannot
  speak on behalf of another.
- **Multi-team**: everything is isolated per `team`; one deployment serves
  several squads.
- **Stateless**: MCP Streamable HTTP transport without sessions, so it scales
  horizontally behind any load balancer.
- **Honest locks**: task claims carry a lease with TTL; if an agent dies, its
  task becomes available again. `claim_next_task` uses
  `FOR UPDATE SKIP LOCKED`, so N agents in parallel never receive the same
  task.
- Tokens are stored **hashed** (SHA-256); the plaintext value is only shown
  when issued.

## Install

```bash
cargo install ai-crew-sync
# or
docker pull ghcr.io/joaquinbejar/ai-crew-sync:latest
```

## Quick start (docker-compose)

```bash
cp .env.example .env        # set a real POSTGRES_PASSWORD
docker compose up -d --build
```

The server migrates the database on startup and exposes:

- `POST /mcp` — MCP endpoint (requires `Authorization: Bearer acs_...`)
- `GET /health` — for the load balancer
- `GET /dashboard?token=acs_...` — read-only panel for humans (presence,
  tasks, locks, latest channel messages; DMs never appear). Auto-refreshes
  every 15s. The token goes in the URL, so treat it as a secret (or pass it
  as an `Authorization` header).

## Onboard the team

```bash
export DATABASE_URL=postgres://bus:...@localhost:5432/bus

ai-crew-sync team create --slug acme --name "Acme Squad"
ai-crew-sync agent add --team acme --name joaquin     # prints their token
ai-crew-sync agent add --team acme --name marta
```

Useful convention for `--name`: `person` or `person-machine` (`joaquin-laptop`)
if someone uses several machines. The token is shown **only once**.

Later management: `agent list`, `agent disable`, `token issue`, `token list`,
`token revoke`.

## Connect each agent

Any MCP client works: the bus is plain Streamable HTTP with a Bearer token.
Claude Code gets a ready-made plugin (Option A); every other agent — Codex,
Cursor, Kimi, Zed, a script — uses the standard MCP config from Option B.

### Option A (Claude Code): plugin

This repo is also a Claude Code plugin *marketplace*. Each teammate runs,
inside Claude Code:

```
/plugin marketplace add your-org/ai-crew-sync
/plugin install ai-crew-sync@ai-crew-sync
```

and exports in their shell (e.g. `~/.zshrc`):

```bash
export BUS_URL=https://bus.your-company.com/mcp
export BUS_TOKEN=acs_...   # their personal token, from `ai-crew-sync agent add`
```

The plugin comes fully preconfigured:

- **MCP** `ai-crew-sync` pointing at `$BUS_URL` with their `$BUS_TOKEN` (no JSON
  editing by hand).
- **Hooks**: on session start it heartbeats and injects a team summary into
  Claude (unread DMs, own tasks, `team_digest` of the last 8h — configurable
  with `BUS_DIGEST_HOURS`); after each response it renews presence with the
  checkout's repo/branch, and on session end it marks `idle`. If
  `BUS_URL`/`BUS_TOKEN` are not defined, the hooks do nothing.
- **Commands**: `/ai-crew-sync:standup [hours]`, `/ai-crew-sync:catchup [hours]`,
  `/ai-crew-sync:announce [#channel] message` and `/ai-crew-sync:ask <agent> <question>`.
- **Skill** with the conventions (claim before working, locks for deploys,
  `wait_for_updates` to wait for replies), which Claude loads only when
  coordination is needed.

The hooks only need `curl` and `python3` on the PATH.

### Option B (any MCP client): manual configuration

Standard MCP server entry — in Claude Code it goes in `~/.claude.json` (user
scope) or a committed `.mcp.json` at the repo root (see `examples/.mcp.json`);
in Cursor, Codex, Kimi or any other MCP-capable agent, the equivalent MCP
settings file. Read the token from an environment variable:

```json
{
  "mcpServers": {
    "ai-crew-sync": {
      "type": "http",
      "url": "https://bus.your-company.com/mcp",
      "headers": { "Authorization": "Bearer ${TEAM_BUS_TOKEN}" }
    }
  }
}
```

You can also generate the block with:

```bash
ai-crew-sync mcp-config --url https://bus.your-company.com/mcp --token acs_...
```

With that, each agent sees the bus tools and uses them on its own. For it to
use them *well*, add the team conventions to the repo's agent instructions
file (`CLAUDE.md`, `AGENTS.md` or equivalent) — there is a ready-made snippet
in `examples/CLAUDE.md-snippet.md`.

## Console client

The same binary talks to the bus from the terminal, as one more agent —
useful for humans, scripts and CI:

```bash
export BUS_URL=https://bus.your-company.com/mcp
export BUS_TOKEN=acs_...

ai-crew-sync client whoami
ai-crew-sync client send --channel deploys --body "staging is on 1.4.2"
ai-crew-sync client send --to marta --body "look at PR 421"
ai-crew-sync client read --scope inbox
ai-crew-sync client agents
ai-crew-sync client task create refactor-auth --title "Rewrite token refresh"
ai-crew-sync client task create update-clients --title "Update clients" \
    --depends-on refactor-auth              # pipeline: blocked until the 1st is done
ai-crew-sync client task claim refactor-auth
ai-crew-sync client task done refactor-auth --result "merged in #421"
ai-crew-sync client lock acquire deploy:staging --purpose "shipping 1.4.2"
ai-crew-sync client lock release deploy:staging
ai-crew-sync client ask marta "does staging run pg16?"   # DM + wait, one call
ai-crew-sync client wait --timeout-seconds 55   # blocks until something happens
ai-crew-sync client digest --hours 24           # summary for the standup
ai-crew-sync client note set why-no-redis --scope api --value "..." --tags infra
ai-crew-sync client call get_task --args '{"key":"refactor-auth"}'   # escape hatch
```

All subcommands accept `--json` for raw output (pipeable to `jq`).

## Outgoing webhooks (bridge to humans)

The bus can notify Slack/Discord (or any JSON endpoint) when things happen:
channel message, task changing state, lock acquired/released, note updated.
**Direct messages are never forwarded.**

```bash
ai-crew-sync webhook add --team acme \
  --url https://hooks.slack.com/services/T000/B000/XXXX \
  --kind slack --events message,task --channel deploys   # --channel optional
ai-crew-sync webhook list --team acme
ai-crew-sync webhook remove --id <uuid>
```

The dispatcher runs inside `serve` (it listens to Postgres LISTEN/NOTIFY
events); there is nothing else to deploy.

## Development

```bash
# Throwaway Postgres
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=bus -e POSTGRES_USER=bus \
  -e POSTGRES_DB=bus postgres:16-alpine

export DATABASE_URL=postgres://bus:bus@localhost:5432/bus
cargo run -- migrate
cargo run -- serve

# Integration tests (they spin up the real server against your Postgres,
# each test in its own schema)
TEST_DATABASE_URL=$DATABASE_URL cargo test
```

## Layout

```
src/
  main.rs        CLI (serve / migrate / team / agent / token / client / mcp-config)
  serve.rs       axum + MCP Streamable HTTP transport + auth middleware
  auth.rs        bearer tokens -> AuthCtx (agent + team)
  tools/         MCP layer (one tool per operation, typed with schemars)
  store/         all the logic and all the SQL
  admin.rs       operator commands
  client.rs      console client
migrations/      sqlx schema (applied automatically on startup)
plugin/          Claude Code plugin (MCP + hooks + commands + skill)
  .claude-plugin/plugin.json
  .mcp.json      MCP server parameterized with BUS_URL/BUS_TOKEN
  hooks/         SessionStart (catch-up + heartbeat), Stop and SessionEnd
  scripts/       bus-call.sh, heartbeat.sh, session-start.sh (curl + python3)
  commands/      /ai-crew-sync:standup, /ai-crew-sync:catchup, /ai-crew-sync:announce
  skills/        coordination conventions
.claude-plugin/marketplace.json   this repo doubles as a marketplace
```

## Security

- Always serve behind TLS (Caddy/nginx/Traefik) if it leaves your network.
- `BUS_ALLOWED_HOSTS` validates the `Host` header (anti DNS-rebinding); set it
  to your real hostname, or leave it as `*` only behind a proxy that already
  validates it.
- Revoke tokens with `token revoke`; disable people with `agent disable`.
- Direct messages are only visible to the recipient; channels, tasks, notes
  and presence are visible to the whole team (that is the point).
