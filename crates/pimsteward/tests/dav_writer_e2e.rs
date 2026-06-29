//! Live e2e tests for the generic CalDAV/CardDAV writers
//! (`DavCalendarWriter` / `DavContactsWriter`).
//!
//! **Opt-in only.** These mutate a real CalDAV/CardDAV server (intended for
//! the Stalwart instance on saturn, but any standards-compliant server
//! works). They are gated three ways, mirroring `icloud_e2e.rs`:
//!
//! - `#[ignore]` so plain `cargo test` skips them.
//! - Env opt-in: `PIMSTEWARD_DAV_LIVE_URL` must be set (the server base URL,
//!   e.g. `https://sn.purpose.dev`). Plus credentials:
//!   `PIMSTEWARD_DAV_LIVE_USER`, `PIMSTEWARD_DAV_LIVE_PASS`.
//! - A collection path/URL for each resource type:
//!   `PIMSTEWARD_DAV_LIVE_CAL` (a calendar collection) and/or
//!   `PIMSTEWARD_DAV_LIVE_CARD` (an addressbook collection). The matching
//!   test skips if its collection var is unset.
//!
//! Without these the tests print a one-line skip and exit early.
//!
//! ## Running
//!
//! ```sh
//! export PIMSTEWARD_DAV_LIVE_URL=https://sn.purpose.dev
//! export PIMSTEWARD_DAV_LIVE_USER=dan@example.test
//! export PIMSTEWARD_DAV_LIVE_PASS=...
//! export PIMSTEWARD_DAV_LIVE_CAL=/dav/cal/dan%40example.test/default
//! export PIMSTEWARD_DAV_LIVE_CARD=/dav/card/dan%40example.test/default
//! cargo test -p pimsteward --test dav_writer_e2e -- --ignored
//! ```
//!
//! Each create test deletes the object it created in a `finally`-style
//! cleanup so the collection is left clean even on assertion failure.

use pimsteward::source::traits::{CalendarSource, CalendarWriter, ContactsSource};
use pimsteward::source::{
    DavCalendarSource, DavCalendarWriter, DavContactsSource, DavContactsWriter,
};

struct Creds {
    base_url: String,
    user: String,
    pass: String,
}

/// Read base URL + credentials, or `None` (with a skip message) if the
/// opt-in env isn't present.
fn creds() -> Option<Creds> {
    let base_url = match std::env::var("PIMSTEWARD_DAV_LIVE_URL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!(
                "dav_writer_e2e: skip — set PIMSTEWARD_DAV_LIVE_URL plus \
                 PIMSTEWARD_DAV_LIVE_USER/PASS and a collection var"
            );
            return None;
        }
    };
    let user = std::env::var("PIMSTEWARD_DAV_LIVE_USER").unwrap_or_default();
    let pass = std::env::var("PIMSTEWARD_DAV_LIVE_PASS").unwrap_or_default();
    if user.is_empty() || pass.is_empty() {
        eprintln!("dav_writer_e2e: skip — PIMSTEWARD_DAV_LIVE_USER/PASS unset");
        return None;
    }
    Some(Creds {
        base_url,
        user,
        pass,
    })
}

fn unique_uid(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pimsteward-e2e-{prefix}-{now}")
}

/// CalDAV: create an event via the writer, read it back via the source,
/// then delete it and confirm it's gone. Re-PUT same UID is idempotent.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_DAV_LIVE_URL + collection vars"]
async fn caldav_writer_roundtrip() {
    let Some(c) = creds() else { return };
    let collection = match std::env::var("PIMSTEWARD_DAV_LIVE_CAL") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!("dav_writer_e2e: caldav skip — PIMSTEWARD_DAV_LIVE_CAL unset");
            return;
        }
    };

    let writer = DavCalendarWriter::new(&c.base_url, &c.user, &c.pass)
        .expect("build DavCalendarWriter");
    let source = DavCalendarSource::new(&c.base_url, &c.user, &c.pass)
        .expect("build DavCalendarSource");

    let uid = unique_uid("evt");
    let ical = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//pimsteward//e2e//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:pimsteward e2e event\r\n\
         DTSTART:20270115T100000Z\r\nDTEND:20270115T110000Z\r\n\
         STATUS:CONFIRMED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    );

    // Create.
    let created = writer
        .create_event(&collection, &uid, &ical)
        .await
        .expect("create_event");
    assert_eq!(created.uid.as_deref(), Some(uid.as_str()));
    assert_eq!(created.summary.as_deref(), Some("pimsteward e2e event"));

    // Idempotent re-PUT of the same UID must overwrite, not duplicate.
    let ical2 = ical.replace("pimsteward e2e event", "pimsteward e2e event v2");
    writer
        .update_event(&collection, &uid, &ical2, "")
        .await
        .expect("update_event (re-PUT same UID)");

    // Read back via the source and confirm exactly one matching event.
    let events = source.list_events(None).await.expect("list_events");
    let matches: Vec<_> = events
        .iter()
        .filter(|e| e.uid.as_deref() == Some(uid.as_str()))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one event for UID {uid}, found {}",
        matches.len()
    );

    // Delete and confirm gone.
    writer
        .delete_event(&collection, &uid, "")
        .await
        .expect("delete_event");
    let after = source.list_events(None).await.expect("list_events post-delete");
    assert!(
        !after.iter().any(|e| e.uid.as_deref() == Some(uid.as_str())),
        "event {uid} still present after delete"
    );
}

/// CardDAV: create a contact via the writer, read it back via the source,
/// then delete it and confirm gone.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_DAV_LIVE_URL + collection vars"]
async fn carddav_writer_roundtrip() {
    let Some(c) = creds() else { return };
    let collection = match std::env::var("PIMSTEWARD_DAV_LIVE_CARD") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => {
            eprintln!("dav_writer_e2e: carddav skip — PIMSTEWARD_DAV_LIVE_CARD unset");
            return;
        }
    };

    let writer = DavContactsWriter::new(&c.base_url, &c.user, &c.pass)
        .expect("build DavContactsWriter");
    let source = DavContactsSource::new(&c.base_url, &c.user, &c.pass)
        .expect("build DavContactsSource");

    let uid = unique_uid("card");
    let vcard = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Pimsteward E2E\r\n\
         EMAIL:e2e@example.com\r\nEND:VCARD\r\n"
    );

    let created = writer
        .create_contact(&collection, &uid, &vcard)
        .await
        .expect("create_contact");
    assert_eq!(created.uid, uid);
    assert_eq!(created.full_name, "Pimsteward E2E");

    let contacts = source.list_contacts().await.expect("list_contacts");
    let matches: Vec<_> = contacts.iter().filter(|c| c.uid == uid).collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one contact for UID {uid}, found {}",
        matches.len()
    );

    writer
        .delete_contact(&collection, &uid, "")
        .await
        .expect("delete_contact");
    let after = source.list_contacts().await.expect("list_contacts post-delete");
    assert!(
        !after.iter().any(|c| c.uid == uid),
        "contact {uid} still present after delete"
    );
}
