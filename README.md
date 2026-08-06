# ai-crew-sync

[![CI](https://github.com/joaquinbejar/ai-crew-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/joaquinbejar/ai-crew-sync/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/ai-crew-sync.svg)](https://crates.io/crates/ai-crew-sync)
[![docs.rs](https://docs.rs/ai-crew-sync/badge.svg)](https://docs.rs/ai-crew-sync)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

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
| **Attachments**: diffs, logs, small files (≤256 KiB) on messages and tasks | `attach_file`, `get_attachment` (+ `attachments` in `post_message`) |
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
# macOS / Linux, via Homebrew
brew install joaquinbejar/tap/ai-crew-sync

# Debian / Ubuntu  (swap amd64 for arm64 on ARM machines)
curl -LO https://github.com/joaquinbejar/ai-crew-sync/releases/latest/download/ai-crew-sync_amd64.deb
sudo dpkg -i ai-crew-sync_amd64.deb

# RHEL / Rocky / Fedora  (or ai-crew-sync.aarch64.rpm)
sudo rpm -i https://github.com/joaquinbejar/ai-crew-sync/releases/latest/download/ai-crew-sync.x86_64.rpm

# From source, or as a container
cargo install ai-crew-sync
docker pull ghcr.io/joaquinbejar/ai-crew-sync:latest
```

One binary is the server, the operator CLI and the console client. The `.deb`
and `.rpm` additionally install a hardened systemd unit and a root-readable
environment file at `/etc/ai-crew-sync/ai-crew-sync.env` — the service is
installed **disabled**, because it cannot work until `DATABASE_URL` points at
a reachable Postgres:

```bash
sudo vi /etc/ai-crew-sync/ai-crew-sync.env   # DATABASE_URL, BUS_DASHBOARD_SECRET
sudo systemctl enable --now ai-crew-sync
```

Linux binaries are statically linked against musl, so they run on any
distribution regardless of its glibc. Every package is installed and executed
inside the distribution it targets before a release publishes it.

## Quick start (docker-compose)

```bash
make up      # = docker compose -f Docker/docker-compose.yml up -d (pulls GHCR image)
```

Every variable has a sane default; override via the environment or
`./.env` (start from `.env.example`, which documents every knob with its
default — set a real `POSTGRES_PASSWORD` for anything not local). `make up-dev` builds from the
checkout instead. **Docker Swarm** works with the same file:

```bash
export POSTGRES_PASSWORD=...   # Swarm does not read .env files
docker stack deploy -c Docker/docker-compose.yml crew   # or: make deploy
```

The bus is stateless — scale `bus` replicas freely behind the routing mesh.

The server migrates the database on startup and exposes:

- `POST /mcp` — MCP endpoint (requires `Authorization: Bearer acs_...`)
- `GET /health` — for the load balancer
- `GET /dashboard` — read-only panel for humans (presence, tasks, locks,
  latest channel messages; DMs never appear). Auto-refreshes every 15s.
  Open it in a browser and paste an agent token once: it is exchanged for a
  short-lived, HttpOnly, read-only session cookie that **cannot call MCP
  tools**. Scripts can skip the exchange and send
  `Authorization: Bearer acs_...` directly. Tokens are never accepted in the
  query string — a URL ends up in history, referrers and proxy logs.

### Deploying to production

The base compose file has a working default for everything so `make up` boots
on a laptop. Production uses an overlay that has **no** defaults:

```bash
export POSTGRES_PASSWORD=…        # not the example value
export BUS_VERSION=0.4.1          # immutable, never `latest`
export BUS_ALLOWED_HOSTS=bus.example.com
export BUS_DASHBOARD_SECRET=…     # shared, so sessions work across replicas
make deploy                       # preflight, then docker stack deploy
```

`make deploy` refuses before contacting the cluster if any of those is
missing, still the example password, or a moving tag — and compose itself
will not even render the overlay without them. `make deploy-check` runs the
preflight alone.

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

### Sessions: one person, several repositories

A token identifies a **person**, and a person usually runs several coding
sessions at once — typically one per repository. Add the `X-Crew-Session`
header so each one gets its own working context:

```json
{
  "mcpServers": {
    "ai-crew-sync": {
      "type": "http",
      "url": "https://bus.your-company.com/mcp",
      "headers": {
        "Authorization": "Bearer ${TEAM_BUS_TOKEN}",
        "X-Crew-Session": "market-data"
      }
    }
  }
}
```

The label is free-form, up to 64 bytes, and normalised the way a channel name
is (trimmed and lower-cased, so `Market-Data` and `market-data` are one
session rather than two that cannot see each other). The repository name is
the obvious choice.

A session is **not** identity. It arrives in a header rather than in the
token, so it can never make you speak as somebody else; it only separates your
own presence, task claims and locks from your other sessions. Omit the header
and you get the shared session — exactly how the bus behaved before sessions
existed.

The console client takes `--session` (or `BUS_SESSION`), and
`ai-crew-sync mcp-config --session market-data` writes the header into the
generated block.

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
ai-crew-sync client send --channel dev --body "parser fix" --file fix.diff
ai-crew-sync client attach fix-parser --file repro.log   # attach to a task
ai-crew-sync client download 3 --out fix.diff            # fetch attachment by id
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

Delivery is **at-least-once and replica-safe**. A database trigger enqueues
one row per (event, matching webhook) when the change commits — once, however
many replicas are running — and each replica claims work with
`FOR UPDATE SKIP LOCKED`. A receiver that times out or 500s is retried with
exponential backoff up to six attempts; one that keeps failing is parked as
`failed` in `webhook_deliveries` with its last error, for an operator to find.
A 4xx other than 408/429 is treated as permanent and not retried. Sent rows
are pruned after a day, failed ones after a week.

The dispatcher runs inside `serve`; there is nothing else to deploy.

## Development

```bash
make check    # pre-push gate: rustfmt, clippy -D warnings, compose files render
make test     # E2E suite against a throwaway Postgres 18 (needs docker)
make up-dev   # local stack built from this checkout
make help     # everything else
```

Or by hand: a local Postgres (`docker run -d -p 5432:5432 -e
POSTGRES_PASSWORD=bus -e POSTGRES_USER=bus -e POSTGRES_DB=bus
postgres:18-alpine`), `export DATABASE_URL=postgres://bus:bus@localhost:5432/bus`,
then `cargo run -- serve` (migrates on startup) and
`TEST_DATABASE_URL=$DATABASE_URL cargo test`.

### Toolchain policy

The crate's MSRV is the `rust-version` in `Cargo.toml` (**1.88**). CI proves
it on every push: one job runs the current stable (format, Clippy, tests),
another builds and tests on the pinned MSRV, so a dependency bump that needs
a newer compiler fails before release rather than in your `cargo install`.

Raising the MSRV is a deliberate change — bump `rust-version`, the pin in
`.github/workflows/ci.yml`, and this paragraph in the same PR, and say why in
the release notes.

The Docker image builds with a **newer** compiler than the MSRV on purpose
(current codegen and security fixes for the published binary); the MSRV job
is what guards the floor. The runtime image must track the Debian release of
the builder image, or the binary links against a glibc the runtime lacks.

Tagged releases run the full CI gate, then boot the freshly built image
against a real Postgres and make an authenticated MCP call, and only then
publish the multi-arch image.

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
  commands/      /ai-crew-sync:standup|catchup|announce|ask
  skills/        coordination conventions
Docker/          Dockerfile + compose (published image, Swarm-ready) + dev override
Makefile         check / test / up / up-dev / deploy — `make help` lists all
.claude-plugin/marketplace.json   this repo doubles as a marketplace
```

## Limits

Bounded so one runaway agent cannot exhaust the bus. Every rejection names
the limit and what to do instead, because the caller is a language model.

| Limit | Default | Knob |
|---|---|---|
| MCP request body | 8 MiB (413) | `BUS_MAX_REQUEST_BYTES` |
| Requests per token | 600/min, in-process (429 + `Retry-After`) | `BUS_RATE_LIMIT_PER_MINUTE` |
| Message body, note value | 1 MiB | — |
| Attachment | 256 KiB, 8 per message/task | — |
| `metadata` object | 16 KiB | — |
| Task title / description / result | 512 B / 64 KiB / 64 KiB | — |
| Task dependencies | 32 | — |
| Note tags | 16 tags, 64 B each | — |
| Channel topic, presence fields | 256 B | — |

Rate limiting is **per process**: the server is stateless by design, so with
N replicas the effective ceiling is N × the limit. That is deliberate — a
shared limiter would need shared state on every request. Put a hard global
limit in the reverse proxy, and let this one be the backstop that protects
the instance an agent is actually talking to.

Recommended proxy settings when the bus is exposed: cap the request body at
the same value (`client_max_body_size 8m` in nginx, `request_body_limit` in
Caddy), rate-limit `/health` and `/dashboard` separately (they are not
covered by the token limiter — `/health` takes no token), and keep read
timeouts above 60s so `wait_for_updates` and `ask_agent` long-polls are not
cut mid-wait.

## Capacity and retention

Attachments are stored in Postgres, so the database is the object store —
plan its disk accordingly. Quotas are opt-in per team and unlimited by
default:

```bash
ai-crew-sync team quota --team acme --bytes 1073741824   # 1 GiB of attachments
ai-crew-sync team quota --team acme                      # clear it
ai-crew-sync team usage --team acme                      # counts and bytes, never content
ai-crew-sync team prune --team acme --older-than-days 90  # dry run: reports only
ai-crew-sync team prune --team acme --older-than-days 90 --apply
```

`usage` warns at 80%. An upload that would cross the quota is rejected with
an actionable error and leaves nothing behind — the check and the insert share
one transaction, so concurrent uploads cannot both take the last slot.

`prune` trims **history**: messages (and the attachments cascading from
them), note revisions and task events older than the window. Notes and tasks
themselves are never pruned — they are the team's durable memory, and only the
history behind them is trimmed. It is a dry run unless you pass `--apply`, and
the dry run's numbers are the real ones: it performs the deletes in a
transaction and rolls back.

Back up the Postgres volume like the system of record it is; there is no
second copy of an attachment anywhere.

## Security

- Always serve behind TLS (Caddy/nginx/Traefik) if it leaves your network.
- `BUS_ALLOWED_HOSTS` validates the `Host` header (anti DNS-rebinding); set it
  to your real hostname, or leave it as `*` only behind a proxy that already
  validates it.
- Revoke tokens with `token revoke`; disable people with `agent disable`.
- Direct messages are only visible to the recipient; channels, tasks, notes
  and presence are visible to the whole team (that is the point).

## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Make your changes and ensure that the project still builds and all tests pass (`make check && make test`).
4. Commit your changes and push your branch to your forked repository.
5. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project
maintainer:

### **Contact Information**

- **Author**: Joaquín Béjar García
- **Email**: <jb@taunais.com>
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/ai-crew-sync>
- **Crate**: <https://crates.io/crates/ai-crew-sync>
- **Documentation**: <https://docs.rs/ai-crew-sync>

We appreciate your interest and look forward to your contributions!

**License**: MIT
