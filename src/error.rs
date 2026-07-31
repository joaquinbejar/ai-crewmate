use rmcp::ErrorData;

/// Errors produced by the store layer.
///
/// These map onto MCP tool errors so that the calling agent gets a useful
/// message instead of an opaque "internal error".
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("unauthenticated: {0}")]
    Unauthenticated(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

impl BusError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
}

impl From<BusError> for ErrorData {
    fn from(err: BusError) -> Self {
        match err {
            BusError::NotFound(m) => ErrorData::invalid_params(format!("not found: {m}"), None),
            BusError::Invalid(m) => ErrorData::invalid_params(m, None),
            BusError::Conflict(m) => ErrorData::invalid_params(format!("conflict: {m}"), None),
            BusError::Unauthenticated(m) => {
                ErrorData::invalid_request(format!("unauthenticated: {m}"), None)
            }
            BusError::Db(e) => {
                // Never leak SQL/connection detail to the model; log it instead.
                tracing::error!(error = %e, "database error");
                ErrorData::internal_error("database error", None)
            }
        }
    }
}

pub type BusResult<T> = Result<T, BusError>;
