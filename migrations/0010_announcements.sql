-- Announcements: a channel message that reaches every session, focus or not.
--
-- Giving a session the channel named after it stops a market-data window
-- waking for core-manager chatter, which is most of the point. It also
-- silences the case that most needs to get through: a deploy, a migration, a
-- breaking change, "stop pushing to main for ten minutes". Those are posted
-- to a channel most people are not focused on, and they are seen whenever
-- someone next looks — which for an announcement is too late.
--
-- The flag lives on the message, not on the channel. The same channel carries
-- routine chatter and, now and then, something everybody must see; a reserved
-- channel name would force that choice up front and break for a team that
-- names its channels differently.
--
-- Deliberately not fan-out to several channels: one message with one id in one
-- place keeps replies and reply_to coherent, which five copies would not.

ALTER TABLE messages ADD COLUMN announce BOOLEAN NOT NULL DEFAULT false;

-- Announcements are rare by design, so the index only covers them. A team's
-- whole history of announcements is a handful of rows.
CREATE INDEX messages_announce_idx ON messages (team_id, id DESC)
    WHERE announce;

-- Carry the flag in the NOTIFY payload: wait_for_updates decides whether to
-- wake from the payload alone, without a round trip to read the row.
CREATE OR REPLACE FUNCTION notify_message() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'message',
        'team_id', NEW.team_id,
        'id', NEW.id,
        'channel_id', NEW.channel_id,
        -- Every field the previous version emitted has to be repeated here:
        -- CREATE OR REPLACE rewrites the whole body, so an omission silently
        -- turns a direct message into a team-wide event.
        'recipient_agent_id', NEW.recipient_agent_id,
        -- NULL means every session of the recipient, which is what addressing
        -- a person has always meant.
        'recipient_session', NEW.recipient_session,
        'sender_agent_id', NEW.sender_agent_id,
        'announce', NEW.announce
    )::text);
    RETURN NULL;
END $$;
