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
//! Task 6 will wire `IcloudCalendarWriter` behind a unified
//! `Provider::build_calendar_writer()` trait method. For now, the writer
//! is a concrete type used directly by the iCloud provider.

use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::icloud::discovery::{self, DiscoveredCalendar};
use crate::source::dav::DavMultistatus;
use crate::source::traits::CalendarSource;
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use std::sync::Arc;
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
    discovered: Arc<Mutex<Option<Vec<DiscoveredCalendar>>>>,
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
            discovered: Arc::new(Mutex::new(None)),
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

/// Pull the first occurrence of a named property out of the first VEVENT
/// block in an iCalendar blob. Minimal line-by-line parser — does NOT
/// implement RFC 5545 unfolding or parameter handling, just `KEY:value`
/// lookups, which is enough for surface-level fields like SUMMARY/UID.
fn extract_ical_field(ics: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    let mut in_vevent = false;
    for line in ics.lines() {
        let l = line.trim();
        if l.eq_ignore_ascii_case("BEGIN:VEVENT") {
            in_vevent = true;
        } else if l.eq_ignore_ascii_case("END:VEVENT") {
            in_vevent = false;
        } else if in_vevent {
            let upper = l.to_ascii_uppercase();
            if let Some(rest) = upper.strip_prefix(&prefix) {
                // Use the original (non-uppercased) text for the value
                // so we don't mangle case.
                let start = l.len() - rest.len();
                return Some(l[start..].trim().to_string());
            }
        }
    }
    None
}

// ─── Writer ─────────────────────────────────────────────────────────────

/// Write-side iCloud CalDAV client. Concrete type for now; Task 6 will
/// add a unified `Provider::build_calendar_writer()` trait method.
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
