use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Bus, auth_of};
use crate::{
    model::{Ack, LockList, LockResult},
    store::locks,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AcquireLockArgs {
    /// Resource to lock, e.g. "deploy:staging" or "schema:users". Free-form,
    /// lowercased; pick names the whole team will recognise.
    pub name: String,
    /// Seconds until the lock lapses on its own (5-86400, default 300).
    /// Re-acquire before it expires to extend it.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
    /// One line shown to whoever finds the resource locked.
    #[serde(default)]
    pub purpose: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LockNameArgs {
    /// The lock name.
    pub name: String,
}

#[tool_router(router = locks_router, vis = "pub")]
impl Bus {
    #[tool(
        description = "Take an advisory lock on a shared resource (a deploy target, a schema, \
                       a file) before touching it. Returns acquired=false with the holder's name \
                       if someone else has it — in that case wait_for_updates will wake you when \
                       it frees. Locks expire on their own, so a crashed agent cannot jam the \
                       team. Re-acquiring your own lock extends it."
    )]
    async fn acquire_lock(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<AcquireLockArgs>,
    ) -> Result<Json<LockResult>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            locks::acquire_lock(&self.db, &auth, &args.name, args.ttl_seconds, args.purpose)
                .await?,
        ))
    }

    #[tool(
        description = "Release a lock you hold, waking anyone waiting on it. Releasing a lock \
                       you do not hold is an error."
    )]
    async fn release_lock(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<LockNameArgs>,
    ) -> Result<Json<Ack>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            locks::release_lock(&self.db, &auth, &args.name).await?,
        ))
    }

    #[tool(description = "List the team's currently held locks: who holds what, why, until when.")]
    async fn list_locks(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
    ) -> Result<Json<LockList>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(locks::list_locks(&self.db, &auth).await?))
    }
}
