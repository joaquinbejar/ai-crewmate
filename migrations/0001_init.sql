-- ai-crewmate :: initial schema
--
-- Model:
--   team    -> a group of people sharing one bus (usually one squad / one company)
--   agent   -> one Claude Code instance (normally "person + machine"), belongs to a team
--   token   -> bearer credential that identifies exactly one agent
--
-- Everything else (messages, tasks, notes) is scoped by team_id, so a single
-- deployment can host several teams without leaking data between them.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------------------------------------------------------------- identity --

CREATE TABLE teams (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE agents (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id       UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- stable handle used in tool arguments, e.g. "joaquin-laptop"
    name          TEXT NOT NULL,
    display_name  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at   TIMESTAMPTZ,
    UNIQUE (team_id, name)
);

CREATE INDEX agents_team_idx ON agents (team_id);

CREATE TABLE api_tokens (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id      UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    -- sha256 of the raw token; the raw value is shown exactly once at creation
    token_hash    BYTEA NOT NULL UNIQUE,
    -- first characters of the raw token, for display in `token list`
    prefix        TEXT NOT NULL,
    label         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);

CREATE INDEX api_tokens_agent_idx ON api_tokens (agent_id);

-- ---------------------------------------------------------------- presence --

CREATE TABLE agent_presence (
    agent_id    UUID PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    status      TEXT NOT NULL DEFAULT 'active',
    repo        TEXT,
    branch      TEXT,
    activity    TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX agent_presence_expires_idx ON agent_presence (expires_at);

-- --------------------------------------------------------------- messaging --

CREATE TABLE channels (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id     UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    topic       TEXT,
    created_by  UUID REFERENCES agents(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (team_id, name)
);

CREATE TABLE messages (
    id                  BIGSERIAL PRIMARY KEY,
    team_id             UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- exactly one of channel_id / recipient_agent_id is set
    channel_id          UUID REFERENCES channels(id) ON DELETE CASCADE,
    recipient_agent_id  UUID REFERENCES agents(id) ON DELETE CASCADE,
    sender_agent_id     UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    body                TEXT NOT NULL,
    reply_to            BIGINT REFERENCES messages(id) ON DELETE SET NULL,
    metadata            JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT messages_target_check CHECK (
        (channel_id IS NOT NULL AND recipient_agent_id IS NULL)
        OR (channel_id IS NULL AND recipient_agent_id IS NOT NULL)
    )
);

CREATE INDEX messages_channel_idx ON messages (channel_id, id DESC);
CREATE INDEX messages_dm_idx ON messages (recipient_agent_id, id DESC);
CREATE INDEX messages_sender_idx ON messages (sender_agent_id, id DESC);
CREATE INDEX messages_team_idx ON messages (team_id, id DESC);
CREATE INDEX messages_fts_idx ON messages
    USING GIN (to_tsvector('simple', body));

-- Per-agent read position. `scope` is 'dm' or 'channel:<uuid>'.
CREATE TABLE read_cursors (
    agent_id         UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    scope            TEXT NOT NULL,
    last_message_id  BIGINT NOT NULL DEFAULT 0,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, scope)
);

-- ------------------------------------------------------------------- tasks --

CREATE TABLE tasks (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id           UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- human-readable stable id, e.g. "refactor-auth" or "repo:api#421"
    key               TEXT NOT NULL,
    title             TEXT NOT NULL,
    description       TEXT,
    status            TEXT NOT NULL DEFAULT 'open',
    claimed_by        UUID REFERENCES agents(id) ON DELETE SET NULL,
    claimed_at        TIMESTAMPTZ,
    -- advisory lock expiry: a claim whose lease has expired can be stolen
    lease_expires_at  TIMESTAMPTZ,
    result            TEXT,
    metadata          JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_by        UUID REFERENCES agents(id) ON DELETE SET NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (team_id, key),
    CONSTRAINT tasks_status_check
        CHECK (status IN ('open', 'claimed', 'done', 'cancelled'))
);

CREATE INDEX tasks_team_status_idx ON tasks (team_id, status);
CREATE INDEX tasks_claimed_by_idx ON tasks (claimed_by);

CREATE TABLE task_events (
    id          BIGSERIAL PRIMARY KEY,
    task_id     UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    agent_id    UUID REFERENCES agents(id) ON DELETE SET NULL,
    event       TEXT NOT NULL,
    detail      TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX task_events_task_idx ON task_events (task_id, id DESC);

-- ------------------------------------------------------------------- notes --

CREATE TABLE notes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id     UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- free-form namespace, typically a repo or project name; 'global' by default
    scope       TEXT NOT NULL DEFAULT 'global',
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    updated_by  UUID REFERENCES agents(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (team_id, scope, key)
);

CREATE INDEX notes_team_scope_idx ON notes (team_id, scope);
CREATE INDEX notes_tags_idx ON notes USING GIN (tags);
CREATE INDEX notes_fts_idx ON notes
    USING GIN (to_tsvector('simple', key || ' ' || value));

CREATE TABLE note_revisions (
    id          BIGSERIAL PRIMARY KEY,
    note_id     UUID NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    value       TEXT NOT NULL,
    updated_by  UUID REFERENCES agents(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX note_revisions_note_idx ON note_revisions (note_id, id DESC);
