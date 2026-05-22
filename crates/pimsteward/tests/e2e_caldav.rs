//! e2e tests for the CalDAV read path against the real forwardemail
//! CalDAV server (`caldav.forwardemail.net`).
//!
//! Read-only: verifies list_calendars and list_events return valid data
//! with ETags and iCalendar bodies.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::source::{CalendarSource, DavCalendarSource};

fn caldav_source(ctx: &E2eContext) -> DavCalendarSource {
    let pass = std::fs::read_to_string(
        std::env::var("PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE")
            .unwrap_or_else(|_| "/home/dan/.config/secrets/pimsteward-test-alias-password".into()),
    )
    .expect("reading password file")
    .trim()
    .to_string();

    DavCalendarSource::new("https://caldav.forwardemail.net", ctx.alias.clone(), pass)
        .expect("build CalDAV source")
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn caldav_list_calendars_returns_at_least_one() {
    let ctx = E2eContext::from_env();
    let source = caldav_source(&ctx);

    let calendars = source.list_calendars().await.expect("list_calendars");
    assert!(
        !calendars.is_empty(),
        "CalDAV should return at least one calendar"
    );
    for cal in &calendars {
        assert!(!cal.id.is_empty(), "calendar id should not be empty");
    }
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn caldav_list_events_returns_ical_with_etags() {
    let ctx = E2eContext::from_env();
    let source = caldav_source(&ctx);

    // Create a test event via REST first so we have something to read.
    let uid = format!("e2e-caldav-{}", std::process::id());
    let ical = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nSUMMARY:CalDAV e2e test\r\n\
         DTSTART:20260401T120000Z\r\nDTEND:20260401T130000Z\r\n\
         END:VEVENT\r\nEND:VCALENDAR"
    );

    let calendars = ctx.client.list_calendars().await.expect("list cals");
    let cal_id = &calendars[0].id;
    let created = ctx
        .client
        .create_calendar_event(cal_id, &ical, Some(&uid))
        .await
        .expect("create test event");

    // Now read via CalDAV
    let events = source.list_events(None).await.expect("list_events");
    let found = events.iter().find(|e| e.uid.as_deref() == Some(&uid));
    assert!(found.is_some(), "CalDAV should return the test event");

    let event = found.unwrap();
    assert!(
        event.ical.is_some(),
        "CalDAV event should have iCal content"
    );
    assert!(
        event.etag.is_some(),
        "CalDAV event should have an etag (getetag)"
    );
    assert!(
        event
            .ical
            .as_deref()
            .unwrap()
            .contains("CalDAV e2e test"),
        "iCal should contain the summary"
    );

    // Cleanup
    ctx.client
        .delete_calendar_event(&created.id)
        .await
        .expect("cleanup");
}
