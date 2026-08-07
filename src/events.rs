//! In-process event hub fed by Postgres LISTEN/NOTIFY.
//!
//! One background task holds a single LISTEN connection on `bus_events` and
//! fans every payload out to in-process subscribers through a tokio broadcast
//! channel. Consumers: the `wait_for_updates` tool (long-poll) and the webhook
//! dispatcher. Payloads carry ids only; consumers resolve names against the
//! database when they need them.

use sqlx::{PgPool, postgres::PgListener};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const PG_CHANNEL: &str = "bus_events";

/// A parsed NOTIFY payload. Kept as loose JSON plus typed accessors so adding
/// fields to the triggers never breaks older consumers.
#[derive(Clone, Debug)]
pub struct BusEvent(pub serde_json::Value);

impl BusEvent {
    pub fn kind(&self) -> &str {
        self.0.get("kind").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn uuid_field(&self, key: &str) -> Option<Uuid> {
        self.0
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
    }

    pub fn team_id(&self) -> Option<Uuid> {
        self.uuid_field("team_id")
    }

    pub fn recipient_agent_id(&self) -> Option<Uuid> {
        self.uuid_field("recipient_agent_id")
    }

    pub fn sender_agent_id(&self) -> Option<Uuid> {
        self.uuid_field("sender_agent_id")
    }

    pub fn channel_id(&self) -> Option<Uuid> {
        self.uuid_field("channel_id")
    }

    pub fn message_id(&self) -> Option<i64> {
        self.0.get("id").and_then(|v| v.as_i64())
    }

    /// Working context this message was addressed to, if it was addressed to
    /// one. Absent means every session of the recipient.
    pub fn recipient_session(&self) -> Option<&str> {
        self.0.get("recipient_session").and_then(|v| v.as_str())
    }

    /// Was this posted as an announcement — something the sender judged worth
    /// interrupting the whole team for?
    pub fn is_announcement(&self) -> bool {
        self.0
            .get("announce")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn is_direct_message(&self) -> bool {
        self.kind() == "message" && self.recipient_agent_id().is_some()
    }

    /// Is this event visible to `session` of `agent` of `team`?
    ///
    /// Direct messages are only visible to their recipient (and sender);
    /// everything else is team-wide. A message addressed to one session does
    /// not wake the recipient's other sessions — otherwise every window of a
    /// person would wake for a question meant for one of them. It still
    /// reaches every session of the *sender*, which is how a reply finds the
    /// window that is blocked waiting for it.
    pub fn visible_to(&self, team_id: Uuid, agent_id: Uuid, session: &str) -> bool {
        if self.team_id() != Some(team_id) {
            return false;
        }
        if self.is_direct_message() {
            if self.sender_agent_id() == Some(agent_id) {
                return true;
            }
            if self.recipient_agent_id() != Some(agent_id) {
                return false;
            }
            return match self.recipient_session() {
                Some(addressed) => addressed == session,
                // Addressed to the person: every one of their sessions.
                None => true,
            };
        }
        true
    }
}

#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<BusEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        // 256 in-flight events is plenty; laggards get Lagged and resync from
        // the database, which every consumer does anyway.
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, event: BusEvent) {
        // No receivers is fine: nobody is waiting right now.
        let _ = self.tx.send(event);
    }
}

/// Run the LISTEN loop until cancelled. Reconnects with backoff on failure so
/// a Postgres restart degrades to polling latency instead of killing wakeups.
pub async fn run_pg_listener(pool: PgPool, hub: EventHub, ct: CancellationToken) {
    loop {
        if ct.is_cancelled() {
            return;
        }
        match PgListener::connect_with(&pool).await {
            Ok(mut listener) => {
                if let Err(e) = listener.listen(PG_CHANNEL).await {
                    tracing::warn!(error = %e, "LISTEN failed; retrying");
                } else {
                    tracing::info!("event listener attached to '{PG_CHANNEL}'");
                    loop {
                        tokio::select! {
                            _ = ct.cancelled() => return,
                            recv = listener.try_recv() => match recv {
                                Ok(Some(notification)) => {
                                    match serde_json::from_str(notification.payload()) {
                                        Ok(value) => hub.publish(BusEvent(value)),
                                        Err(e) => tracing::warn!(
                                            error = %e,
                                            payload = notification.payload(),
                                            "unparseable bus event"
                                        ),
                                    }
                                }
                                // None = connection dropped and was re-established;
                                // notifications in between are lost, which consumers
                                // tolerate by re-checking the database.
                                Ok(None) => tracing::debug!("event listener reconnected"),
                                Err(e) => {
                                    tracing::warn!(error = %e, "event listener error");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not attach event listener; retrying");
            }
        }
        tokio::select! {
            _ = ct.cancelled() => return,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
        }
    }
}
