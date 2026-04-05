//! CalDAV-backed CalendarSource.
//!
//! Discovers calendars via PROPFIND on the alias's DAV home, then
//! enumerates events in each via REPORT `calendar-query`. Each REPORT
//! returns all events with their etags and iCalendar bodies in one
//! round trip — significantly cheaper than REST's list+per-item-GET
//! pattern at high event counts.
//!
//! Live-tested against `caldav.forwardemail.net` with a forwardemail
//! alias. Not production-hardened against arbitrary CalDAV servers —
//! pimsteward's scope is forwardemail, so we match the quirks of that
//! specific server (e.g. href format, namespace prefixes).

use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::source::dav::{DavClient, DavConfig};
use crate::source::traits::CalendarSource;
use async_trait::async_trait;

pub struct DavCalendarSource {
    client: DavClient,
    user: String,
}

impl std::fmt::Debug for DavCalendarSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavCalendarSource")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl DavCalendarSource {
    pub fn new(
        base_url: impl Into<String>,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, Error> {
        let user = user.into();
        let client = DavClient::new(DavConfig {
            base_url: base_url.into(),
            user: user.clone(),
            password: password.into(),
        })?;
        Ok(Self { client, user })
    }

    /// Build the URL path for the alias's DAV home on forwardemail.
    fn home_path(&self) -> String {
        format!("/dav/{}/", self.user)
    }
}

#[async_trait]
impl CalendarSource for DavCalendarSource {
    fn tag(&self) -> &'static str {
        "caldav"
    }

    async fn list_calendars(&self) -> Result<Vec<Calendar>, Error> {
        // PROPFIND depth=1 on /dav/<user>/ — returns the home collection
        // plus one response per calendar collection beneath it.
        let body = r#"<?xml version="1.0"?>
<D:propfind xmlns:D="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <CAL:supported-calendar-component-set/>
  </D:prop>
</D:propfind>"#;
        let ms = self.client.propfind(&self.home_path(), 1, body).await?;

        Ok(ms
            .responses
            .into_iter()
            .filter(|r| r.is_calendar)
            .map(|r| {
                // Calendar id = the last path segment of the href (the
                // forwardemail ObjectId, same one the REST API returns).
                let id = r
                    .href
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                Calendar {
                    id,
                    name: r.displayname.unwrap_or_default(),
                    description: String::new(),
                    color: String::new(),
                    timezone: String::new(),
                    order: None,
                    created_at: None,
                    updated_at: None,
                }
            })
            .collect())
    }

    async fn list_events(&self, calendar_id: Option<&str>) -> Result<Vec<CalendarEvent>, Error> {
        // When no calendar_id is given, enumerate all calendars first and
        // query each one. REPORT scope is per-collection, not global.
        let calendar_ids: Vec<String> = match calendar_id {
            Some(id) => vec![id.to_string()],
            None => self
                .list_calendars()
                .await?
                .into_iter()
                .map(|c| c.id)
                .collect(),
        };

        let body = r#"<?xml version="1.0"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>"#;

        let mut out = Vec::new();
        for cal_id in calendar_ids {
            let path = format!("/dav/{}/{}/", self.user, cal_id);
            let ms = self.client.report(&path, 1, body).await?;
            for r in ms.responses {
                let Some(ical) = r.calendar_data else {
                    continue;
                };
                // Extract the VEVENT UID from the iCal text — pimsteward
                // uses UID as the canonical event identifier, same as the
                // REST source.
                let uid = extract_ical_uid(&ical);
                // The forwardemail REST `id` field is the last path segment
                // of the href, minus the .ics extension.
                let href_id = r
                    .href
                    .rsplit('/')
                    .next()
                    .map(|s| s.trim_end_matches(".ics").to_string())
                    .unwrap_or_default();
                out.push(CalendarEvent {
                    id: href_id,
                    uid,
                    calendar_id: Some(cal_id.clone()),
                    ical: Some(ical),
                    summary: None,
                    description: None,
                    location: None,
                    start_date: None,
                    end_date: None,
                    status: None,
                    created_at: None,
                    updated_at: None,
                });
            }
        }
        Ok(out)
    }
}

/// Extract the first UID: line from a VEVENT in an iCalendar blob. Minimal
/// parser — good enough for forwardemail output.
fn extract_ical_uid(ics: &str) -> Option<String> {
    // Find the first VEVENT block and look for a UID: line inside it.
    let mut in_vevent = false;
    for line in ics.lines() {
        let l = line.trim();
        if l.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_vevent = true;
        } else if l.eq_ignore_ascii_case("END:VEVENT") {
            in_vevent = false;
        } else if in_vevent && l.to_ascii_uppercase().starts_with("UID:") {
            return Some(l[4..].trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_uid_from_vevent() {
        let ics =
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc-123\nSUMMARY:Hi\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_uid(ics), Some("abc-123".into()));
    }

    #[test]
    fn extract_uid_ignores_vcalendar_level_uid() {
        // Some forwardemail responses include a calendar-level UID: header
        // outside the VEVENT. We should skip it and take the VEVENT UID.
        let ics = "BEGIN:VCALENDAR\nUID:cal-level\nBEGIN:VEVENT\nUID:event-level\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_uid(ics), Some("event-level".into()));
    }
}
