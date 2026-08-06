-- Sessions: one person, several concurrent working contexts.
--
-- A bearer token identifies a person, but a person runs several coding
-- sessions at once — typically one per repository. Everything tracked per
-- agent (presence, task claims, locks) was therefore shared between sessions
-- doing unrelated work, and they overwrote each other.
--
-- A session is a short label that rides on every request in the
-- X-Crew-Session header. It partitions *work context*; it never partitions
-- *identity*, which still comes from the token alone. A caller may claim any
-- session string it likes and the worst it can do is confuse its own agent's
-- rows.
--
-- The empty string is a real session, and the one every existing row lands
-- on, so a client that sends no header behaves exactly as it did before.
--
-- Every session column lives in this one migration, including the message
-- ones that no code reads yet: applied migrations are immutable, and having
-- several stacked changes edit one file is how checksums break.

-- ------------------------------------------------------------- presence --

-- Was PRIMARY KEY (agent_id): one row per person, so a second session
-- silently overwrote the first one's repo and branch.
ALTER TABLE agent_presence ADD COLUMN session TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_presence DROP CONSTRAINT agent_presence_pkey;
ALTER TABLE agent_presence ADD PRIMARY KEY (agent_id, session);

-- ---------------------------------------------------------- claims/locks --

-- Which session holds the claim. NULL means a claim taken before this
-- migration, or by a client that sends no session header; both are treated
-- as the shared session by the code that reads them.
ALTER TABLE tasks ADD COLUMN claimed_session TEXT;
ALTER TABLE locks ADD COLUMN holder_session TEXT;

-- --------------------------------------------------------------- messages --

-- recipient_session NULL on a direct message means every session of the
-- recipient, which is what addressing a person (rather than one of their
-- sessions) has always meant. sender_session lets a reply return to the
-- session that asked instead of to whichever one notices first.
ALTER TABLE messages ADD COLUMN recipient_session TEXT;
ALTER TABLE messages ADD COLUMN sender_session TEXT;
