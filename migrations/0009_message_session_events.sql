-- Carry the addressed session in the message NOTIFY payload.
--
-- A direct message can now be addressed to one working context of a person
-- (`joaquin/market-data`) rather than to the person. Without the label in the
-- payload, `wait_for_updates` cannot tell one of your sessions from another,
-- so every window would wake for a question meant for one of them — the noise
-- sessions exist to remove.
--
-- 0008 added the columns; this replaces the trigger function that reads them.
-- Consumers parse the payload as loose JSON with typed accessors, so an older
-- server ignores the new field rather than failing on it.

CREATE OR REPLACE FUNCTION notify_message() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('bus_events', json_build_object(
        'kind', 'message',
        'team_id', NEW.team_id,
        'id', NEW.id,
        'channel_id', NEW.channel_id,
        'recipient_agent_id', NEW.recipient_agent_id,
        -- NULL means every session of the recipient, which is what addressing
        -- a person has always meant.
        'recipient_session', NEW.recipient_session,
        'sender_agent_id', NEW.sender_agent_id
    )::text);
    RETURN NULL;
END $$;
