//! The `CalendarEvent` DTO — the shape returned by forwardemail's
//! `/v1/calendar-events` endpoint and produced by the CalDAV sources.
//!
//! Lives here, in the dependency-light crate, so both pimsteward and a
//! host-side ICS feed builder can construct and consume events without
//! sharing any network/storage/credential code. The HTTP client methods
//! that fetch/create/update these events stay in the pimsteward crate.

use serde::{Deserialize, Serialize};

/// Calendar event as returned by `/v1/calendar-events`. The raw iCalendar
/// text lives in `ical`; forwardemail parses it server-side and surfaces
/// convenience fields (summary, start/end, etc.) for humans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// eventId — this is what the URL uses as `:id` on GET/PUT/DELETE.
    pub id: String,
    /// iCalendar UID from the VEVENT component.
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub calendar_id: Option<String>,
    /// Raw iCalendar text — the authoritative representation. Store
    /// verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ical: Option<String>,
    /// CalDAV `getetag` value. Present when events are pulled via the
    /// CalDAV source; absent for REST pulls (forwardemail's REST API
    /// does not return ETags for calendar events). Used for optimistic
    /// concurrency control on writes (If-Match header) when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}
