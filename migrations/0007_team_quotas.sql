-- ai-crew-sync :: v0.5 — per-team storage quotas.
--
-- Attachments are bytea in Postgres by design (one binary, one database), so
-- the database IS the object store and nothing bounded its growth: a team
-- could fill the volume that every other team shares. Messages, note
-- revisions and task events are append-only for the same reason.
--
-- A quota is per team and opt-in: NULL means unlimited, which keeps every
-- existing deployment behaving exactly as before until an operator sets one.
ALTER TABLE teams
    ADD COLUMN attachment_bytes_limit BIGINT
        CHECK (attachment_bytes_limit IS NULL OR attachment_bytes_limit > 0);

COMMENT ON COLUMN teams.attachment_bytes_limit IS
    'Total attachment bytes this team may store. NULL = unlimited.';

-- The quota check sums a team's attachment bytes on every upload, so it needs
-- to be an index-only lookup rather than a scan of the payloads.
CREATE INDEX attachments_team_size_idx ON attachments (team_id) INCLUDE (size_bytes);

-- Retention prunes by age; without this the sweep scans every message.
CREATE INDEX messages_created_idx ON messages (created_at);
