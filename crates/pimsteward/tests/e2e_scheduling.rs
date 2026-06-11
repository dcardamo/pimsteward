//! REAL end-to-end acceptance gate for the organizer-side iMIP scheduler.
//!
//! Dan's CalDAV provider (ForwardEmail) does NOT itself send calendar invites,
//! so pimsteward grew an organizer-side iMIP sender. Before that goes live on
//! Dan's real calendar, this gate proves — against the *real* ForwardEmail API
//! — that the full production path (real pull -> watermark gate -> real
//! `run_scheduling`) actually delivers `METHOD:REQUEST`, an updated REQUEST,
//! and `METHOD:CANCEL` emails to an invitee's inbox over the entire create /
//! update / cancel lifecycle.
//!
//! Safety: the ONLY attendee is `rocky@hld.ca`, a mailbox Dan owns. The
//! organizer is `dan@hld.ca`, also Dan's. No third party is ever emailed.
//! The event is created on Dan's real calendar but is a far-future
//! (year 2099) throwaway and is deleted again in the CANCEL step + teardown.
//! Dan explicitly authorized this test.
//!
//! Run live (needs network + creds):
//!   cargo test -p pimsteward --test e2e_scheduling -- --ignored --nocapture
//! The test SKIPS (prints a message and returns) if the four credential env
//! vars are not set, so a normal `cargo test` run is unaffected.

use async_trait::async_trait;
use chrono::Utc;
use pimsteward::error::Error;
use pimsteward::forwardemail::writes::NewMessage;
use pimsteward::forwardemail::Client;
use pimsteward::pull::calendar::pull_calendar;
use pimsteward::scheduling::watermark::ensure_watermark;
use pimsteward::scheduling::{run_scheduling, Sender};
use pimsteward::source::RestCalendarSource;
use pimsteward::store::Repo;
use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_BASE: &str = "https://api.forwardemail.net";
const DAN_EMAIL: &str = "dan@hld.ca";
const ROCKY_EMAIL: &str = "rocky@hld.ca";

/// Thin adapter wrapping the dan `Client` so the real `run_scheduling`
/// orchestrator can drive sends. Deliberately minimal — it does NOT
/// reimplement any planning/dedup logic, it just forwards to the same
/// `Client::send_imip` / `Client::send_email` the daemon uses.
struct DanSender {
    client: Client,
}

#[async_trait]
impl Sender for DanSender {
    async fn send_imip(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        payload: &str,
        method: &str,
    ) -> Result<String, Error> {
        let resp = self
            .client
            .send_imip(to, subject, text_body, payload, method)
            .await?;
        // ForwardEmail returns the queued message object; surface its id.
        let id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok(id)
    }

    async fn notify(&self, subject: &str, body: &str) -> Result<(), Error> {
        let msg = NewMessage {
            folder: String::new(),
            to: vec![DAN_EMAIL.to_string()],
            cc: vec![],
            bcc: vec![],
            subject: subject.to_string(),
            text: Some(body.to_string()),
            html: None,
            in_reply_to: None,
            references: vec![],
        };
        self.client.send_email(&msg).await?;
        Ok(())
    }
}

/// Build a VCALENDAR+VEVENT with dan@ as organizer and rocky@ as the sole
/// attendee. `dtstart`/`seq` let the update step vary the event.
fn build_ics(uid: &str, dtstart: &str, summary: &str, seq: u32) -> String {
    [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//pimsteward//e2e-scheduling//EN",
        "CALSCALE:GREGORIAN",
        "BEGIN:VEVENT",
        &format!("UID:{uid}"),
        "DTSTAMP:20260101T000000Z",
        &format!("DTSTART:{dtstart}"),
        &format!("DTEND:{}", dtstart.replace("T120000Z", "T130000Z")),
        &format!("SUMMARY:{summary}"),
        &format!("SEQUENCE:{seq}"),
        &format!("ORGANIZER;EMAIL={DAN_EMAIL}:mailto:{DAN_EMAIL}"),
        &format!("ATTENDEE;EMAIL={ROCKY_EMAIL}:mailto:{ROCKY_EMAIL}"),
        "END:VEVENT",
        "END:VCALENDAR",
    ]
    .join("\r\n")
}

/// Poll rocky's INBOX for a *new* (not-yet-consumed) message whose full JSON
/// contains the uid AND every `must_contain` token. On success records the
/// id in `consumed` and returns it. Up to ~90s, every ~5s.
async fn poll_for_message(
    rocky: &Client,
    uid: &str,
    must_contain: &[&str],
    consumed: &mut HashSet<String>,
) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let msgs = match rocky.list_messages_in_folder("INBOX").await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  [poll] list INBOX failed: {e}");
                vec![]
            }
        };
        for m in &msgs {
            if consumed.contains(&m.id) {
                continue;
            }
            let full = match rocky.get_message(&m.id).await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("  [poll] get_message {} failed: {e}", m.id);
                    continue;
                }
            };
            let hay = full.to_string();
            let hit = hay.contains(uid) && must_contain.iter().all(|t| hay.contains(t));
            if hit {
                eprintln!(
                    "  [poll] MATCH id={} subject={:?} (tokens {:?})",
                    m.id, m.subject, must_contain
                );
                consumed.insert(m.id.clone());
                return Some(m.id.clone());
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Best-effort: delete every rocky INBOX message whose JSON mentions the uid.
async fn cleanup_rocky_inbox(rocky: &Client, uid: &str) -> usize {
    let mut deleted = 0;
    if let Ok(msgs) = rocky.list_messages_in_folder("INBOX").await {
        for m in &msgs {
            if let Ok(full) = rocky.get_message(&m.id).await {
                if full.to_string().contains(uid) {
                    if rocky.delete_message(&m.id).await.is_ok() {
                        deleted += 1;
                    }
                }
            }
        }
    }
    deleted
}

#[tokio::test]
#[ignore = "real ForwardEmail e2e; needs creds + network"]
async fn e2e_rocky_invite_lifecycle() {
    // ── Credentials (skip cleanly if unset) ──────────────────────────
    let (Ok(dan_user), Ok(dan_pass), Ok(rocky_user), Ok(rocky_pass)) = (
        std::env::var("PIMSTEWARD_DAN_USER"),
        std::env::var("PIMSTEWARD_DAN_PASS"),
        std::env::var("PIMSTEWARD_ROCKY_USER"),
        std::env::var("PIMSTEWARD_ROCKY_PASS"),
    ) else {
        eprintln!(
            "SKIP e2e_rocky_invite_lifecycle: set PIMSTEWARD_DAN_USER / \
             PIMSTEWARD_DAN_PASS / PIMSTEWARD_ROCKY_USER / PIMSTEWARD_ROCKY_PASS \
             to run this real-ForwardEmail gate."
        );
        return;
    };

    let dan = Client::new(API_BASE, dan_user, dan_pass).expect("dan client");
    let rocky = Client::new(API_BASE, rocky_user, rocky_pass).expect("rocky client");

    let source = RestCalendarSource::new(dan.clone());
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repo::open_or_init(tmp.path()).expect("repo init");

    // ── Pick the calendar named "Dan" (fallback: first) ──────────────
    let cals = dan.list_calendars().await.expect("list_calendars");
    assert!(!cals.is_empty(), "dan has no calendars");
    let cal = cals
        .iter()
        .find(|c| c.name == "Dan")
        .unwrap_or(&cals[0])
        .clone();
    let cal_id = cal.id.clone();
    eprintln!("Using calendar id={cal_id} name={:?}", cal.name);

    // ── Baseline pull -> watermark (makes pre-existing events ineligible) ─
    let base = pull_calendar(&source, &repo, DAN_EMAIL, "test", "test@test")
        .await
        .expect("baseline pull");
    let base_sha = match base.commit_sha {
        Some(s) => s,
        // Nothing to commit: anchor the watermark on an empty commit that
        // predates the test event.
        None => repo
            .empty_commit("test", "test@test", "e2e baseline anchor")
            .expect("anchor commit"),
    };
    ensure_watermark(&repo, &base_sha).expect("watermark");
    eprintln!("Watermark anchored at {base_sha}");

    let sender = DanSender { client: dan.clone() };
    let mut consumed: HashSet<String> = HashSet::new();

    // Unique uid + summary so we never collide with real events.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let uid = format!("pimsteward-e2e-{ts}");
    let summary = format!("pimsteward e2e gate {ts}");

    // ── 1. CREATE ────────────────────────────────────────────────────
    let ics1 = build_ics(&uid, "20990101T120000Z", &summary, 0);
    let created = dan
        .create_calendar_event(&cal_id, &ics1, Some(&uid))
        .await
        .expect("create_calendar_event");
    let event_id = created.id.clone();
    eprintln!("Created event id={event_id} uid={:?}", created.uid);

    // Pull the event back and inspect what ForwardEmail actually stored —
    // it normalizes the ical server-side and MAY strip/rewrite the
    // organizer/attendee, which would silently break scheduling.
    let p1 = pull_calendar(&source, &repo, DAN_EMAIL, "test", "test@test")
        .await
        .expect("pull after create");
    let sha1 = p1.commit_sha.clone().unwrap_or_default();
    eprintln!("Pull after CREATE: +{} ~{} -{} sha={sha1}", p1.added, p1.updated, p1.deleted);

    let stored = read_stored_ics(&repo, &uid);
    eprintln!("─── stored ics after CREATE ───\n{stored}\n──────────────────────────────");
    let org = pimsteward_ical::imip::organizer(&stored);
    let atts = pimsteward_ical::imip::attendees(&stored);
    eprintln!("Resolved organizer={org:?} attendees={atts:?}");
    assert!(
        org.as_ref().map(|o| o.email.as_str()) == Some(DAN_EMAIL),
        "ForwardEmail normalization dropped/changed ORGANIZER: stored ics did not \
         resolve to {DAN_EMAIL} (got {org:?}). This is a real production-path finding."
    );
    assert!(
        atts.iter().any(|a| a.email == ROCKY_EMAIL),
        "ForwardEmail normalization dropped ATTENDEE {ROCKY_EMAIL}: stored ics \
         resolved attendees {atts:?}. This is a real production-path finding."
    );

    assert!(!sha1.is_empty(), "CREATE pull produced no commit");
    let n1 = run_scheduling(&repo, &sha1, &sender, DAN_EMAIL, false, Utc::now())
        .await
        .expect("run_scheduling create");
    eprintln!("run_scheduling after CREATE sent {n1} message(s)");
    assert_eq!(n1, 1, "expected exactly one REQUEST to be sent on CREATE");

    let req1 = poll_for_message(&rocky, &uid, &["METHOD:REQUEST"], &mut consumed).await;
    assert!(
        req1.is_some(),
        "METHOD:REQUEST for uid {uid} never arrived in rocky's INBOX"
    );
    eprintln!("STAGE 1 OK: initial METHOD:REQUEST received (msg {req1:?})");

    // ── 2. UPDATE time (SEQUENCE stays 0 in the ics — proves the
    //        content-fingerprint path; scheduler bumps to SEQUENCE:1) ──
    let ics2 = build_ics(&uid, "20990101T150000Z", &summary, 0);
    dan.update_calendar_event(&event_id, Some(&ics2), None, None)
        .await
        .expect("update_calendar_event");
    eprintln!("Updated event time");

    let p2 = pull_calendar(&source, &repo, DAN_EMAIL, "test", "test@test")
        .await
        .expect("pull after update");
    let sha2 = p2.commit_sha.clone().unwrap_or_default();
    eprintln!("Pull after UPDATE: +{} ~{} -{} sha={sha2}", p2.added, p2.updated, p2.deleted);
    assert!(!sha2.is_empty(), "UPDATE pull produced no commit");
    let n2 = run_scheduling(&repo, &sha2, &sender, DAN_EMAIL, false, Utc::now())
        .await
        .expect("run_scheduling update");
    eprintln!("run_scheduling after UPDATE sent {n2} message(s)");
    assert_eq!(n2, 1, "expected exactly one updated REQUEST on UPDATE");

    // Second REQUEST: a NEW message (consumed set excludes the first) that
    // carries the bumped SEQUENCE:1.
    let req2 = poll_for_message(
        &rocky,
        &uid,
        &["METHOD:REQUEST", "SEQUENCE:1"],
        &mut consumed,
    )
    .await;
    assert!(
        req2.is_some(),
        "updated METHOD:REQUEST (SEQUENCE:1) for uid {uid} never arrived"
    );
    eprintln!("STAGE 2 OK: updated METHOD:REQUEST (SEQUENCE:1) received (msg {req2:?})");

    // ── 3. CANCEL ────────────────────────────────────────────────────
    dan.delete_calendar_event(&event_id)
        .await
        .expect("delete_calendar_event");
    eprintln!("Deleted event (cancel)");

    let p3 = pull_calendar(&source, &repo, DAN_EMAIL, "test", "test@test")
        .await
        .expect("pull after cancel");
    let sha3 = p3.commit_sha.clone().unwrap_or_default();
    eprintln!("Pull after CANCEL: +{} ~{} -{} sha={sha3}", p3.added, p3.updated, p3.deleted);
    assert!(!sha3.is_empty(), "CANCEL pull produced no commit");
    let n3 = run_scheduling(&repo, &sha3, &sender, DAN_EMAIL, false, Utc::now())
        .await
        .expect("run_scheduling cancel");
    eprintln!("run_scheduling after CANCEL sent {n3} message(s)");
    assert_eq!(n3, 1, "expected exactly one CANCEL on delete");

    let cancel = poll_for_message(&rocky, &uid, &["METHOD:CANCEL"], &mut consumed).await;
    assert!(
        cancel.is_some(),
        "METHOD:CANCEL for uid {uid} never arrived in rocky's INBOX"
    );
    eprintln!("STAGE 3 OK: METHOD:CANCEL received (msg {cancel:?})");

    // ── Teardown (best-effort) ───────────────────────────────────────
    let _ = dan.delete_calendar_event(&event_id).await; // already gone; ignore
    let removed = cleanup_rocky_inbox(&rocky, &uid).await;
    eprintln!("Teardown: removed {removed} rocky INBOX message(s) for uid {uid}");

    eprintln!("ALL THREE LIFECYCLE STAGES CONFIRMED RECEIVED ✔");
}

/// Read the stored `.ics` for `uid` out of the pulled repo (events are keyed
/// by a filename-safe form of the uid; our uid is already filename-safe).
fn read_stored_ics(repo: &Repo, uid: &str) -> String {
    let root = repo.root();
    // Walk calendars/*/events/<uid>.ics
    let cals = root.join("calendars");
    if let Ok(dirs) = std::fs::read_dir(&cals) {
        for d in dirs.flatten() {
            let p = d.path().join("events").join(format!("{uid}.ics"));
            if p.exists() {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    return s;
                }
            }
        }
    }
    String::new()
}
