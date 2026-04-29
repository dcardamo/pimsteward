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

    /// HTTP transport error from reqwest. The captured `String` is the
    /// full source chain — `reqwest::Error`'s `Display` only shows the
    /// top-level message (e.g. "error sending request for url (...)"),
    /// dropping the underlying cause (DNS, TLS handshake, connection
    /// refused, timeout, etc). We expand it at construction time via
    /// `From<reqwest::Error>` so the cause chain survives into logs and
    /// MCP error responses.
    #[error("http: {0}")]
    Http(String),

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

    /// Send-email permission is separate from read/write on the email
    /// resource because sending over SMTP is irreversible in a way that
    /// read_write on a mailbox folder isn't. Denial surfaces its own
    /// variant so the MCP layer can produce a specific error rather than
    /// overloading `PermissionDenied`'s Access fields with a synthetic
    /// encoding.
    #[error("permission denied: email_send requires 'allowed' but config grants 'denied' — set [permissions] email_send = \"allowed\" to enable (explicit opt-in; read_write does NOT imply send)")]
    SendDenied,

    #[error("git store: {0}")]
    Store(String),

    #[error("search index: {0}")]
    Index(String),

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
    pub fn index(msg: impl Into<String>) -> Self {
        Self::Index(msg.into())
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Index(e.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(fmt_error_chain(&e))
    }
}

/// Walk a `std::error::Error`'s source chain and join every level into
/// a single ": "-separated string. Keeps the top-level message intact
/// and appends each underlying cause so logs like
/// `http: error sending request for url (...)` become
/// `http: error sending request for url (...): connection refused
/// (os error 111)`. Stops at the deepest source.
pub fn fmt_error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        out.push_str(": ");
        out.push_str(&e.to_string());
        cur = e.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Chained {
        msg: &'static str,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    }
    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.msg)
        }
    }
    impl std::error::Error for Chained {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
        }
    }

    #[test]
    fn fmt_error_chain_no_source() {
        let e = Chained { msg: "top", source: None };
        assert_eq!(fmt_error_chain(&e), "top");
    }

    #[test]
    fn fmt_error_chain_walks_full_chain() {
        let leaf = Chained { msg: "connection refused", source: None };
        let mid = Chained { msg: "tcp connect failed", source: Some(Box::new(leaf)) };
        let top = Chained {
            msg: "error sending request for url (https://api.forwardemail.net/v1/folders)",
            source: Some(Box::new(mid)),
        };
        assert_eq!(
            fmt_error_chain(&top),
            "error sending request for url (https://api.forwardemail.net/v1/folders): tcp connect failed: connection refused"
        );
    }
}
