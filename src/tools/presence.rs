use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Bus, auth_of};
use crate::{
    model::{AgentInfo, AgentList},
    store::presence,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HeartbeatArgs {
    /// One of "active", "idle", "busy", "blocked". Defaults to "active".
    #[serde(default)]
    pub status: Option<String>,
    /// Repository you are working in, e.g. "acme/api".
    #[serde(default)]
    pub repo: Option<String>,
    /// Branch you are on.
    #[serde(default)]
    pub branch: Option<String>,
    /// Short description of what you are doing right now, e.g. "rewriting the
    /// token refresh flow". This is what teammates see in list_agents.
    #[serde(default)]
    pub activity: Option<String>,
    /// How long this presence stays valid before you are shown as offline.
    /// Defaults to 600 (10 minutes).
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAgentsArgs {
    /// Only return agents whose presence has not expired.
    #[serde(default)]
    pub online_only: bool,
}

#[tool_router(router = presence_router, vis = "pub")]
impl Bus {
    #[tool(
        description = "Publish what you are currently working on so teammates' agents can \
                       see it. Call this when you start a piece of work and whenever the \
                       focus changes. Omitted fields keep their previous value."
    )]
    async fn heartbeat(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<HeartbeatArgs>,
    ) -> Result<Json<AgentInfo>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let input = presence::HeartbeatInput {
            status: args.status,
            repo: args.repo,
            branch: args.branch,
            activity: args.activity,
            ttl_seconds: args.ttl_seconds,
        };
        Ok(Json(presence::heartbeat(&self.db, &auth, input).await?))
    }

    #[tool(
        description = "See who else is on the bus, whether they are online, and what each \
                       one is working on. Useful before claiming work or asking a question."
    )]
    async fn list_agents(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<ListAgentsArgs>,
    ) -> Result<Json<AgentList>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            presence::list_agents(&self.db, &auth, args.online_only).await?,
        ))
    }
}
