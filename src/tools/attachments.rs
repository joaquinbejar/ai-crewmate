use rmcp::{
    ErrorData, Json, handler::server::wrapper::Parameters, service::RequestContext, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Bus, auth_of};
use crate::{
    error::BusError,
    model::{AttachmentContent, AttachmentMeta},
    store::attachments,
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachFileArgs {
    /// Task key to attach to (any teammate may attach to a task).
    #[serde(default)]
    pub task: Option<String>,
    /// Message id to attach to (only your own messages; prefer attaching at
    /// post_message time so readers never see the message without its files).
    #[serde(default)]
    pub message_id: Option<i64>,
    pub filename: String,
    /// MIME type; defaults to application/octet-stream.
    #[serde(default)]
    pub content_type: Option<String>,
    /// File content, base64-encoded. Decoded size is limited to 256 KiB.
    pub data_base64: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetAttachmentArgs {
    /// Attachment id, as listed in a message's or task's `attachments`.
    pub id: i64,
}

#[tool_router(router = attachments_router, vis = "pub")]
impl Bus {
    #[tool(
        description = "Attach a small file (diff, log, config — max 256 KiB) to a task or to \
                       a message you sent. Teammates see it listed on the task/message and \
                       download it with get_attachment. To share a file with a new message, \
                       pass `attachments` to post_message instead."
    )]
    async fn attach_file(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<AttachFileArgs>,
    ) -> Result<Json<AttachmentMeta>, ErrorData> {
        let auth = auth_of(&ctx)?;
        let att = attachments::decode_input(&args.filename, args.content_type, &args.data_base64)?;
        let meta = match (&args.task, args.message_id) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(BusError::invalid(
                    "set either `task` (a task key) or `message_id`, not both",
                )
                .into());
            }
            (Some(task), None) => attachments::attach_to_task(&self.db, &auth, task, &att).await?,
            (None, Some(message_id)) => {
                attachments::attach_to_message(&self.db, &auth, message_id, &att).await?
            }
        };
        Ok(Json(meta))
    }

    #[tool(
        description = "Download an attachment by id (returns metadata plus base64 content). \
                       Attachments on direct messages are only visible to the DM's two \
                       parties; everything else is team-visible."
    )]
    async fn get_attachment(
        &self,
        ctx: RequestContext<rmcp::RoleServer>,
        Parameters(args): Parameters<GetAttachmentArgs>,
    ) -> Result<Json<AttachmentContent>, ErrorData> {
        let auth = auth_of(&ctx)?;
        Ok(Json(
            attachments::get_attachment(&self.db, &auth, args.id).await?,
        ))
    }
}
