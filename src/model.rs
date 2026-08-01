//! Wire types returned by the MCP tools.
//!
//! Timestamps are RFC 3339 strings rather than typed datetimes: the consumer is
//! a language model, and a plain string is both unambiguous and free of extra
//! schema dependencies.

use schemars::JsonSchema;
use serde::Serialize;

/// `serde_json::Value` fields would produce a boolean `true` schema, which
/// some MCP clients' validators reject; an empty object schema means the same
/// ("anything") and passes everywhere.
pub fn any_json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({})
}

pub fn ts(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn ts_opt(dt: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    dt.map(ts)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WhoAmI {
    /// Your agent handle. Other agents address you by this name.
    pub agent: String,
    pub agent_id: String,
    pub team: String,
    pub team_id: String,
    /// Number of unread direct messages waiting for you.
    pub unread_direct_messages: i64,
    /// Tasks currently claimed by you and not yet completed.
    pub open_claimed_tasks: i64,
}

// ------------------------------------------------------------------ agents --

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentInfo {
    pub name: String,
    pub display_name: Option<String>,
    /// One of `active`, `idle`, `offline`. `offline` means the presence lease
    /// expired, i.e. the agent has not sent a heartbeat recently.
    pub status: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// Free-text description of what this agent is currently doing.
    pub activity: Option<String>,
    pub last_seen: Option<String>,
    pub online: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentList {
    pub agents: Vec<AgentInfo>,
    pub online_count: usize,
}

// -------------------------------------------------------------- messaging --

#[derive(Debug, Serialize, JsonSchema)]
pub struct ChannelInfo {
    pub name: String,
    pub topic: Option<String>,
    pub message_count: i64,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ChannelList {
    pub channels: Vec<ChannelInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MessageInfo {
    pub id: i64,
    pub from: String,
    /// Channel name for channel messages; `null` for direct messages.
    pub channel: Option<String>,
    /// Recipient handle for direct messages; `null` for channel messages.
    pub to: Option<String>,
    pub body: String,
    pub reply_to: Option<i64>,
    #[schemars(schema_with = "any_json_schema")]
    pub metadata: serde_json::Value,
    /// Files attached to this message; fetch content with get_attachment.
    pub attachments: Vec<AttachmentMeta>,
    pub created_at: String,
}

#[derive(Debug, Serialize, serde::Deserialize, JsonSchema)]
pub struct AttachmentMeta {
    /// Pass this id to get_attachment to download the content.
    pub id: i64,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AttachmentContent {
    pub id: i64,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub uploaded_by: String,
    pub created_at: String,
    /// The file content, base64-encoded.
    pub data_base64: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PostMessageResult {
    pub message: MessageInfo,
    /// Handles that can now see this message.
    pub delivered_to: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MessageList {
    pub messages: Vec<MessageInfo>,
    /// The scope that was actually read, after normalisation.
    pub scope: String,
    /// Read cursor position after this call. Messages at or below this id will
    /// not be returned again when `only_new` is true.
    pub cursor: i64,
    /// True when the result hit `limit` and older/newer messages remain.
    pub truncated: bool,
}

// ------------------------------------------------------------------ tasks --

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskInfo {
    pub key: String,
    pub title: String,
    pub description: Option<String>,
    /// One of `open`, `claimed`, `done`, `cancelled`.
    pub status: String,
    /// Keys of tasks this one depends on.
    pub depends_on: Vec<String>,
    /// True while any dependency is not yet done/cancelled. Blocked tasks
    /// cannot be claimed.
    pub blocked: bool,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    /// When the current claim expires. After this instant another agent may
    /// steal the task, so renew the lease if you are still working on it.
    pub lease_expires_at: Option<String>,
    /// True when the claim has already lapsed.
    pub lease_expired: bool,
    pub result: Option<String>,
    #[schemars(schema_with = "any_json_schema")]
    pub metadata: serde_json::Value,
    /// Files attached to this task; fetch content with get_attachment.
    pub attachments: Vec<AttachmentMeta>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskList {
    pub tasks: Vec<TaskInfo>,
    pub open: i64,
    pub claimed: i64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ClaimResult {
    pub claimed: bool,
    pub task: Option<TaskInfo>,
    /// Present when `claimed` is false: why the claim did not succeed.
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskEventInfo {
    pub event: String,
    pub agent: Option<String>,
    pub detail: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskDetail {
    pub task: TaskInfo,
    pub history: Vec<TaskEventInfo>,
}

// ------------------------------------------------------------------ notes --

#[derive(Debug, Serialize, JsonSchema)]
pub struct NoteInfo {
    pub scope: String,
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    pub updated_by: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NoteList {
    pub notes: Vec<NoteInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NoteRef {
    pub scope: String,
    pub key: String,
    pub found: bool,
    pub note: Option<NoteInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Ack {
    pub ok: bool,
    pub detail: String,
}

// ------------------------------------------------------------------ locks --

#[derive(Debug, Serialize, JsonSchema)]
pub struct LockInfo {
    pub name: String,
    pub holder: String,
    pub purpose: Option<String>,
    pub acquired_at: String,
    /// When the lock lapses on its own if not renewed.
    pub expires_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LockList {
    pub locks: Vec<LockInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LockResult {
    pub acquired: bool,
    pub lock: Option<LockInfo>,
    /// Present when `acquired` is false: who holds it and until when.
    pub reason: Option<String>,
}

// ----------------------------------------------------------------- events --

#[derive(Debug, Serialize, JsonSchema)]
pub struct WaitEvent {
    /// One of `message`, `task`, `lock`, `note`.
    pub kind: String,
    /// Human-readable one-liner of what happened.
    pub summary: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WaitResult {
    /// True when something happened; false when the timeout elapsed quietly.
    pub woke: bool,
    pub timed_out: bool,
    pub events: Vec<WaitEvent>,
    /// Unread direct messages after the wait — if > 0, call read_messages.
    pub unread_direct_messages: i64,
    /// What to do next, e.g. which tool to call to fetch the details.
    pub suggestion: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AskResult {
    /// True when the teammate answered before the timeout.
    pub answered: bool,
    /// The agent the question was addressed to.
    pub to: String,
    /// Id of the question message. On timeout, pass it back as
    /// `resume_message_id` to keep waiting without re-sending the question.
    pub question_message_id: i64,
    /// The answer: their reply to the question, or failing that their first
    /// direct message to you after it.
    pub answer: Option<MessageInfo>,
    /// What to do next.
    pub suggestion: String,
}

// ----------------------------------------------------------------- digest --

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestMessage {
    pub from: String,
    pub body: String,
    pub at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestChannel {
    pub name: String,
    pub message_count: i64,
    pub last_messages: Vec<DigestMessage>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestTask {
    pub key: String,
    pub title: String,
    pub status: String,
    pub claimed_by: Option<String>,
    pub result: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestNote {
    pub scope: String,
    pub key: String,
    pub updated_by: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestAgent {
    pub name: String,
    pub activity: Option<String>,
    pub last_seen: Option<String>,
    pub online: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DigestResult {
    /// Window covered, in hours.
    pub hours: i64,
    pub channels: Vec<DigestChannel>,
    /// Tasks whose state changed inside the window, newest first.
    pub tasks_moved: Vec<DigestTask>,
    pub open_tasks: i64,
    pub claimed_tasks: i64,
    pub notes_updated: Vec<DigestNote>,
    pub agents_seen: Vec<DigestAgent>,
    pub active_locks: Vec<LockInfo>,
}
