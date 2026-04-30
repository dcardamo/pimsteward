//! iCloud-flavoured CalDAV source and writer.
//!
//! Layers iCloud quirks on top of the generic CalDAV transport in
//! `src/source/dav.rs`:
//!
//! - User-Agent set on every request (iCloud 403s an empty UA).
//! - RFC 6764 discovery via `crate::icloud::discovery::discover` (cached).
//! - 200-OR-201 success normalisation on PUT (iCloud sometimes returns
//!   `200 OK` for what should be `201 Created`).
//! - 412 Precondition Failed surfaced as the structured
//!   `Error::PreconditionFailed { etag }` so MCP callers can give users a
//!   "re-read and retry" message rather than a generic HTTP error.
//! - Discovery cache invalidation on 4xx responses against a cached
//!   calendar URL (404 / 410 / redirect-to-elsewhere). 5xx is treated as
//!   a transient server hiccup and does NOT invalidate.
//!
//! `IcloudCalendarWriter` is wired through `Provider::build_calendar_writer`
//! since b4d7e07 — the MCP layer dispatches calendar mutations through the
//! `CalendarWriter` trait without needing to know which provider is active.

use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::icloud::discovery::{self, DiscoveredCalendar};
use crate::source::dav::DavMultistatus;
use crate::source::traits::CalendarSource;
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use std::time::Duration;
use tokio::sync::Mutex;

/// CalDAV `calendar-query` REPORT body — fetches every VEVENT in the
/// target collection along with its etag and raw iCalendar payload in a
/// single round trip.
const REPORT_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;

/// Credentials + transport config shared between source and writer. Kept
/// as a small Clone-able bundle so building both halves from one config
/// is cheap.
#[derive(Debug, Clone)]
struct Creds {
    base_url: String,
    user_agent: String,
    user: String,
    password: String,
}

impl Creds {
    fn new(
        base_url: String,
        user_agent: String,
        user: String,
        password: String,
    ) -> Result<Self, Error> {
        if user_agent.trim().is_empty() {
            return Err(Error::config(
                "iCloud CalDAV requires a non-empty User-Agent",
            ));
        }
        Ok(Self {
            base_url,
            user_agent,
            user,
            password,
        })
    }

    fn build_client(&self) -> Result<Client, Error> {
        Client::builder()
            .user_agent(&self.user_agent)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(Error::from)
    }
}

// ─── Source ─────────────────────────────────────────────────────────────

/// Read-side iCloud CalDAV source. Holds a discovery cache so the
/// expensive 3-step well-known → principal → home-set walk runs at most
/// once per source instance under steady-state.
pub struct IcloudCalendarSource {
    creds: Creds,
    /// Cached discovery result. `None` = not yet discovered (or
    /// invalidated by a 4xx). A `tokio::sync::Mutex` is sufficient here:
    /// discovery is rare relative to event reads, and we want exclusive
    /// access during the discovery walk anyway.
    discovered: Mutex<Option<Vec<DiscoveredCalendar>>>,
    client: Client,
}

impl std::fmt::Debug for IcloudCalendarSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcloudCalendarSource")
            .field("user", &self.creds.user)
            .field("user_agent", &self.creds.user_agent)
            .finish_non_exhaustive()
    }
}

impl IcloudCalendarSource {
    pub fn new(
        base_url: String,
        user_agent: String,
        user: String,
        password: String,
    ) -> Result<Self, Error> {
        let creds = Creds::new(base_url, user_agent, user, password)?;
        let client = creds.build_client()?;
        Ok(Self {
            creds,
            discovered: Mutex::new(None),
            client,
        })
    }

    /// Return cached discovery, or run it if the cache is empty.
    async fn discovered(&self) -> Result<Vec<DiscoveredCalendar>, Error> {
        let mut guard = self.discovered.lock().await;
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
        let fresh = discovery::discover(
            &self.creds.base_url,
            &self.creds.user_agent,
            &self.creds.user,
            &self.creds.password,
        )
        .await?;
        *guard = Some(fresh.clone());
        Ok(fresh)
    }

    /// Drop the cached calendar list — next `list_calendars` /
    /// `list_events` call re-discovers. Used when a known calendar URL
    /// returns 4xx (URL drift, calendar deleted, shard migration, etc.).
    /// 5xx errors do NOT invalidate; they're treated as transient.
    async fn invalidate_cache(&self) {
        *self.discovered.lock().await = None;
    }

    /// Issue a REPORT against `calendar_url` and return the response body.
    /// On 4xx, invalidates the discovery cache so the next call
    /// re-discovers. On 5xx, leaves the cache intact (server hiccup).
    async fn report(&self, calendar_url: &str) -> Result<String, Error> {
        let method = Method::from_bytes(b"REPORT")
            .map_err(|e| Error::config(format!("invalid HTTP method: {e}")))?;
        let resp = self
            .client
            .request(method, calendar_url)
            .basic_auth(&self.creds.user, Some(&self.creds.password))
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(REPORT_BODY)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            // 4xx on a cached URL → calendar list is stale. Drop it so
            // the next call re-discovers. 5xx is treated as transient.
            if status.is_client_error() {
                self.invalidate_cache().await;
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                message: format!(
                    "REPORT {calendar_url}: {}",
                    body.chars().take(500).collect::<String>()
                ),
            });
        }
        resp.text().await.map_err(Error::from)
    }
}

#[async_trait]
impl CalendarSource for IcloudCalendarSource {
    fn tag(&self) -> &'static str {
        "icloud-caldav"
    }

    async fn list_calendars(&self) -> Result<Vec<Calendar>, Error> {
        let discovered = self.discovered().await?;
        Ok(discovered
            .into_iter()
            .map(|d| Calendar {
                id: d.url,
                name: d.displayname,
                description: String::new(),
                color: d.color.unwrap_or_default(),
                timezone: String::new(),
                order: None,
                created_at: None,
                updated_at: None,
            })
            .collect())
    }

    async fn list_events(&self, calendar_id: Option<&str>) -> Result<Vec<CalendarEvent>, Error> {
        let discovered = self.discovered().await?;
        let urls: Vec<String> = match calendar_id {
            Some(id) => discovered
                .iter()
                .filter(|c| c.url == id)
                .map(|c| c.url.clone())
                .collect(),
            None => discovered.iter().map(|c| c.url.clone()).collect(),
        };

        let mut events = Vec::new();
        for url in urls {
            let body = self.report(&url).await?;
            events.extend(parse_report(body.as_bytes(), &url)?);
        }
        Ok(events)
    }
}

/// Parse a CalDAV REPORT multistatus response into `CalendarEvent`s.
/// Reuses `DavMultistatus::parse` (handles namespace prefixes, CDATA,
/// CR entities) and adapts the result to the `forwardemail::CalendarEvent`
/// shape used by the rest of pimsteward.
///
/// `calendar_url` is the URL the REPORT was issued against — it becomes
/// the event's `calendar_id`.
fn parse_report(xml: &[u8], calendar_url: &str) -> Result<Vec<CalendarEvent>, Error> {
    let ms = DavMultistatus::parse(xml)?;
    let mut out = Vec::with_capacity(ms.responses.len());
    for r in ms.responses {
        let Some(ical) = r.calendar_data else {
            // The collection itself shows up as a response with no
            // calendar-data — skip those.
            continue;
        };
        // Event id = href tail (filename). iCloud uses `<UID>.ics`.
        // If the href has no tail (collection root), fall back to the
        // full href so the id is at least non-empty.
        let id = href_tail(&r.href).unwrap_or_else(|| r.href.clone());
        let uid = extract_ical_uid(&ical);
        let summary = extract_ical_field(&ical, "SUMMARY");
        let location = extract_ical_field(&ical, "LOCATION");
        let status = extract_ical_field(&ical, "STATUS");
        out.push(CalendarEvent {
            id,
            uid,
            calendar_id: Some(calendar_url.to_string()),
            ical: Some(ical),
            etag: r.etag,
            summary,
            description: None,
            location,
            start_date: None,
            end_date: None,
            status,
            created_at: None,
            updated_at: None,
        });
    }
    Ok(out)
}

/// Extract the last path segment of an href, ignoring trailing slashes.
/// Returns `None` if there's nothing useful to extract (empty href, or
/// just `/`).
fn href_tail(href: &str) -> Option<String> {
    let trimmed = href.trim_end_matches('/');
    let tail = trimmed.rsplit('/').next()?;
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

/// Extract the first `UID:` line from inside a VEVENT block.
fn extract_ical_uid(ics: &str) -> Option<String> {
    extract_ical_field(ics, "UID")
}

/// Extract a top-level iCalendar property value from `ical`. Handles
/// RFC 5545 §3.1 line folding (CRLF or LF followed by space/tab) and
/// parametered properties (e.g. `SUMMARY;LANGUAGE=en:`). Property name
/// match is case-insensitive.
fn extract_ical_field(ical: &str, name: &str) -> Option<String> {
    // Step 1: unfold (RFC 5545 §3.1). Replace any CRLF + (space|tab) or
    // bare LF + (space|tab) with empty — folding is purely cosmetic and
    // the rest of the parser wants a single logical line per property.
    let unfolded: String = {
        let mut out = String::with_capacity(ical.len());
        let mut chars = ical.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next(); // consume \n
                if matches!(chars.peek(), Some(' ') | Some('\t')) {
                    chars.next(); // consume continuation space/tab
                    continue;
                }
                out.push('\r');
                out.push('\n');
            } else if c == '\n' {
                if matches!(chars.peek(), Some(' ') | Some('\t')) {
                    chars.next();
                    continue;
                }
                out.push('\n');
            } else {
                out.push(c);
            }
        }
        out
    };

    // Step 2: walk lines. Match property name with optional parameters:
    // either "NAME:" or "NAME;...:".
    let upper_name = name.to_ascii_uppercase();
    for line in unfolded.lines() {
        // Find the first ':' that ends the property name+params.
        let Some(colon) = line.find(':') else { continue };
        let head = &line[..colon];
        // Property name is `head` up to the first ';' (params delimiter).
        let prop_name = head.split(';').next().unwrap_or(head);
        if prop_name.eq_ignore_ascii_case(&upper_name) {
            // Strip any trailing CR (in case the input had CRLF without
            // splitting cleanly on `lines()`).
            return Some(line[colon + 1..].trim_end_matches('\r').to_string());
        }
    }
    None
}

// ─── Writer ─────────────────────────────────────────────────────────────

/// Write-side iCloud CalDAV client. Exposed through the
/// `CalendarWriter` trait (see impl below) so the MCP layer dispatches
/// mutations identically across forwardemail and iCloud.
pub struct IcloudCalendarWriter {
    creds: Creds,
    client: Client,
}

impl std::fmt::Debug for IcloudCalendarWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcloudCalendarWriter")
            .field("user", &self.creds.user)
            .field("user_agent", &self.creds.user_agent)
            .finish_non_exhaustive()
    }
}

impl IcloudCalendarWriter {
    pub fn new(
        base_url: String,
        user_agent: String,
        user: String,
        password: String,
    ) -> Result<Self, Error> {
        let creds = Creds::new(base_url, user_agent, user, password)?;
        let client = creds.build_client()?;
        Ok(Self { creds, client })
    }

    /// PUT `<calendar_url>/<uid>.ics` with `If-None-Match: *` to create a
    /// new event. Returns the server-issued ETag from the response — or
    /// an empty string if the server didn't include one (some CalDAV
    /// servers omit ETag on initial PUT and require a follow-up GET; in
    /// that case the caller should re-read and pick up the etag from the
    /// REPORT response).
    pub async fn create_event(
        &self,
        calendar_url: &str,
        uid: &str,
        ical: &str,
    ) -> Result<String, Error> {
        let url = event_url(calendar_url, uid);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.creds.user, Some(&self.creds.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("If-None-Match", "*")
            .body(ical.to_string())
            .send()
            .await?;
        Self::handle_put_response(&url, resp).await
    }

    /// PUT `<calendar_url>/<uid>.ics` with `If-Match: <if_match>` to
    /// update an existing event. Returns the new ETag. On 412 returns
    /// `Error::PreconditionFailed` carrying the server's current etag (if
    /// the server provided one) so the caller can re-read and retry.
    pub async fn update_event(
        &self,
        calendar_url: &str,
        uid: &str,
        ical: &str,
        if_match: &str,
    ) -> Result<String, Error> {
        let url = event_url(calendar_url, uid);
        let resp = self
            .client
            .put(&url)
            .basic_auth(&self.creds.user, Some(&self.creds.password))
            .header("Content-Type", "text/calendar; charset=utf-8")
            .header("If-Match", if_match)
            .body(ical.to_string())
            .send()
            .await?;
        Self::handle_put_response(&url, resp).await
    }

    /// DELETE `<calendar_url>/<uid>.ics` with `If-Match: <if_match>`. On
    /// 412 returns `Error::PreconditionFailed`. 404 is treated as success
    /// — the resource is already gone, which is what the caller wanted.
    pub async fn delete_event(
        &self,
        calendar_url: &str,
        uid: &str,
        if_match: &str,
    ) -> Result<(), Error> {
        let url = event_url(calendar_url, uid);
        let resp = self
            .client
            .delete(&url)
            .basic_auth(&self.creds.user, Some(&self.creds.password))
            .header("If-Match", if_match)
            .send()
            .await?;
        let status = resp.status();
        if status == StatusCode::PRECONDITION_FAILED {
            return Err(Error::precondition_failed(extract_etag(&resp)));
        }
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Error::Api {
            status: status.as_u16(),
            message: format!(
                "DELETE {url}: {}",
                body.chars().take(500).collect::<String>()
            ),
        })
    }

    /// Shared PUT response handling: 200 and 201 both count as success
    /// (iCloud sometimes returns 200 OK where 201 Created would be
    /// strictly correct). 412 becomes the structured PreconditionFailed
    /// error. Everything else is an Api error with the response body.
    async fn handle_put_response(
        url: &str,
        resp: reqwest::Response,
    ) -> Result<String, Error> {
        let status = resp.status();
        if status == StatusCode::PRECONDITION_FAILED {
            return Err(Error::precondition_failed(extract_etag(&resp)));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                status: status.as_u16(),
                message: format!(
                    "PUT {url}: {}",
                    body.chars().take(500).collect::<String>()
                ),
            });
        }
        // 200 OR 201 — treat both as success.
        Ok(extract_etag(&resp).unwrap_or_default())
    }
}

// `IcloudCalendarWriter` exposes itself through the unified
// `CalendarWriter` trait so the MCP layer can dispatch calendar mutations
// without knowing which provider is active. The mapping is direct: the
// trait's `calendar_id` parameter is the calendar's collection URL, and
// `uid` is the iCalendar UID (also the `.ics` filename tail).
//
// CalDAV's PUT response body is empty — there is no server-normalised
// event to round-trip back. Synthesize a `CalendarEvent` from the caller's
// iCal text (which IS the canonical text for iCloud) plus the response
// ETag so the MCP layer's serialisation still surfaces the derived fields
// (`summary`, `start_date`, `end_date`, `location`, `status`) that the LLM
// expects.
#[async_trait]
impl crate::source::traits::CalendarWriter for IcloudCalendarWriter {
    fn tag(&self) -> &'static str {
        "icloud-caldav-writer"
    }

    async fn create_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
    ) -> Result<CalendarEvent, Error> {
        let etag = IcloudCalendarWriter::create_event(self, calendar_id, uid, ical).await?;
        Ok(synthesize_event(calendar_id, uid, ical, etag))
    }

    async fn update_event(
        &self,
        calendar_id: &str,
        uid: &str,
        ical: &str,
        if_match: &str,
    ) -> Result<CalendarEvent, Error> {
        let etag =
            IcloudCalendarWriter::update_event(self, calendar_id, uid, ical, if_match).await?;
        Ok(synthesize_event(calendar_id, uid, ical, etag))
    }

    async fn delete_event(
        &self,
        calendar_id: &str,
        uid: &str,
        if_match: &str,
    ) -> Result<(), Error> {
        IcloudCalendarWriter::delete_event(self, calendar_id, uid, if_match).await
    }
}

/// Build a `CalendarEvent` from a successful CalDAV PUT — caller's iCal +
/// response etag. Parses the same derived fields (summary, location,
/// status, start/end) the source-side `parse_report` extracts, so a freshly
/// created event looks the same to MCP callers as one re-read via
/// `list_events`.
fn synthesize_event(
    calendar_url: &str,
    uid: &str,
    ical: &str,
    etag: String,
) -> CalendarEvent {
    // `id` mirrors the source-side convention: the .ics filename tail.
    // For iCloud writes the URL is `<calendar_url>/<uid>.ics`, so the tail
    // is exactly `<uid>.ics` — no need to actually parse it.
    let id = format!("{uid}.ics");
    CalendarEvent {
        id,
        uid: extract_ical_uid(ical).or_else(|| Some(uid.to_string())),
        calendar_id: Some(calendar_url.to_string()),
        ical: Some(ical.to_string()),
        etag: if etag.is_empty() { None } else { Some(etag) },
        summary: extract_ical_field(ical, "SUMMARY"),
        description: extract_ical_field(ical, "DESCRIPTION"),
        location: extract_ical_field(ical, "LOCATION"),
        start_date: extract_ical_field(ical, "DTSTART"),
        end_date: extract_ical_field(ical, "DTEND"),
        status: extract_ical_field(ical, "STATUS"),
        // CalDAV does not expose server-side timestamps for events.
        created_at: None,
        updated_at: None,
    }
}

/// Pull the `ETag` header off a response, if present and decodable as
/// UTF-8. CalDAV servers may include or omit ETag on PUT/DELETE — the
/// caller decides whether the empty case is recoverable.
fn extract_etag(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Build the per-event URL: `<calendar_url>/<uid>.ics`. Tolerates a
/// trailing slash on `calendar_url`.
fn event_url(calendar_url: &str, uid: &str) -> String {
    format!("{}/{uid}.ics", calendar_url.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn href_tail_extracts_filename() {
        assert_eq!(
            href_tail("/123/calendars/home/abc-123.ics").as_deref(),
            Some("abc-123.ics")
        );
        // Trailing slashes are stripped before splitting, so a directory
        // href yields its last segment.
        assert_eq!(
            href_tail("/123/calendars/home/").as_deref(),
            Some("home")
        );
        assert_eq!(href_tail("/").as_deref(), None);
        assert_eq!(href_tail("").as_deref(), None);
    }

    #[test]
    fn event_url_handles_trailing_slash() {
        assert_eq!(
            event_url("https://x.example/cal/", "evt-1"),
            "https://x.example/cal/evt-1.ics"
        );
        assert_eq!(
            event_url("https://x.example/cal", "evt-1"),
            "https://x.example/cal/evt-1.ics"
        );
    }

    #[test]
    fn extract_ical_uid_from_vevent() {
        let ics = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc-123\nSUMMARY:Hi\nEND:VEVENT\nEND:VCALENDAR";
        assert_eq!(extract_ical_uid(ics).as_deref(), Some("abc-123"));
    }

    #[test]
    fn extract_ical_field_handles_folded_summary() {
        let ical = "BEGIN:VEVENT\r\nSUMMARY:This is a long\r\n  summary that wraps\r\nEND:VEVENT\r\n";
        assert_eq!(
            extract_ical_field(ical, "SUMMARY"),
            Some("This is a long summary that wraps".to_string())
        );
    }

    #[test]
    fn extract_ical_field_handles_parametered_property() {
        let ical = "BEGIN:VEVENT\r\nSUMMARY;LANGUAGE=en:Hi there\r\nEND:VEVENT\r\n";
        assert_eq!(
            extract_ical_field(ical, "SUMMARY"),
            Some("Hi there".to_string())
        );
    }

    #[test]
    fn extract_ical_field_handles_multiple_params() {
        let ical = "BEGIN:VEVENT\r\nDTSTART;TZID=America/Toronto;VALUE=DATE-TIME:20260501T143000\r\nEND:VEVENT\r\n";
        assert_eq!(
            extract_ical_field(ical, "DTSTART"),
            Some("20260501T143000".to_string())
        );
    }

    #[test]
    fn extract_ical_field_returns_none_when_absent() {
        let ical = "BEGIN:VEVENT\r\nEND:VEVENT\r\n";
        assert_eq!(extract_ical_field(ical, "SUMMARY"), None);
    }

    #[test]
    fn parse_report_extracts_event() {
        let xml = br#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/123/calendars/home/event-uid.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"abc"</D:getetag>
        <CAL:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:event-uid
SUMMARY:Hello
END:VEVENT
END:VCALENDAR</CAL:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let events = parse_report(xml, "https://example/cal/").unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.id, "event-uid.ics");
        assert_eq!(e.uid.as_deref(), Some("event-uid"));
        assert_eq!(e.calendar_id.as_deref(), Some("https://example/cal/"));
        assert_eq!(e.etag.as_deref(), Some("\"abc\""));
        assert_eq!(e.summary.as_deref(), Some("Hello"));
    }
}
