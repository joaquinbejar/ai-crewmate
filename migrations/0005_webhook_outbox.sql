-- ai-crew-sync :: v0.5 — durable, replica-safe webhook delivery.
--
-- Before this, every replica received the same NOTIFY and every replica ran
-- its own dispatcher, so one channel message became N HTTP POSTs on an
-- N-replica deployment — and the compose file explicitly supports N > 1.
-- Failures were equally unhandled: a timeout, a 500, a listener reconnect or a
-- lagged broadcast dropped the event permanently, with only a WARN behind.
--
-- The fix is an outbox. Rows are enqueued by a database trigger, which fires
-- once per change no matter how many processes are listening, and replicas
-- claim work with FOR UPDATE SKIP LOCKED — the same primitive task claims
-- already use. Delivery becomes at-least-once with bounded retries instead of
-- best-effort-and-forget.

CREATE TABLE webhook_deliveries (
    id               BIGSERIAL PRIMARY KEY,
    webhook_id       UUID NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    team_id          UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    kind             TEXT NOT NULL,
    -- Rendered at enqueue time, from the row as it was when the event
    -- happened. Re-reading at send time would report later state.
    summary          TEXT NOT NULL,
    payload          JSONB NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'sent', 'failed')),
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error       TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The claim query's index: only pending rows, ordered by when they are due.
CREATE INDEX webhook_deliveries_claimable_idx
    ON webhook_deliveries (next_attempt_at, id)
    WHERE status = 'pending';

CREATE INDEX webhook_deliveries_team_idx ON webhook_deliveries (team_id, created_at DESC);

-- --------------------------------------------------------------- fan-out --

-- One row per (event, matching webhook). Enabled hooks of the right team that
-- subscribe to this kind; the channel filter narrows message events only.
CREATE FUNCTION enqueue_webhook_deliveries(
    p_team_id UUID,
    p_kind    TEXT,
    p_summary TEXT,
    p_payload JSONB,
    p_channel TEXT
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO webhook_deliveries (webhook_id, team_id, kind, summary, payload)
    SELECT w.id, p_team_id, p_kind, p_summary, p_payload
    FROM webhooks w
    WHERE w.team_id = p_team_id
      AND w.enabled
      AND p_kind = ANY(w.events)
      AND (p_kind <> 'message' OR w.channel_filter IS NULL OR w.channel_filter = p_channel);

    IF FOUND THEN
        -- Wake whichever replica is idle; the poll interval is the backstop.
        PERFORM pg_notify('webhook_pending', '');
    END IF;
END $$;

-- Direct messages never leave the bus. The guard lives here, in the only
-- place that can enqueue one, rather than in each consumer.
CREATE FUNCTION enqueue_message_webhook() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    v_sender  TEXT;
    v_channel TEXT;
BEGIN
    IF NEW.recipient_agent_id IS NOT NULL THEN
        RETURN NEW;  -- a DM
    END IF;

    SELECT name INTO v_sender FROM agents WHERE id = NEW.sender_agent_id;
    SELECT name INTO v_channel FROM channels WHERE id = NEW.channel_id;

    PERFORM enqueue_webhook_deliveries(
        NEW.team_id,
        'message',
        format('#%s %s: %s', v_channel, v_sender, left(NEW.body, 500)),
        jsonb_build_object(
            'kind', 'message',
            'channel', v_channel,
            'from', v_sender,
            'body', left(NEW.body, 500)
        ),
        v_channel
    );
    RETURN NEW;
END $$;

CREATE TRIGGER messages_enqueue_webhook
    AFTER INSERT ON messages
    FOR EACH ROW EXECUTE FUNCTION enqueue_message_webhook();

CREATE FUNCTION enqueue_task_webhook() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    v_holder TEXT;
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.status = NEW.status THEN
        RETURN NEW;  -- a lease renewal is not a state change
    END IF;

    SELECT name INTO v_holder FROM agents WHERE id = NEW.claimed_by;

    PERFORM enqueue_webhook_deliveries(
        NEW.team_id,
        'task',
        format('task %s is now %s%s', NEW.key, NEW.status,
               COALESCE(' by ' || v_holder, '')),
        jsonb_build_object(
            'kind', 'task',
            'key', NEW.key,
            'title', NEW.title,
            'status', NEW.status,
            'claimed_by', v_holder
        ),
        NULL
    );
    RETURN NEW;
END $$;

CREATE TRIGGER tasks_enqueue_webhook
    AFTER INSERT OR UPDATE ON tasks
    FOR EACH ROW EXECUTE FUNCTION enqueue_task_webhook();

CREATE FUNCTION enqueue_lock_webhook() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    v_row    RECORD;
    v_event  TEXT;
    v_holder TEXT;
BEGIN
    IF TG_OP = 'DELETE' THEN
        v_row := OLD;
        v_event := 'released';
    ELSE
        v_row := NEW;
        v_event := 'acquired';
    END IF;

    SELECT name INTO v_holder FROM agents WHERE id = v_row.holder_agent_id;

    PERFORM enqueue_webhook_deliveries(
        v_row.team_id,
        'lock',
        format('lock %s %s%s', v_row.name, v_event, COALESCE(' by ' || v_holder, '')),
        jsonb_build_object(
            'kind', 'lock',
            'name', v_row.name,
            'event', v_event,
            'holder', v_holder
        ),
        NULL
    );
    RETURN v_row;
END $$;

CREATE TRIGGER locks_enqueue_webhook
    AFTER INSERT OR DELETE ON locks
    FOR EACH ROW EXECUTE FUNCTION enqueue_lock_webhook();

CREATE FUNCTION enqueue_note_webhook() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM enqueue_webhook_deliveries(
        NEW.team_id,
        'note',
        format('note %s/%s was updated', NEW.scope, NEW.key),
        jsonb_build_object('kind', 'note', 'scope', NEW.scope, 'key', NEW.key),
        NULL
    );
    RETURN NEW;
END $$;

CREATE TRIGGER notes_enqueue_webhook
    AFTER INSERT OR UPDATE ON notes
    FOR EACH ROW EXECUTE FUNCTION enqueue_note_webhook();
