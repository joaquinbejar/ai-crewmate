-- ai-crew-sync :: v0.5 — enforce team isolation in the database, not only in
-- the queries.
--
-- Every team-scoped table stores team_id next to foreign keys pointing at
-- agents, channels, tasks and messages, but nothing stopped those two from
-- disagreeing: an application bug, an operator's UPDATE or a future migration
-- could attach team A's message to team B's channel, and the schema would
-- accept it. The store layer scopes correctly today and the integration tests
-- prove the API isolates — this is the layer underneath, so the invariant
-- holds even when something bypasses the helpers.
--
-- Technique: composite foreign keys. A referenced table exposes a UNIQUE
-- (id, team_id), and the referring table points its (fk, team_id) pair at it,
-- so PostgreSQL itself checks that both sides name the same team.

-- --------------------------------------------------------- pre-flight check --
-- Refuse to install constraints over data that already violates them, and say
-- exactly where the problem is instead of leaving a half-migrated schema.
DO $$
DECLARE
    offenders BIGINT;
BEGIN
    SELECT count(*) INTO offenders FROM (
        SELECT 1 FROM messages m JOIN agents a ON a.id = m.sender_agent_id
         WHERE a.team_id <> m.team_id
        UNION ALL
        SELECT 1 FROM messages m JOIN agents a ON a.id = m.recipient_agent_id
         WHERE a.team_id <> m.team_id
        UNION ALL
        SELECT 1 FROM messages m JOIN channels c ON c.id = m.channel_id
         WHERE c.team_id <> m.team_id
        UNION ALL
        SELECT 1 FROM tasks t JOIN agents a ON a.id = t.claimed_by
         WHERE a.team_id <> t.team_id
        UNION ALL
        SELECT 1 FROM tasks t JOIN agents a ON a.id = t.created_by
         WHERE a.team_id <> t.team_id
        UNION ALL
        SELECT 1 FROM notes n JOIN agents a ON a.id = n.updated_by
         WHERE a.team_id <> n.team_id
        UNION ALL
        SELECT 1 FROM channels c JOIN agents a ON a.id = c.created_by
         WHERE a.team_id <> c.team_id
        UNION ALL
        SELECT 1 FROM locks l JOIN agents a ON a.id = l.holder_agent_id
         WHERE a.team_id <> l.team_id
        UNION ALL
        SELECT 1 FROM attachments at JOIN agents a ON a.id = at.uploader_agent_id
         WHERE a.team_id <> at.team_id
        UNION ALL
        SELECT 1 FROM attachments at JOIN messages m ON m.id = at.message_id
         WHERE m.team_id <> at.team_id
        UNION ALL
        SELECT 1 FROM attachments at JOIN tasks t ON t.id = at.task_id
         WHERE t.team_id <> at.team_id
        UNION ALL
        SELECT 1 FROM task_deps d
          JOIN tasks a ON a.id = d.task_id
          JOIN tasks b ON b.id = d.blocked_by_task_id
         WHERE a.team_id <> b.team_id
    ) AS bad;

    IF offenders > 0 THEN
        RAISE EXCEPTION
            'cannot enforce team consistency: % row(s) already reference another team''s data. '
            'Inspect and fix them before migrating; this migration is refusing rather than '
            'silently dropping the constraint.', offenders;
    END IF;
END $$;

-- ------------------------------------------------- referenceable identities --
-- (id) is already unique as the primary key; these add the (id, team_id) pair
-- a composite foreign key needs to point at. They cost one index each.
ALTER TABLE agents   ADD CONSTRAINT agents_id_team_key   UNIQUE (id, team_id);
ALTER TABLE channels ADD CONSTRAINT channels_id_team_key UNIQUE (id, team_id);
ALTER TABLE tasks    ADD CONSTRAINT tasks_id_team_key    UNIQUE (id, team_id);
ALTER TABLE messages ADD CONSTRAINT messages_id_team_key UNIQUE (id, team_id);

-- ------------------------------------------------------- composite foreign --
-- Deletion behavior mirrors the original single-column keys exactly, so
-- nothing about cascade or set-null semantics changes.

ALTER TABLE messages
    ADD CONSTRAINT messages_sender_same_team
        FOREIGN KEY (sender_agent_id, team_id) REFERENCES agents (id, team_id)
        ON DELETE CASCADE,
    ADD CONSTRAINT messages_recipient_same_team
        FOREIGN KEY (recipient_agent_id, team_id) REFERENCES agents (id, team_id)
        ON DELETE CASCADE,
    ADD CONSTRAINT messages_channel_same_team
        FOREIGN KEY (channel_id, team_id) REFERENCES channels (id, team_id)
        ON DELETE CASCADE;

ALTER TABLE channels
    ADD CONSTRAINT channels_creator_same_team
        FOREIGN KEY (created_by, team_id) REFERENCES agents (id, team_id)
        ON DELETE SET NULL (created_by);

ALTER TABLE tasks
    ADD CONSTRAINT tasks_claimer_same_team
        FOREIGN KEY (claimed_by, team_id) REFERENCES agents (id, team_id)
        ON DELETE SET NULL (claimed_by),
    ADD CONSTRAINT tasks_creator_same_team
        FOREIGN KEY (created_by, team_id) REFERENCES agents (id, team_id)
        ON DELETE SET NULL (created_by);

ALTER TABLE notes
    ADD CONSTRAINT notes_updater_same_team
        FOREIGN KEY (updated_by, team_id) REFERENCES agents (id, team_id)
        ON DELETE SET NULL (updated_by);

ALTER TABLE locks
    ADD CONSTRAINT locks_holder_same_team
        FOREIGN KEY (holder_agent_id, team_id) REFERENCES agents (id, team_id)
        ON DELETE CASCADE;

ALTER TABLE attachments
    ADD CONSTRAINT attachments_uploader_same_team
        FOREIGN KEY (uploader_agent_id, team_id) REFERENCES agents (id, team_id)
        ON DELETE CASCADE,
    ADD CONSTRAINT attachments_message_same_team
        FOREIGN KEY (message_id, team_id) REFERENCES messages (id, team_id)
        ON DELETE CASCADE,
    ADD CONSTRAINT attachments_task_same_team
        FOREIGN KEY (task_id, team_id) REFERENCES tasks (id, team_id)
        ON DELETE CASCADE;

-- task_deps carries no team_id of its own: both sides are tasks, so the rule
-- is that the two tasks agree with each other. A trigger is the honest tool —
-- a foreign key cannot compare two parents.
CREATE FUNCTION assert_task_dep_same_team() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    a UUID;
    b UUID;
BEGIN
    SELECT team_id INTO a FROM tasks WHERE id = NEW.task_id;
    SELECT team_id INTO b FROM tasks WHERE id = NEW.blocked_by_task_id;
    IF a IS DISTINCT FROM b THEN
        RAISE EXCEPTION 'task dependency crosses teams';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER task_deps_same_team
    BEFORE INSERT OR UPDATE ON task_deps
    FOR EACH ROW EXECUTE FUNCTION assert_task_dep_same_team();
