-- ai-crew-sync :: v0.5 — one index, justified by a measured plan.
--
-- The digest and the dashboard both ask the same question — "channel messages
-- for this team in the last N hours" — and both got a sequential scan over
-- every message the team has ever sent, because messages_team_idx is
-- (team_id, id DESC) and answers nothing about time.
--
-- Measured on 200k messages across 40 channels and 30 days (PostgreSQL 18):
--
--   dashboard 24h count   6.57 ms  seq scan   ->  0.54 ms  index-only scan
--   digest (24h window)  13.39 ms  seq scan   -> 11.30 ms  bitmap index scan
--
-- The count is the clear win; the digest gain is smaller at this size but the
-- plan stops reading the whole table, so it no longer degrades with total
-- history the way a scan does.
--
-- Partial on channel_id IS NOT NULL: every consumer of this index excludes
-- direct messages, so indexing them would be dead weight.
--
-- Nothing else earned an index. The dashboard's task list already resolves
-- with a top-N heapsort in 0.8 ms over 5k tasks, and an index there would
-- cost writes on every claim and lease renewal to save nothing measurable.
CREATE INDEX messages_team_created_idx
    ON messages (team_id, created_at DESC)
    WHERE channel_id IS NOT NULL;
