//! Tests for the iCloud CalDAV RFC 6764 discovery walk.
//!
//! Two layers:
//!
//! - **Parser unit tests** drive the streaming XML parsers directly with
//!   canned iCloud-shaped responses. These are the cheap, fast checks
//!   that catch namespace-prefix and component-filter regressions.
//! - **One end-to-end wiremock test** stands up an HTTP server that
//!   responds to all three PROPFIND requests in sequence, exercises
//!   `discover()` against it, and checks the full pipeline including
//!   relative-href resolution and User-Agent propagation.

use pimsteward::error::Error;
use pimsteward::icloud::discovery::{
    parse_calendar_home_set_href, parse_calendar_list, parse_principal_href,
};
use pimsteward::icloud::{
    discover, DiscoveredCalendar, IcloudCalendarSource, IcloudCalendarWriter,
};
use pimsteward::source::traits::CalendarSource;
use reqwest::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// --- canned iCloud-shaped XML responses --------------------------------

const PRINCIPAL_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat>
      <prop>
        <current-user-principal>
          <href>/123456789/principal/</href>
        </current-user-principal>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

const CALENDAR_HOME_SET_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/123456789/principal/</href>
    <propstat>
      <prop>
        <cal:calendar-home-set>
          <href>https://p07-caldav.icloud.com/123456789/calendars/</href>
        </cal:calendar-home-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

const CALENDAR_LIST_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav"
             xmlns:cs="http://calendarserver.org/ns/"
             xmlns:ic="http://apple.com/ns/ical/">
  <response>
    <href>/123456789/calendars/home/</href>
    <propstat>
      <prop>
        <resourcetype>
          <collection/>
          <cal:calendar/>
        </resourcetype>
        <displayname>Home</displayname>
        <cs:getctag>HwoQEgwAAAhYAA==</cs:getctag>
        <ic:calendar-color>#FF2968FF</ic:calendar-color>
        <cal:supported-calendar-component-set>
          <cal:comp name="VEVENT"/>
        </cal:supported-calendar-component-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
  <response>
    <href>/123456789/calendars/reminders/</href>
    <propstat>
      <prop>
        <resourcetype>
          <collection/>
          <cal:calendar/>
        </resourcetype>
        <displayname>Reminders</displayname>
        <cs:getctag>HwoQEgwAAAhYBB==</cs:getctag>
        <cal:supported-calendar-component-set>
          <cal:comp name="VTODO"/>
        </cal:supported-calendar-component-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
  <response>
    <href>/123456789/calendars/pimsteward_test/</href>
    <propstat>
      <prop>
        <resourcetype>
          <collection/>
          <cal:calendar/>
        </resourcetype>
        <displayname>pimsteward_test</displayname>
        <cs:getctag>HwoQEgwAAAhYCC==</cs:getctag>
        <cal:supported-calendar-component-set>
          <cal:comp name="VEVENT"/>
        </cal:supported-calendar-component-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

// --- parser unit tests -------------------------------------------------

#[test]
fn discovery_parse_principal_extracts_href() {
    let got = parse_principal_href(PRINCIPAL_RESPONSE.as_bytes()).unwrap();
    assert_eq!(got.as_deref(), Some("/123456789/principal/"));
}

#[test]
fn discovery_parse_calendar_home_set_extracts_href() {
    let got = parse_calendar_home_set_href(CALENDAR_HOME_SET_RESPONSE.as_bytes()).unwrap();
    assert_eq!(
        got.as_deref(),
        Some("https://p07-caldav.icloud.com/123456789/calendars/")
    );
}

#[test]
fn discovery_parse_calendar_list_returns_vevent_calendars_only() {
    // Resolves relative hrefs against this URL.
    let req = Url::parse("https://p07-caldav.icloud.com/123456789/calendars/").unwrap();
    let cals = parse_calendar_list(CALENDAR_LIST_RESPONSE.as_bytes(), &req).unwrap();

    let names: Vec<&str> = cals.iter().map(|c| c.displayname.as_str()).collect();
    assert_eq!(names, vec!["Home", "pimsteward_test"]);
    assert!(
        !names.contains(&"Reminders"),
        "VTODO-only Reminders calendar must be filtered out"
    );
}

#[test]
fn discovery_parse_calendar_list_extracts_metadata() {
    let req = Url::parse("https://p07-caldav.icloud.com/123456789/calendars/").unwrap();
    let cals = parse_calendar_list(CALENDAR_LIST_RESPONSE.as_bytes(), &req).unwrap();

    let home = cals.iter().find(|c| c.displayname == "Home").unwrap();
    assert_eq!(home.ctag.as_deref(), Some("HwoQEgwAAAhYAA=="));
    assert_eq!(home.color.as_deref(), Some("#FF2968FF"));
    assert_eq!(home.supported_components, vec!["VEVENT".to_string()]);
    // Relative href resolved against the request URL.
    assert_eq!(
        home.url,
        "https://p07-caldav.icloud.com/123456789/calendars/home/"
    );
}

#[test]
fn discovery_parse_calendar_list_handles_absolute_href() {
    // iCloud sometimes returns absolute hrefs. Verify they survive
    // resolution unchanged.
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>https://p99-caldav.icloud.com/elsewhere/work/</href>
    <propstat>
      <prop>
        <resourcetype><collection/><cal:calendar/></resourcetype>
        <displayname>Work</displayname>
        <cal:supported-calendar-component-set>
          <cal:comp name="VEVENT"/>
        </cal:supported-calendar-component-set>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
    let req = Url::parse("https://p07-caldav.icloud.com/123456789/calendars/").unwrap();
    let cals = parse_calendar_list(xml, &req).unwrap();
    assert_eq!(cals.len(), 1);
    assert_eq!(cals[0].url, "https://p99-caldav.icloud.com/elsewhere/work/");
}

// --- end-to-end wiremock test -----------------------------------------

/// Build a wiremock response matcher that triggers only on PROPFIND
/// requests with the right Depth header. Wiremock's `method()` accepts
/// arbitrary tokens (it parses via `http::Method::from_str`), so
/// `PROPFIND` works fine.
#[tokio::test]
async fn discovery_full_walk_against_mock_server() {
    let server = MockServer::start().await;

    // Step 1: PROPFIND /.well-known/caldav -> principal href.
    Mock::given(method("PROPFIND"))
        .and(path("/.well-known/caldav"))
        .and(header("Depth", "0"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(PRINCIPAL_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Step 2: PROPFIND on the principal URL -> calendar-home-set href.
    // We rewrite the home-set XML at request time so the absolute href
    // points back at our wiremock server rather than the canned
    // p07-caldav.icloud.com URL — that way Step 3 hits us, not iCloud.
    let mock_uri = server.uri();
    let home_set_body = CALENDAR_HOME_SET_RESPONSE.replace(
        "https://p07-caldav.icloud.com/123456789/calendars/",
        &format!("{mock_uri}/123456789/calendars/"),
    );
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/principal/"))
        .and(header("Depth", "0"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(home_set_body),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Step 3: PROPFIND Depth: 1 on the calendar-home-set URL -> list.
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/calendars/"))
        .and(header("Depth", "1"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        // Sanity check the request body asks for the props we expect.
        .and(BodyContains("supported-calendar-component-set"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(CALENDAR_LIST_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    let cals = discover(
        &server.uri(),
        "pimsteward-test/1.0",
        "alice@example.com",
        "app-specific-pw",
    )
    .await
    .unwrap();

    let names: Vec<&str> = cals.iter().map(|c| c.displayname.as_str()).collect();
    assert_eq!(names, vec!["Home", "pimsteward_test"]);

    // Relative hrefs resolved against the Step-3 request URL — so they
    // point back at the mock server.
    let home: &DiscoveredCalendar = cals.iter().find(|c| c.displayname == "Home").unwrap();
    assert!(
        home.url.starts_with(&mock_uri),
        "expected URL to resolve against mock server, got {}",
        home.url
    );
    assert!(home.url.ends_with("/123456789/calendars/home/"));
}

#[tokio::test]
async fn discovery_rejects_empty_user_agent() {
    let err = discover("https://caldav.icloud.com/", "", "user", "pw")
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("User-Agent"), "got: {msg}");
}

// --- helpers ----------------------------------------------------------

/// Wiremock matcher that asserts the request body bytes contain a given
/// substring. Avoids the ceremony of pulling in a regex matcher just for
/// "is the right XML body".
struct BodyContains(&'static str);

impl wiremock::Match for BodyContains {
    fn matches(&self, request: &Request) -> bool {
        std::str::from_utf8(&request.body)
            .map(|s| s.contains(self.0))
            .unwrap_or(false)
    }
}

// --- IcloudCalendarSource / IcloudCalendarWriter tests ----------------
//
// These exercise the read/write paths via wiremock. The Source tests
// stand up the full discovery walk + REPORT against the mock server. The
// Writer tests skip discovery entirely — `IcloudCalendarWriter::*_event`
// take a fully-qualified `calendar_url` and don't consult the discovery
// cache, so a mock PUT/DELETE is enough.

const REPORT_RESPONSE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/123456789/calendars/home/event-uid.ics</href>
    <propstat>
      <prop>
        <getetag>"etag-1"</getetag>
        <cal:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:event-uid
SUMMARY:Test event
END:VEVENT
END:VCALENDAR</cal:calendar-data>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

/// Stand up a wiremock that serves the full discovery walk and a single
/// REPORT response for the "Home" calendar — the rest of the canned
/// list (Reminders / pimsteward_test) is intentionally also returned by
/// discovery, but only "Home" gets a REPORT mount here.
async fn mock_with_full_discovery() -> (MockServer, String) {
    let server = MockServer::start().await;

    // PROPFIND well-known.
    Mock::given(method("PROPFIND"))
        .and(path("/.well-known/caldav"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(PRINCIPAL_RESPONSE),
        )
        .mount(&server)
        .await;

    // PROPFIND principal — rewrite the home-set href to point at us.
    let mock_uri = server.uri();
    let home_set_body = CALENDAR_HOME_SET_RESPONSE.replace(
        "https://p07-caldav.icloud.com/123456789/calendars/",
        &format!("{mock_uri}/123456789/calendars/"),
    );
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/principal/"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(home_set_body),
        )
        .mount(&server)
        .await;

    // PROPFIND home-set — calendar list.
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/calendars/"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(CALENDAR_LIST_RESPONSE),
        )
        .mount(&server)
        .await;

    let home_url = format!("{mock_uri}/123456789/calendars/home/");
    (server, home_url)
}

#[tokio::test]
async fn caldav_list_events_round_trips() {
    let (server, home_url) = mock_with_full_discovery().await;

    // Mount a REPORT against the Home calendar URL.
    Mock::given(method("REPORT"))
        .and(path("/123456789/calendars/home/"))
        .and(header("Depth", "1"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .and(BodyContains("VEVENT"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(REPORT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Also mount the pimsteward_test calendar so list_events(None) works
    // — it tries every discovered calendar.
    Mock::given(method("REPORT"))
        .and(path("/123456789/calendars/pimsteward_test/"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(
                    r#"<?xml version="1.0"?><multistatus xmlns="DAV:"></multistatus>"#,
                ),
        )
        .mount(&server)
        .await;

    let src = IcloudCalendarSource::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "app-pw".into(),
    )
    .unwrap();

    // Filter to just Home so this assertion isn't sensitive to the order
    // pimsteward_test responds in.
    let events = src.list_events(Some(&home_url)).await.unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.id, "event-uid.ics");
    assert_eq!(e.uid.as_deref(), Some("event-uid"));
    assert_eq!(e.calendar_id.as_deref(), Some(home_url.as_str()));
    assert_eq!(e.etag.as_deref(), Some("\"etag-1\""));
    assert!(e.ical.as_deref().unwrap().contains("SUMMARY:Test event"));
    assert_eq!(e.summary.as_deref(), Some("Test event"));
}

#[tokio::test]
async fn caldav_create_event_returns_etag() {
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/cal/evt-1.ics"))
        .and(header("If-None-Match", "*"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .and(header("Content-Type", "text/calendar; charset=utf-8"))
        .respond_with(
            ResponseTemplate::new(201).insert_header("ETag", "\"new-etag\""),
        )
        .expect(1)
        .mount(&server)
        .await;

    let writer = IcloudCalendarWriter::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();

    let cal_url = format!("{}/cal", server.uri());
    let etag = writer
        .create_event(&cal_url, "evt-1", "BEGIN:VCALENDAR\nEND:VCALENDAR")
        .await
        .unwrap();
    assert_eq!(etag, "\"new-etag\"");
}

#[tokio::test]
async fn caldav_create_event_handles_200_response() {
    // iCloud sometimes returns 200 OK on PUT where 201 Created would be
    // strictly correct. Both must be treated as success.
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/cal/evt-2.ics"))
        .and(header("If-None-Match", "*"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("ETag", "\"two-hundred\""),
        )
        .expect(1)
        .mount(&server)
        .await;

    let writer = IcloudCalendarWriter::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();

    let cal_url = format!("{}/cal", server.uri());
    let etag = writer
        .create_event(&cal_url, "evt-2", "BEGIN:VCALENDAR\nEND:VCALENDAR")
        .await
        .unwrap();
    assert_eq!(etag, "\"two-hundred\"");
}

#[tokio::test]
async fn caldav_update_event_with_etag_conflict_errors() {
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path("/cal/evt-3.ics"))
        .and(header("If-Match", "\"stale-etag\""))
        .respond_with(
            ResponseTemplate::new(412).insert_header("ETag", "\"current-etag\""),
        )
        .expect(1)
        .mount(&server)
        .await;

    let writer = IcloudCalendarWriter::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();

    let cal_url = format!("{}/cal", server.uri());
    let err = writer
        .update_event(
            &cal_url,
            "evt-3",
            "BEGIN:VCALENDAR\nEND:VCALENDAR",
            "\"stale-etag\"",
        )
        .await
        .unwrap_err();
    match err {
        Error::PreconditionFailed { etag } => {
            assert_eq!(etag.as_deref(), Some("\"current-etag\""));
        }
        other => panic!("expected PreconditionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn caldav_delete_event_with_etag_conflict_errors() {
    // DELETE shares the 412 path with PUT — verify it surfaces the
    // structured error too.
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/cal/evt-4.ics"))
        .and(header("If-Match", "\"old\""))
        .respond_with(
            ResponseTemplate::new(412).insert_header("ETag", "\"newer\""),
        )
        .expect(1)
        .mount(&server)
        .await;

    let writer = IcloudCalendarWriter::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();

    let cal_url = format!("{}/cal", server.uri());
    let err = writer
        .delete_event(&cal_url, "evt-4", "\"old\"")
        .await
        .unwrap_err();
    match err {
        Error::PreconditionFailed { etag } => {
            assert_eq!(etag.as_deref(), Some("\"newer\""));
        }
        other => panic!("expected PreconditionFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn caldav_4xx_on_report_invalidates_discovery_cache() {
    // First REPORT against the discovered Home calendar URL returns 404
    // — the source MUST drop the cached calendar list so the next call
    // re-runs discovery. We verify this by having the second
    // `list_events` call trigger a *second* set of PROPFINDs (4 in total
    // across the two discovery walks).
    let server = MockServer::start().await;

    // Discovery PROPFINDs — set up to be hit twice.
    Mock::given(method("PROPFIND"))
        .and(path("/.well-known/caldav"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(PRINCIPAL_RESPONSE),
        )
        .expect(2)
        .mount(&server)
        .await;
    let mock_uri = server.uri();
    let home_set_body = CALENDAR_HOME_SET_RESPONSE.replace(
        "https://p07-caldav.icloud.com/123456789/calendars/",
        &format!("{mock_uri}/123456789/calendars/"),
    );
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/principal/"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(home_set_body),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/123456789/calendars/"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(CALENDAR_LIST_RESPONSE),
        )
        .expect(2)
        .mount(&server)
        .await;

    // First REPORT: 404 — invalidate the cache.
    Mock::given(method("REPORT"))
        .and(path("/123456789/calendars/home/"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Second REPORT (after re-discovery): success.
    Mock::given(method("REPORT"))
        .and(path("/123456789/calendars/home/"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(REPORT_RESPONSE),
        )
        .mount(&server)
        .await;

    let src = IcloudCalendarSource::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();
    let home_url = format!("{}/123456789/calendars/home/", server.uri());

    // First call: 404 on REPORT bubbles up. Cache gets invalidated.
    let err = src.list_events(Some(&home_url)).await.unwrap_err();
    assert!(format!("{err}").contains("404"), "got: {err}");

    // Second call: triggers a fresh discovery walk + a successful REPORT.
    let events = src.list_events(Some(&home_url)).await.unwrap();
    assert_eq!(events.len(), 1);
}

#[tokio::test]
async fn caldav_user_agent_set_on_every_request() {
    // Cover REPORT (read), PUT (create), and DELETE (delete) all in one
    // test — each `Mock::expect(1)` matcher requires the User-Agent
    // header to match, so if any request omits it, that mock fails to
    // match and `.expect(1)` triggers a panic on drop.
    let (server, home_url) = mock_with_full_discovery().await;

    Mock::given(method("REPORT"))
        .and(path("/123456789/calendars/home/"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(
            ResponseTemplate::new(207)
                .insert_header("Content-Type", "application/xml; charset=utf-8")
                .set_body_string(REPORT_RESPONSE),
        )
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/cal/evt-ua.ics"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(ResponseTemplate::new(201).insert_header("ETag", "\"e\""))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/cal/evt-ua.ics"))
        .and(header("User-Agent", "pimsteward-test/1.0"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let src = IcloudCalendarSource::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();
    let writer = IcloudCalendarWriter::new(
        server.uri(),
        "pimsteward-test/1.0".into(),
        "alice@example.com".into(),
        "pw".into(),
    )
    .unwrap();

    let _ = src.list_events(Some(&home_url)).await.unwrap();

    let cal_url = format!("{}/cal", server.uri());
    writer
        .create_event(&cal_url, "evt-ua", "BEGIN:VCALENDAR\nEND:VCALENDAR")
        .await
        .unwrap();
    writer
        .delete_event(&cal_url, "evt-ua", "\"e\"")
        .await
        .unwrap();

    // expect(1) on each Mock fires its assertion when the server is
    // dropped — if any UA header was missing, those assertions fail.
    drop(server);
}
