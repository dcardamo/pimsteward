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
                let status = extract_ical_status(&ical);
                // The forwardemail REST `id` field is the last path segment
                // of the href, minus the .ics extension.
                let href_id = r
                    .href
                    .rsplit('/')
                    .next()
                    .map(|s| s.trim_end_matches(".ics").to_string())
                    .unwrap_or_default();
                // Populate the derived fields from the iCal payload.
                // Previous behaviour was to leave summary/description/
                // location/start_date/end_date as `None`, which made the
                // forwardemail CalDAV path opaque to MCP `list_events`
                // window filters: every event had `start_date == None`,
                // every windowed query returned the empty list, and the
                // dan@hld.ca calendar appeared empty in the daily brief
                // even when it had events. The shared `vevent_field`
                // helper is `VEVENT`-scoped (Fastmail puts `VTIMEZONE`
                // first, so an unscoped grep would otherwise return
                // 1895-era timezone-transition timestamps as `DTSTART`).
                out.push(CalendarEvent {
                    id: href_id,
                    uid,
                    calendar_id: Some(cal_id.clone()),
                    summary: crate::ical::vevent_field(&ical, "SUMMARY"),
                    description: crate::ical::vevent_field(&ical, "DESCRIPTION"),
                    location: crate::ical::vevent_field(&ical, "LOCATION"),
                    start_date: crate::ical::vevent_field(&ical, "DTSTART"),
                    end_date: crate::ical::vevent_field(&ical, "DTEND"),
                    etag: r.etag,
                    ical: Some(ical),
                    status,
                    created_at: None,
                    updated_at: None,
                });
            }
        }
        Ok(out)
    }
}

/// Extract the first `UID` line from a `VEVENT` in an iCalendar blob.
/// Thin convenience wrapper over [`crate::ical::vevent_field`] — kept
/// for the call sites in this module that want a focused name.
fn extract_ical_uid(ics: &str) -> Option<String> {
    crate::ical::vevent_field(ics, "UID")
}

/// Extract the first `STATUS` line from a `VEVENT`. Returns values
/// like `"CONFIRMED"`, `"TENTATIVE"`, or `"CANCELLED"`.
fn extract_ical_status(ics: &str) -> Option<String> {
    crate::ical::vevent_field(ics, "STATUS")
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

    #[test]
    fn extract_status_cancelled() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nSTATUS:CANCELLED\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_status(ics), Some("CANCELLED".into()));
    }

    #[test]
    fn extract_status_confirmed() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nSTATUS:CONFIRMED\nSUMMARY:Hi\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_status(ics), Some("CONFIRMED".into()));
    }

    #[test]
    fn extract_status_absent() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nSUMMARY:Hi\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_status(ics), None);
    }

    /// Direct shape check for the `Fastmail` payload layout (VTIMEZONE
    /// before VEVENT, parametered DTSTART with TZID). The previous
    /// `list_events` impl hardcoded `start_date: None`, which made every
    /// dan@hld.ca calendar event invisible to MCP window filters. The
    /// fields below must round-trip out of `vevent_field` so the
    /// `CalendarEvent` returned by the caldav source carries usable
    /// dates.
    #[test]
    fn fastmail_style_payload_yields_full_event_fields() {
        let ics = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "BEGIN:VTIMEZONE\r\n",
            "TZID:America/Toronto\r\n",
            "BEGIN:STANDARD\r\n",
            "DTSTART:18950101T000000\r\n",
            "RRULE:FREQ=YEARLY;UNTIL=19230513T070000Z;BYMONTH=5\r\n",
            "END:STANDARD\r\n",
            "END:VTIMEZONE\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:b28741c0\r\n",
            "SUMMARY:🔧 Rivian Key Drop-off\r\n",
            "DTSTART;TZID=America/Toronto:20260214T131000\r\n",
            "DTEND;TZID=America/Toronto:20260214T133000\r\n",
            "LOCATION:5720 Rue Ferrier\\, Mount Royal\r\n",
            "STATUS:CONFIRMED\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );

        // The shared helper is what list_events now calls when building
        // the CalendarEvent. Pin the values so a regression in the
        // extractor surfaces here as well as in src/ical.rs.
        assert_eq!(
            crate::ical::vevent_field(ics, "DTSTART").as_deref(),
            Some("20260214T131000"),
        );
        assert_eq!(
            crate::ical::vevent_field(ics, "DTEND").as_deref(),
            Some("20260214T133000"),
        );
        assert_eq!(
            crate::ical::vevent_field(ics, "SUMMARY").as_deref(),
            Some("🔧 Rivian Key Drop-off"),
        );
        assert_eq!(
            crate::ical::vevent_field(ics, "LOCATION").as_deref(),
            Some("5720 Rue Ferrier\\, Mount Royal"),
        );
        assert_eq!(extract_ical_uid(ics).as_deref(), Some("b28741c0"));
        assert_eq!(extract_ical_status(ics).as_deref(), Some("CONFIRMED"));
    }
}
