//! `.eml` → `MessageRow` conversion.
//!
//! Separated from `mod.rs` so the MIME/mailparse side is testable in
//! isolation from the SQL side.  Implemented in phase 2 — this file
//! currently only defines the `MessageRow` shape the index consumes.

/// A single row destined for `messages` + `messages_body`, derived from
/// the raw RFC822 bytes plus the on-disk `MessageMeta` sidecar.  The
/// envelope parser populates everything; the pull loop and rebuild call
/// site decide what `folder` to attribute it to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageRow {
    pub canonical_id: String,
    pub folder: String,
    pub source_id: String,
    pub message_id: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub to_addrs: Option<String>,
    pub cc_addrs: Option<String>,
    pub subject: Option<String>,
    /// Unix timestamp, seconds.  Pulled from the `Date:` header when
    /// present; otherwise from the `internal_date` field in MessageMeta.
    pub date_unix: Option<i64>,
    pub size: Option<i64>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    /// Extracted plain-text body, truncated to [`super::BODY_CAP_BYTES`].
    pub body_text: Option<String>,
}
