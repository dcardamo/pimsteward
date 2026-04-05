//! Calendar endpoints wrapper (`/v1/calendars`, `/v1/calendar-events`).
//!
//! Calendar event creation payload shape is still unresolved from the smoke
//! test (see `docs/api-findings.md` unresolved section). Reading/listing
//! works fine — that's what the v1 pull loop uses.

use crate::error::Error;
use crate::forwardemail::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Calendar event as returned by `/v1/calendar-events`. Shape inferred from
/// the contacts endpoint (same forwardemail design pattern): a `content`
/// field holds the raw iCal text and `etag` is the CalDAV etag.
///
/// Fields are all optional to tolerate shape drift — the pull loop only
/// actually uses `id`, `content`, and `etag`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub calendar_id: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub dtstart: Option<String>,
    #[serde(default)]
    pub dtend: Option<String>,
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
}
