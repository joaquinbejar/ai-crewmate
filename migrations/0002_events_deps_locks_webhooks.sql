-- ai-crewmate :: v0.2 — real-time events, task dependencies, generic
-- locks and outgoing webhooks.

-- ------------------------------------------------------- task dependencies --

CREATE TABLE task_deps (
    task_id             UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    blocked_by_task_id  UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, blocked_by_task_id),
    CHECK (task_id <> blocked_by_task_id)
);

CREATE INDEX task_deps_blocked_by_idx ON task_deps (blocked_by_task_id);

-- ----------------------------------------------------------- generic locks --
--
-- Lighter than a task: an advisory mutex on an arbitrary resource name
-- ("deploy:staging", "schema:users"). Expired locks are simply overwritten by
-- the next acquirer.

CREATE TABLE locks (
    team_id          UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    holder_agent_id  UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    purpose          TEXT,
    acquired_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at       TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (team_id, name)
);

CREATE INDEX locks_expires_idx ON locks (expires_at);

-- ------------------------------------------------------- outgoing webhooks --

CREATE TABLE webhooks (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id         UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    url             TEXT NOT NULL,
    -- payload format: slack {text}, discord {content}, generic (raw event)
    kind            TEXT NOT NULL CHECK (kind IN ('slack', 'discord', 'generic')),
    -- which event kinds to forward
    events          TEXT[] NOT NULL DEFAULT '{message,task}',
    -- for message events: only forward this channel (NULL = all channels)
    channel_filter  TEXT,
    enabled         BOOLEAN NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX webhooks_team_idx ON webhooks (team_id);

-- --------------------------------------------------------- NOTIFY triggers --
--
-- Every mutation the team cares about lands on the single Postgres channel
-- 'bus_events' as a small JSON payload (ids only — listeners resolve names).
-- The server keeps one LISTEN connection and fans out to in-process
-- subscribers: wait_for_updates long-polls and the webhook dispatcher.

CREATE FUNCTION notify_message() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'message',
        'team_id', NEW.team_id,
        'id', NEW.id,
        'channel_id', NEW.channel_id,
        'recipient_agent_id', NEW.recipient_agent_id,
        'sender_agent_id', NEW.sender_agent_id
    )::text);
    RETURN NULL;
END $$;

CREATE TRIGGER messages_notify
    AFTER INSERT ON messages
    FOR EACH ROW EXECUTE FUNCTION notify_message();

CREATE FUNCTION notify_task() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    -- Suppress no-op updates (e.g. lease renewals change only timestamps but
    -- still fire; that is acceptable — status is what listeners key on).
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'task',
        'team_id', NEW.team_id,
        'key', NEW.key,
        'status', NEW.status,
        'claimed_by', NEW.claimed_by
    )::text);
    RETURN NULL;
END $$;

CREATE TRIGGER tasks_notify
    AFTER INSERT OR UPDATE ON tasks
    FOR EACH ROW EXECUTE FUNCTION notify_task();

CREATE FUNCTION notify_lock() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM pg_notify('bus_events', json_build_object(
            'kind', 'lock',
            'event', 'released',
            'team_id', OLD.team_id,
            'name', OLD.name
        )::text);
        RETURN NULL;
    END IF;
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'lock',
        'event', 'acquired',
        'team_id', NEW.team_id,
        'name', NEW.name,
        'holder_agent_id', NEW.holder_agent_id
    )::text);
    RETURN NULL;
END $$;

CREATE TRIGGER locks_notify
    AFTER INSERT OR UPDATE OR DELETE ON locks
    FOR EACH ROW EXECUTE FUNCTION notify_lock();

CREATE FUNCTION notify_note() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'note',
        'team_id', NEW.team_id,
        'scope', NEW.scope,
        'key', NEW.key,
        'updated_by', NEW.updated_by
    )::text);
    RETURN NULL;
END $$;

CREATE TRIGGER notes_notify
    AFTER INSERT OR UPDATE ON notes
    FOR EACH ROW EXECUTE FUNCTION notify_note();
