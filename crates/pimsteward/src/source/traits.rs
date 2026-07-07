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
///
/// Returned `CalendarEvent` fields:
/// - **forwardemail:** the server-normalised response — every derived field
///   (`summary`, `start_date`, `end_date`, `created_at`, `updated_at`, …) is
///   populated by forwardemail's server-side iCal parser, and
///   `start_date` / `end_date` are RFC-3339 timestamps.
/// - **iCloud:** synthesized from the request — `id` is the `.ics` filename
///   tail, `etag` is the response ETag header, and `summary`/`location`/
///   `status`/`start_date`/`end_date` are parsed out of the caller's iCal.
///   `created_at`/`updated_at` are `None` since CalDAV does not expose
///   server-side timestamps for events. **Format difference:**
///   `start_date` / `end_date` are the raw iCal value-portion
///   (e.g. `"20270115T100000Z"` or floating `"20270115T100000"`), NOT
///   RFC-3339 — and any `TZID=…` parameter is stripped, so timezone is
///   lost on non-UTC events. Consumers that need timezone fidelity must
///   parse the full `ical` field. This is a documented loss; preserving
///   TZID would require a full iCalendar grammar.
#[async_trait]
pub trait CalendarWriter: Send + Sync {
    fn tag(&self) -> &'static str;
    /// Create a new calendar event and return the post-create event state
    /// (see trait docs for which fields each backend populates).
    async fn create_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
    ) -> Result<CalendarEvent, Error>;
    /// Update an existing calendar event by uid. `if_match` carries an
    /// etag for optimistic concurrency on backends that support it; pass
    /// `""` on backends that don't.
    async fn update_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
        if_match: &str,
    ) -> Result<CalendarEvent, Error>;
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

// ── Sieve ───────────────────────────────────────────────────────────

/// Read/write abstraction over a Sieve script store. Two implementations:
/// - **forwardemail (REST):** script CRUD over the `/v1/sieve-scripts`
///   REST API, activation over ManageSieve (the REST `is_active` field
///   is read-only).
/// - **Stalwart:** full CRUD + activation over ManageSieve (RFC 5804),
///   with STARTTLS on port 4190. No REST surface.
///
/// The trait is script-name keyed (not backend-id keyed) because
/// ManageSieve has no stable opaque ids — `LISTSCRIPTS` returns names
/// only, and `GETSCRIPT`/`PUTSCRIPT`/`DELETESCRIPT`/`SETACTIVE` all
/// key on the script name. The forwardemail REST surface is also
/// name-keyed at this layer (the REST `id` is retained in
/// [`SieveScriptMeta`] for the audit-trail meta.json but is not used
/// as the dispatch key).
#[async_trait]
pub trait SieveBackend: Send + Sync {
    fn tag(&self) -> &'static str;
    /// List every script and whether it's currently active.
    async fn list_scripts(&self) -> Result<Vec<SieveScriptMeta>, Error>;
    /// Fetch the full content of one script by name. Returns
    /// `Error::Api{status:404}` if no script with that name exists.
    async fn get_script(&self, name: &str) -> Result<SieveScriptMeta, Error>;
    /// Create or replace a script's content. Stalwart's ManageSieve
    /// `PUTSCRIPT` is an upsert (create-or-replace by name); the FE
    /// REST surface is also an upsert if the caller first checks the
    /// existing list — implementations may transparently fall back to
    /// update when a create returns a 422 "already exists".
    ///
    /// The returned [`SieveScriptMeta`] reflects the post-write state.
    /// `is_valid` / `validation_errors` are populated when the backend
    /// parses the script server-side (FE REST does; Stalwart
    /// ManageSieve does NOT — it accepts bytes and reports syntax
    /// errors only via the NO response code, which is surfaced as a
    /// 422 `Error::Api`).
    async fn put_script(
        &self,
        name: &str,
        content: &str,
    ) -> Result<SieveScriptMeta, Error>;
    /// Delete a script by name. Idempotent — deleting a non-existent
    /// script returns `Ok(())`.
    async fn delete_script(&self, name: &str) -> Result<(), Error>;
    /// Activate a script by name (deactivates any previously active
    /// script). Pass an empty string to deactivate all scripts.
    async fn activate_script(&self, name: &str) -> Result<(), Error>;
    /// Return the name of the currently active script, or `None` if
    /// no script is active.
    async fn get_active(&self) -> Result<Option<String>, Error>;
}

/// Backend-neutral view of one sieve script. The `id` field is the
/// backend's opaque identifier (FE REST id; Stalwart uses the script
/// name) — kept for the audit-trail meta.json but not used as a
/// dispatch key by [`SieveBackend`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SieveScriptMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    /// True when the backend reports the script parses cleanly.
    /// Stalwart's ManageSieve has no parse step on PUTSCRIPT, so
    /// `is_valid` is `true` on a successful put and `false` is
    /// signalled via a 422 `Error::Api` instead.
    #[serde(default)]
    pub is_valid: bool,
    #[serde(default)]
    pub validation_errors: Vec<String>,
}
