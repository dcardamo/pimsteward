//! Source traits. One trait per resource type. Implementations may cover
//! one or more resources — e.g. the REST client implements all of them,
//! the IMAP source implements only `MailSource`, the DAV sources
//! implement calendar + contacts.

use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::forwardemail::contacts::Contact;
use crate::forwardemail::mail::{Folder, MessageSummary};
use async_trait::async_trait;

// ── Mail ────────────────────────────────────────────────────────────

/// A fetched message: the raw RFC822 bytes plus the forwardemail-shape
/// summary fields that the pull loop uses as diff keys. IMAP-sourced
/// messages synthesize these from FETCH responses so the pull loop logic
/// is identical regardless of backend.
///
/// `extra` carries source-specific metadata (REST's `thread_id`,
/// `folder_path`, `labels`, etc.). IMAP leaves it `None`; the pull loop
/// tolerates missing fields in the sidecar meta.json.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub summary: MessageSummary,
    pub raw: Vec<u8>,
    pub extra: Option<serde_json::Value>,
}

/// Result of enumerating a folder. The split between `all_ids` and
/// `changed` is what lets CONDSTORE-capable sources (IMAP) return only
/// messages that have changed since the caller's last sync, while still
/// giving the caller enough information to detect deletions.
#[derive(Debug, Clone, Default)]
pub struct ListResult {
    /// Authoritative set of message ids currently in the folder. The
    /// caller diffs this against local state to detect deletions.
    pub all_ids: Vec<String>,
    /// Message summaries the caller should consider for refetch. If the
    /// source doesn't support CONDSTORE (or the caller passed
    /// `since_modseq = None`), this contains every message. With a
    /// CHANGEDSINCE hint, it's the server-filtered delta.
    pub changed: Vec<MessageSummary>,
    /// New HIGHESTMODSEQ observed. Callers persist this and pass it back
    /// as `since_modseq` next time. `None` means the source doesn't
    /// surface a mailbox-level modseq (REST).
    pub highest_modseq: Option<i64>,
    /// UIDVALIDITY at fetch time. If this differs from the caller's
    /// stored value, the stored `since_modseq` is invalid and the caller
    /// must do a full resync.
    pub uid_validity: Option<i64>,
}

#[async_trait]
pub trait MailSource: Send + Sync {
    fn tag(&self) -> &'static str;
    async fn list_folders(&self) -> Result<Vec<Folder>, Error>;
    /// Enumerate a folder. `since_modseq` and `uid_validity` are hints
    /// from the caller's previous sync — sources that support CONDSTORE
    /// use them to reduce the FETCH to only changed messages. Sources
    /// that don't may ignore them. If `uid_validity` doesn't match the
    /// server's current value, the source MUST ignore `since_modseq` and
    /// return a full list.
    async fn list_messages(
        &self,
        folder: &str,
        since_modseq: Option<i64>,
        uid_validity: Option<i64>,
    ) -> Result<ListResult, Error>;
    async fn fetch_message(&self, folder: &str, id: &str) -> Result<FetchedMessage, Error>;
}

/// Write-side trait for mail mutations: flag updates, folder moves,
/// deletes, and draft creation. Complements [`MailSource`] (read-side).
/// REST implements it via the forwardemail REST API; IMAP implements it
/// via STORE/MOVE/EXPUNGE commands.
#[async_trait]
pub trait MailWriter: Send + Sync {
    fn tag(&self) -> &'static str;
    /// Replace a message's entire flag set. `folder` is the message's
    /// current folder — IMAP needs it to SELECT before STORE; REST
    /// ignores it (the id is globally unique).
    async fn update_flags(
        &self,
        folder: &str,
        id: &str,
        flags: &[String],
    ) -> Result<(), Error>;
    /// Move a message to a different folder. `source_folder` is the
    /// current location — IMAP needs it for SELECT; REST ignores it.
    async fn move_message(
        &self,
        source_folder: &str,
        id: &str,
        target_folder: &str,
    ) -> Result<(), Error>;
    /// Delete a message. `folder` is the current folder.
    async fn delete_message(&self, folder: &str, id: &str) -> Result<(), Error>;
}

// ── Calendar ───────────────────────────────────────────────────────

/// Read-only abstraction for pulling calendar state. Implementations return
/// forwardemail-shape types so the pull loop and storage layout are
/// identical across backends.
#[async_trait]
pub trait CalendarSource: Send + Sync {
    fn tag(&self) -> &'static str;
    /// List all calendars accessible to the authenticated alias.
    async fn list_calendars(&self) -> Result<Vec<Calendar>, Error>;
    /// List all events from all calendars (or a specific calendar if
    /// `calendar_id` is provided). Each event includes its raw iCalendar
    /// text in the `ical` field.
    async fn list_events(&self, calendar_id: Option<&str>) -> Result<Vec<CalendarEvent>, Error>;
}

/// Write-side abstraction for calendar event mutations. Mirrors the
/// `IcloudCalendarWriter`-shaped surface used by Task 6's MCP refactor.
///
/// Identifier semantics differ across backends:
/// - **forwardemail (REST):** `calendar_id` is forwardemail's stable
///   calendar id, and `uid` is forwardemail's per-event eventId. The REST
///   API does not surface ETags for events, so the `if_match` argument is
///   ignored on update/delete (callers should pass `""`).
/// - **iCloud (CalDAV):** `calendar_id` is the calendar's collection URL
///   discovered via RFC 6764, and `uid` is the iCalendar UID (also the
///   `.ics` filename tail). `if_match` is honored strictly — empty or
///   stale values produce `Error::PreconditionFailed`.
#[async_trait]
pub trait CalendarWriter: Send + Sync {
    fn tag(&self) -> &'static str;
    /// Create a new calendar event. Returns the new event's identifier
    /// (forwardemail eventId for REST, ETag string for iCloud — both are
    /// opaque to the caller).
    async fn create_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
    ) -> Result<String, Error>;
    /// Update an existing calendar event by uid. `if_match` carries an
    /// etag for optimistic concurrency on backends that support it; pass
    /// `""` on backends that don't.
    async fn update_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
        if_match: &str,
    ) -> Result<String, Error>;
    /// Delete a calendar event by uid. `if_match` semantics match
    /// [`Self::update_event`].
    async fn delete_event(
        &self,
        calendar_id: &str,
        uid: &str,
        if_match: &str,
    ) -> Result<(), Error>;
}

// ── Contacts ────────────────────────────────────────────────────────

#[async_trait]
pub trait ContactsSource: Send + Sync {
    fn tag(&self) -> &'static str;
    /// List all contacts for the authenticated alias. Each contact
    /// includes the raw vCard in `content` and the CardDAV etag in `etag`.
    async fn list_contacts(&self) -> Result<Vec<Contact>, Error>;
}
