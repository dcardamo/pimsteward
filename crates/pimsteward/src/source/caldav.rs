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
use crate::source::dav::{DavClient, DavConfig, DavPrecondition};
use crate::source::traits::{CalendarSource, CalendarWriter};
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
                // dan@example.test calendar appeared empty in the daily brief
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

/// Deterministic per-event object URL: `<collection>/<uid>.ics`.
///
/// `collection` is the calendar collection URL (or path) and `uid` is the
/// iCalendar UID. The UID is path-segment-encoded so a UID containing
/// reserved characters (`/`, `?`, `#`, space, …) maps to a single, safe
/// path segment. The mapping is pure and deterministic: the same
/// `(collection, uid)` always produces the same href, which is what makes
/// re-PUTting the same UID overwrite the existing object rather than
/// creating a duplicate.
fn caldav_object_href(collection: &str, uid: &str) -> String {
    format!(
        "{}/{}.ics",
        collection.trim_end_matches('/'),
        encode_uid_segment(uid)
    )
}

/// Percent-encode a UID into a single safe path segment. Conservative
/// allowlist: unreserved RFC-3986 characters plus the common id punctuation
/// (`-._~`) pass through untouched (so a plain UID like `ABC-123` is
/// unchanged); everything else is percent-encoded. Kept dependency-free and
/// local so both the CalDAV and CardDAV writers share one definition.
pub(crate) fn encode_uid_segment(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    for b in uid.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ─── Writer ─────────────────────────────────────────────────────────────

/// Generic, standards-compliant CalDAV writer.
///
/// Implements [`CalendarWriter`] for any RFC-4791 server (Stalwart,
/// Fastmail, Radicale, …) by mapping each event to a deterministic
/// `<collection>/<uid>.ics` object and writing it with HTTP PUT/DELETE via
/// the shared [`DavClient`]. Identifier semantics match the iCloud writer:
/// the trait's `calendar_id` is the calendar collection URL (or path) and
/// `uid` is the iCalendar UID (also the `.ics` filename tail). `if_match`
/// is honored strictly — a stale/empty etag on update/delete surfaces as
/// [`Error::PreconditionFailed`].
///
/// Idempotency: create uses `If-None-Match: *`, update uses `If-Match`, but
/// the href is a pure function of the UID, so a re-PUT of the same UID
/// overwrites the existing object — no duplicate is ever produced.
pub struct DavCalendarWriter {
    client: DavClient,
    user: String,
}

impl std::fmt::Debug for DavCalendarWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavCalendarWriter")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl DavCalendarWriter {
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

    /// PUT the iCal body to `<collection>/<uid>.ics` and synthesize the
    /// post-write `CalendarEvent` from the caller's iCal plus the response
    /// etag. Shared by create (`If-None-Match: *`) and update (`If-Match`).
    async fn put_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
        precondition: DavPrecondition,
    ) -> Result<CalendarEvent, Error> {
        let href = caldav_object_href(calendar_id, uid);
        let etag = self
            .client
            .put_object(
                &href,
                ical.as_bytes(),
                "text/calendar; charset=utf-8",
                precondition,
            )
            .await?;
        Ok(synthesize_event(calendar_id, uid, ical, etag))
    }
}

#[async_trait]
impl CalendarWriter for DavCalendarWriter {
    fn tag(&self) -> &'static str {
        "caldav-writer"
    }

    async fn create_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
    ) -> Result<CalendarEvent, Error> {
        self.put_event(calendar_id, uid, ical, DavPrecondition::IfNoneMatch)
            .await
    }

    async fn update_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
        if_match: &str,
    ) -> Result<CalendarEvent, Error> {
        // An empty if_match means "no precondition" — the caller didn't
        // hold an etag (e.g. an upsert). A non-empty value is honored
        // strictly for optimistic concurrency.
        let precondition = if if_match.is_empty() {
            DavPrecondition::None
        } else {
            DavPrecondition::IfMatch(if_match.to_string())
        };
        self.put_event(calendar_id, uid, ical, precondition).await
    }

    async fn delete_event(
        &self,
        calendar_id: &str,
        uid: &str,
        if_match: &str,
    ) -> Result<(), Error> {
        let href = caldav_object_href(calendar_id, uid);
        let precondition = if if_match.is_empty() {
            DavPrecondition::None
        } else {
            DavPrecondition::IfMatch(if_match.to_string())
        };
        self.client.delete_object(&href, precondition).await
    }
}

/// Build a `CalendarEvent` from a successful CalDAV PUT — the caller's iCal
/// is the canonical text (CalDAV PUT returns an empty body), so we extract
/// the derived fields from it, mirroring the iCloud writer's
/// `synthesize_event`. `id` is the `.ics` filename tail (`<uid>.ics`) and
/// `etag` is the response etag, if the server returned one.
fn synthesize_event(
    calendar_id: &str,
    uid: &str,
    ical: &str,
    etag: Option<String>,
) -> CalendarEvent {
    CalendarEvent {
        id: format!("{}.ics", encode_uid_segment(uid)),
        uid: extract_ical_uid(ical).or_else(|| Some(uid.to_string())),
        calendar_id: Some(calendar_id.to_string()),
        summary: crate::ical::vevent_field(ical, "SUMMARY"),
        description: crate::ical::vevent_field(ical, "DESCRIPTION"),
        location: crate::ical::vevent_field(ical, "LOCATION"),
        start_date: crate::ical::vevent_field(ical, "DTSTART"),
        end_date: crate::ical::vevent_field(ical, "DTEND"),
        etag,
        ical: Some(ical.to_string()),
        status: extract_ical_status(ical),
        created_at: None,
        updated_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caldav_object_href_is_deterministic() {
        let base = "https://h:8443/dav/cal/dan%40example.test/default";
        assert_eq!(
            caldav_object_href(base, "ABC-123"),
            "https://h:8443/dav/cal/dan%40example.test/default/ABC-123.ics"
        );
        assert_eq!(
            caldav_object_href(base, "ABC-123"),
            caldav_object_href(base, "ABC-123")
        );
    }

    #[test]
    fn caldav_object_href_trims_trailing_slash() {
        assert_eq!(
            caldav_object_href("https://h/cal/default/", "evt-1"),
            "https://h/cal/default/evt-1.ics"
        );
    }

    #[test]
    fn caldav_object_href_encodes_reserved_uid_chars() {
        // A UID with a slash/space must not break out of the path segment.
        assert_eq!(
            caldav_object_href("https://h/cal", "a/b c"),
            "https://h/cal/a%2Fb%20c.ics"
        );
        // Plain id punctuation passes through untouched.
        assert_eq!(
            encode_uid_segment("ABC-123_x.y~z"),
            "ABC-123_x.y~z"
        );
    }

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
    /// dan@example.test calendar event invisible to MCP window filters. The
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
