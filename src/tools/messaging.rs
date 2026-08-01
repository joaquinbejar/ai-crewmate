use std::time::Duration;

use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Bus, auth_of};
use crate::{
    model::{AskResult, ChannelInfo, ChannelList, MessageList, PostMessageResult},
    store::{agent_id_by_name, messaging},
};

const ASK_DEFAULT_TIMEOUT_SECS: i64 = 45;
const ASK_MAX_TIMEOUT_SECS: i64 = 55;

fn default_limit() -> i64 {
    50
}
fn default_true() -> bool {
    true
}
fn default_scope() -> String {
    "all".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateChannelArgs {
    /// Channel name, e.g. "deploys". A leading '#' is optional and names are
    /// lowercased. Creating a channel that already exists is not an error.
    pub name: String,
    /// Optional one-line description of what belongs in this channel.
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostMessageArgs {
    /// Channel to broadcast to. Mutually exclusive with `to`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Agent handle to send a direct message to. Mutually exclusive with `channel`.
    #[serde(default)]
    pub to: Option<String>,
    /// The message text. Keep it short and factual; teammates' agents read this.
    pub body: String,
    /// Id of the message this replies to, to keep a thread together.
    #[serde(default)]
    pub reply_to: Option<i64>,
    /// Optional structured payload attached to the message (any JSON object).
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadMessagesArgs {
    /// What to read: "all" (everything visible to you), "inbox" (direct
    /// messages addressed to you), or a channel name such as "deploys".
    #[serde(default = "default_scope")]
    pub scope: String,
    /// When true (the default) return only messages you have not read yet and
    /// advance your read cursor. Set false to re-read recent history.
    #[serde(default = "default_true")]
    pub only_new: bool,
    /// Maximum messages to return (1-200).
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskAgentArgs {
    /// Agent handle to ask. Use list_agents to see who is around.
    pub to: String,
    /// The question, sent as a direct message. Omit when resuming with
    /// `resume_message_id`.
    #[serde(default)]
    pub question: Option<String>,
    /// How long to wait for the answer, in seconds (5-55, default 45).
    /// Kept under a minute so HTTP intermediaries do not cut the call.
    #[serde(default)]
    pub timeout_seconds: Option<i64>,
    /// Keep waiting on an earlier question instead of sending a new one:
    /// pass the `question_message_id` from a timed-out ask_agent call.
    #[serde(default)]
    pub resume_message_id: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchMessagesArgs {
    /// Full-text search terms.
    pub query: String,
    /// Maximum messages to return (1-200).
    #[serde(default = "default_limit")]
    pub limit: i64,
}

#[tool_router(router = messaging_router, vis = "pub")]
impl Bus {
    #[tool(
        description = "List the team's channels with their topic and message count. \
                       Use this before posting so you pick an existing channel."
    )]
    async fn list_channels(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<Json<ChannelList>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(messaging::list_channels(&self.db, &auth).await?))
    }

    #[tool(
        description = "Create a channel (or update its topic if it already exists). \
                       Channels are shared by the whole team."
    )]
    async fn create_channel(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<CreateChannelArgs>,
    ) -> Result<Json<ChannelInfo>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            messaging::create_channel(&self.db, &auth, &args.name, args.topic).await?,
        ))
    }

    #[tool(
        description = "Send a message to the team. Set `channel` to broadcast, or `to` \
                       with an agent handle to send a direct message. The sender is \
                       your own identity and cannot be spoofed."
    )]
    async fn post_message(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<PostMessageArgs>,
    ) -> Result<Json<PostMessageResult>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let input = messaging::PostInput {
            channel: args.channel,
            to: args.to,
            body: args.body,
            reply_to: args.reply_to,
            metadata: args.metadata,
        };
        Ok(Json(messaging::post_message(&self.db, &auth, input).await?))
    }

    #[tool(
        description = "Read messages from the bus. By default returns only what you \
                       have not seen and marks it as read, so calling it repeatedly \
                       gives you an incremental feed of what teammates are saying."
    )]
    async fn read_messages(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<ReadMessagesArgs>,
    ) -> Result<Json<MessageList>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let input = messaging::ReadInput {
            scope: args.scope,
            only_new: args.only_new,
            limit: args.limit,
        };
        Ok(Json(
            messaging::read_messages(&self.db, &auth, input).await?,
        ))
    }

    #[tool(
        description = "Ask a teammate's agent a question and block until they answer or the \
                       timeout passes. Sends the question as a direct message and waits for \
                       their reply, so one call replaces post_message + wait_for_updates + \
                       read_messages. On timeout, call it again with `resume_message_id` set \
                       to the returned question_message_id to keep waiting without asking \
                       twice. If you receive a question yourself, answer with post_message \
                       (`to` the asker, `reply_to` the question id)."
    )]
    async fn ask_agent(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<AskAgentArgs>,
    ) -> Result<Json<AskResult>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let to = args.to.trim().to_owned();
        let timeout = args
            .timeout_seconds
            .unwrap_or(ASK_DEFAULT_TIMEOUT_SECS)
            .clamp(5, ASK_MAX_TIMEOUT_SECS);

        let target_id = agent_id_by_name(&self.db, auth.team_id, &to).await?;
        if target_id == auth.agent_id {
            return Err(crate::error::BusError::invalid(
                "you cannot ask yourself; call list_agents and pick a teammate",
            )
            .into());
        }

        // Subscribe before posting or checking so an answer arriving in
        // between still wakes us.
        let mut rx = self.hub.subscribe();

        let question_id = match args.resume_message_id {
            Some(id) => {
                messaging::verify_question(&self.db, &auth, target_id, id).await?;
                id
            }
            None => {
                let question = args
                    .question
                    .as_deref()
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                    .ok_or_else(|| {
                        crate::error::BusError::invalid(
                            "provide `question`, or `resume_message_id` to keep waiting on \
                             an earlier one",
                        )
                    })?;
                let posted = messaging::post_message(
                    &self.db,
                    &auth,
                    messaging::PostInput {
                        channel: None,
                        to: Some(to.clone()),
                        body: question.to_owned(),
                        reply_to: None,
                        metadata: Some(serde_json::json!({ "question": true })),
                    },
                )
                .await?;
                posted.message.id
            }
        };

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout as u64);
        loop {
            // Check the database first: covers resumed asks whose answer
            // already landed, and events lost while lagging.
            if let Some(answer) =
                messaging::find_answer(&self.db, &auth, target_id, question_id).await?
            {
                return Ok(Json(AskResult {
                    answered: true,
                    to,
                    question_message_id: question_id,
                    answer: Some(answer),
                    suggestion: "The answer also sits unread in your inbox; read_messages \
                                 will mark it read."
                        .into(),
                }));
            }

            // Wait for a direct message from the target (or the deadline).
            let woke = loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break false,
                    recv = rx.recv() => match recv {
                        Ok(ev) => {
                            if ev.kind() == "message"
                                && ev.sender_agent_id() == Some(target_id)
                                && ev.recipient_agent_id() == Some(auth.agent_id)
                            {
                                break true;
                            }
                        }
                        // Lagged: events were dropped; resync from the database.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break true,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break false,
                    }
                }
            };

            if !woke {
                // One last look: the answer may have raced the deadline.
                let answer =
                    messaging::find_answer(&self.db, &auth, target_id, question_id).await?;
                let answered = answer.is_some();
                return Ok(Json(AskResult {
                    answered,
                    suggestion: if answered {
                        "The answer also sits unread in your inbox; read_messages will \
                         mark it read."
                            .into()
                    } else {
                        format!(
                            "{to} has not answered within {timeout}s. Call ask_agent again \
                             with resume_message_id={question_id} to keep waiting without \
                             re-sending, or do other work and check read_messages later."
                        )
                    },
                    to,
                    question_message_id: question_id,
                    answer,
                }));
            }
        }
    }

    #[tool(
        description = "Full-text search across the team's channel history and your own \
                       direct messages. Does not affect your read cursor."
    )]
    async fn search_messages(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<SearchMessagesArgs>,
    ) -> Result<Json<MessageList>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            messaging::search_messages(&self.db, &auth, &args.query, args.limit).await?,
        ))
    }
}
