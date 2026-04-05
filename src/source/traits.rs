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

#[async_trait]
pub trait MailSource: Send + Sync {
    fn tag(&self) -> &'static str;
    async fn list_folders(&self) -> Result<Vec<Folder>, Error>;
    async fn list_messages(&self, folder: &str) -> Result<Vec<MessageSummary>, Error>;
    async fn fetch_message(&self, folder: &str, id: &str) -> Result<FetchedMessage, Error>;
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

// ── Contacts ────────────────────────────────────────────────────────

#[async_trait]
pub trait ContactsSource: Send + Sync {
    fn tag(&self) -> &'static str;
    /// List all contacts for the authenticated alias. Each contact
    /// includes the raw vCard in `content` and the CardDAV etag in `etag`.
    async fn list_contacts(&self) -> Result<Vec<Contact>, Error>;
}
