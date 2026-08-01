-- ai-crew-sync :: v0.4 — small file attachments on messages and tasks.
-- Deliberately bytea-in-Postgres with a hard size cap: one binary, one
-- database is part of the product; no external object storage.

CREATE TABLE attachments (
    id                 BIGSERIAL PRIMARY KEY,
    team_id            UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- exactly one of message_id / task_id is set
    message_id         BIGINT REFERENCES messages(id) ON DELETE CASCADE,
    task_id            UUID REFERENCES tasks(id) ON DELETE CASCADE,
    uploader_agent_id  UUID NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    filename           TEXT NOT NULL,
    content_type       TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes         BIGINT NOT NULL,
    data               BYTEA NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((message_id IS NULL) <> (task_id IS NULL)),
    CHECK (size_bytes > 0)
);

CREATE INDEX attachments_message_idx ON attachments(message_id)
    WHERE message_id IS NOT NULL;
CREATE INDEX attachments_task_idx ON attachments(task_id)
    WHERE task_id IS NOT NULL;
