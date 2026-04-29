//! Calendar endpoints wrapper (`/v1/calendars`, `/v1/calendar-events`).
//!
//! Shape verified against forwardemail's open-source server code (see
//! app/controllers/api/v1/calendar-events.js in forwardemail/forwardemail.net).
//!
//! **Important quirk:** events use an `ical` field for the raw iCalendar
//! payload, NOT `content` like contacts. The API is inconsistent between
//! resource types. The create payload is:
//!
//! ```json
//! { "calendar_id": "<id>", "ical": "BEGIN:VCALENDAR\n...", "event_id": "optional" }
//! ```
//!
//! The response json() function at that path exposes these fields:
//! id (eventId), calendar_id, ical, summary, description, location,
//! start_date, end_date, uid, status, organizer, created_at, updated_at,
//! deleted_at, object="calendar_event".

use crate::error::Error;
use crate::forwardemail::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// May be null for CalDAV-synced calendars.
    #[serde(default, deserialize_with = "crate::forwardemail::calendar::deser_nullable_string")]
    pub description: String,
    #[serde(default)]
    pub color: String,
    /// May contain a full VTIMEZONE iCalendar blob for CalDAV-synced
    /// calendars, or a simple IANA timezone string for REST-created ones.
    #[serde(default, deserialize_with = "crate::forwardemail::calendar::deser_nullable_string")]
    pub timezone: String,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Deserialize a JSON string-or-null into a `String`, mapping `null` to `""`.
/// Forwardemail's API returns `null` for optional text fields on calendars
/// synced via CalDAV (e.g. description), but `#[serde(default)]` only
/// handles *missing* keys, not explicit null values.
fn deser_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

/// Calendar event as returned by `/v1/calendar-events`. The raw iCalendar
/// text lives in `ical`; forwardemail parses it server-side and surfaces
/// convenience fields (summary, start/end, etc.) for humans.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Client {
    /// GET /v1/calendars
    pub async fn list_calendars(&self) -> Result<Vec<Calendar>, Error> {
        self.get_json("/v1/calendars?limit=50").await
    }

    /// POST /v1/calendars — create a new calendar.
    pub async fn create_calendar(
        &self,
        name: &str,
        color: Option<&str>,
    ) -> Result<Calendar, Error> {
        let mut body = serde_json::Map::new();
        body.insert("name".into(), serde_json::Value::String(name.into()));
        if let Some(c) = color {
            body.insert("color".into(), serde_json::Value::String(c.into()));
        }
        self.post_json("/v1/calendars", &serde_json::Value::Object(body))
            .await
    }

    /// DELETE /v1/calendars/:id
    pub async fn delete_calendar(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/calendars/{id}")).await
    }

    /// GET /v1/calendar-events — paginated. If a calendar id is provided,
    /// filter to just that calendar.
    pub async fn list_calendar_events(
        &self,
        calendar_id: Option<&str>,
    ) -> Result<Vec<CalendarEvent>, Error> {
        let mut out = Vec::new();
        let mut page = 1usize;
        loop {
            let mut path = format!("/v1/calendar-events?page={page}&limit=50");
            if let Some(c) = calendar_id {
                path.push_str(&format!("&calendar_id={c}"));
            }
            let chunk: Vec<CalendarEvent> = self.get_json(&path).await?;
            if chunk.is_empty() {
                break;
            }
            let got = chunk.len();
            out.extend(chunk);
            if got < 50 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// GET /v1/calendar-events/:id
    pub async fn get_calendar_event(&self, id: &str) -> Result<CalendarEvent, Error> {
        self.get_json(&format!("/v1/calendar-events/{id}")).await
    }

    /// POST /v1/calendar-events — create a calendar event.
    ///
    /// `calendar_id` is the calendar's id (UUID or ObjectId), not its name.
    /// `ical` is the full iCalendar text including BEGIN:VCALENDAR /
    /// END:VCALENDAR wrappers and a VEVENT component. The server normalizes
    /// the ics and returns the stored representation.
    pub async fn create_calendar_event(
        &self,
        calendar_id: &str,
        ical: &str,
        event_id: Option<&str>,
    ) -> Result<CalendarEvent, Error> {
        let mut body = serde_json::Map::new();
        body.insert(
            "calendar_id".into(),
            serde_json::Value::String(calendar_id.into()),
        );
        body.insert("ical".into(), serde_json::Value::String(ical.into()));
        if let Some(eid) = event_id {
            body.insert("event_id".into(), serde_json::Value::String(eid.into()));
        }
        self.post_json("/v1/calendar-events", &serde_json::Value::Object(body))
            .await
    }

    /// PUT /v1/calendar-events/:id — update the iCal payload and/or
    /// move the event to a different calendar. At least one of `ical`
    /// or `calendar_id` should be provided.
    pub async fn update_calendar_event(
        &self,
        id: &str,
        ical: Option<&str>,
        calendar_id: Option<&str>,
        if_match: Option<&str>,
    ) -> Result<CalendarEvent, Error> {
        let mut body = serde_json::Map::new();
        if let Some(i) = ical {
            body.insert("ical".into(), serde_json::Value::String(i.into()));
        }
        if let Some(c) = calendar_id {
            body.insert("calendar_id".into(), serde_json::Value::String(c.into()));
        }
        self.put_json(
            &format!("/v1/calendar-events/{id}"),
            &serde_json::Value::Object(body),
            if_match,
        )
        .await
    }

    /// DELETE /v1/calendar-events/:id
    pub async fn delete_calendar_event(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/calendar-events/{id}")).await
    }
}
