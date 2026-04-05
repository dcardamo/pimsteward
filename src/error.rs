//! Typed errors for pimsteward.
//!
//! Design rule: every `Display` impl must be safe to log — no credential
//! values, no cookie contents, no `Authorization` header values.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// The forwardemail API returned a non-2xx with a structured error body.
    #[error("forwardemail api: HTTP {status}: {message}")]
    Api { status: u16, message: String },

    /// Permission gate rejected the operation.
    #[error("permission denied: {resource} requires {required:?} but config grants {granted:?}")]
    PermissionDenied {
        resource: crate::permission::Resource,
        required: crate::permission::Access,
        granted: crate::permission::Access,
    },

    #[error("git store: {0}")]
    Store(String),

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

impl Error {
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }
    pub fn store(msg: impl Into<String>) -> Self {
        Self::Store(msg.into())
    }
}
