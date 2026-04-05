//! The MailSource trait and shared types that implementations return.

use crate::error::Error;
use crate::forwardemail::mail::{Folder, MessageSummary};
use async_trait::async_trait;

/// A fetched message: the raw RFC822 bytes plus the forwardemail-shape
/// summary fields that the pull loop uses as diff keys. When the source is
/// IMAP, these fields are synthesized from FETCH responses so the pull
/// loop's logic is identical regardless of backend.
///
/// `extra` carries source-specific metadata that doesn't fit in the
/// generic summary (e.g. REST's `thread_id`, `folder_path`, `labels`).
/// IMAP-sourced messages leave it `None`; the pull loop tolerates missing
/// fields in the sidecar meta.json.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub summary: MessageSummary,
    pub raw: Vec<u8>,
    pub extra: Option<serde_json::Value>,
}

/// Read-only abstraction for pulling mail state.
///
/// Implementations must be cheap to clone (use Arc internally for
/// connection state).
#[async_trait]
pub trait MailSource: Send + Sync {
    /// Human-readable tag identifying this source (e.g. "rest" or "imap").
    /// Used in commit messages and logs so history makes clear where the
    /// data came from.
    fn tag(&self) -> &'static str;

    /// List all folders on the alias.
    async fn list_folders(&self) -> Result<Vec<Folder>, Error>;

    /// List message summaries in a folder (paginated transparently to
    /// the caller). Summaries must include the fields the pull loop diffs
    /// on: `id`, `modseq`, `flags`, `updated_at`.
    async fn list_messages(&self, folder: &str) -> Result<Vec<MessageSummary>, Error>;

    /// Fetch a single full message including its raw RFC822 bytes.
    async fn fetch_message(&self, folder: &str, id: &str) -> Result<FetchedMessage, Error>;
}
