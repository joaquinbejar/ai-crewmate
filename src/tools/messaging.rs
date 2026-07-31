use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Bus, auth_of};
use crate::{
    model::{ChannelInfo, ChannelList, MessageList, PostMessageResult},
    store::messaging,
};

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
