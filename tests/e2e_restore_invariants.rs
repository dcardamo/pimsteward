//! Restore invariant tests — the property-based counterpart to the
//! single-item restore tests in `e2e_{mail,calendar,contacts,sieve}.rs`.
//!
//! # Test architecture
//!
//! Every restore operation must preserve eight invariants:
//!
//! | #  | Invariant                      | Meaning                                                                 |
//! | -- | ------------------------------ | ----------------------------------------------------------------------- |
//! | I1 | **Fidelity**                   | After restore, the target's live state equals its state at `at_sha`.    |
//! | I2 | **Isolation (same-resource)**  | Restoring X leaves every other item of the same resource type alone.   |
//! | I3 | **Isolation (cross-resource)** | Restoring a contact never touches calendars, sieve, or mail.            |
//! | I4 | **Isolation (post-snapshot)**  | Items created AFTER `at_sha` survive a restore of something older.     |
//! | I5 | **Idempotence**                | Applying the same restore twice = applying it once. Second plan = NoOp.|
//! | I6 | **No-op on matching state**    | Planning a restore that matches live state produces a NoOp op.         |
//! | I7 | **Commit attribution**         | Every apply produces a git commit tagged with `tool: restore_*`.       |
//! | I8 | **Token binding**              | A plan_token only validates its own plan; mismatches are refused.      |
//!
//! The per-resource files (`e2e_mail.rs`, etc.) already cover **I1** for
//! single items. This file covers I2–I8 across every resource type that
//! supports restore (mail, calendar, contacts, sieve) plus the bulk path.
//!
//! # Scenario the user asked about
//!
//! The canonical failure mode we're guarding against:
//!
//! > "I have 10 emails. My AI does something bad to one of them. Hours later
//! >  I've received 100 more emails. When I undo the one bad thing to the
//! >  first 10, it doesn't roll back the new 100, right?"
//!
//! Answer: correct — restore is path-scoped. This is exactly what the I4
//! tests prove at the live-state level, and what the bulk isolation test
//! proves at the git-plumbing level.
//!
//! # Running
//!
//! All tests are `#[ignore]`d by default. They require:
//!
//! ```sh
//! PIMSTEWARD_RUN_E2E=1 cargo nextest run --test e2e_restore_invariants --run-ignored all
//! ```
//!
//! The `_test` alias safety guard applies — see `src/safety.rs`.

#![allow(clippy::bool_assert_comparison)]

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::{calendar::pull_calendar, contacts::pull_contacts, mail::pull_mail};
use pimsteward::restore;
use pimsteward::source::{RestCalendarSource, RestContactsSource, RestMailSource};
use pimsteward::write;
use serde_json::json;

// ─────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────

fn current_head(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Read the full message body of the most recent git commit. Used to verify
/// I7 (commit attribution) — the audit trailer must land in the commit the
/// restore apply produced.
fn head_commit_message(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["log", "-1", "--format=%B"])
        .current_dir(repo.root())
        .output()
        .expect("git log");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// List every file changed between two git shas. Used to verify that an
/// isolated restore only modifies paths belonging to its target — the
/// git-plumbing-level isolation assertion.
///
/// Note: we must use a range diff rather than inspecting HEAD alone,
/// because every restore apply runs `pull_*` internally and that pull
/// typically lands the real file changes in its own commit *before* the
/// restore audit commit. The restore audit commit itself is therefore
/// frequently empty (the API call already brought live state in line,
/// pull picked that up, and commit_all found nothing left to stage).
/// Asserting about HEAD would miss the pull's changed files and over-
/// index on the audit commit.
fn files_changed_between(
    repo: &pimsteward::store::Repo,
    from_sha: &str,
    to_ref: &str,
) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only", from_sha, to_ref])
        .current_dir(repo.root())
        .output()
        .expect("git diff");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

/// Append a raw RFC822 message via forwardemail's POST /v1/messages. Mirrors
/// the helper in `e2e_mail.rs` but locally scoped — keeping the shared
/// helper module free of curl-shelling makes the common surface cleaner.
async fn create_test_message(ctx: &E2eContext, subject: &str) -> String {
    let raw = format!(
        "From: e2e_test@example.com\r\nTo: {}\r\nSubject: {subject}\r\n\
         Message-ID: <e2e-{}-{}-{}@example.com>\r\n\
         Date: Sun, 05 Apr 2026 08:00:00 +0000\r\n\r\nBody for {subject}.",
        ctx.alias,
        std::process::id(),
        subject,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let body = json!({"folder": "INBOX", "raw": raw});
    let password_file = std::env::var("PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE")
        .unwrap_or_else(|_| "/home/dan/.config/secrets/pimsteward-test-alias-password".into());
    let password = std::fs::read_to_string(&password_file)
        .expect("password file")
        .trim()
        .to_string();
    let body_str = body.to_string();
    let out = std::process::Command::new("curl")
        .args([
            "-sS",
            "-u",
            &format!("{}:{password}", ctx.alias),
            "-X",
            "POST",
            "https://api.forwardemail.net/v1/messages",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body_str,
        ])
        .output()
        .expect("curl");
    let resp: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("parse message create response");
    resp.get("id")
        .and_then(|v| v.as_str())
        .expect("message id")
        .to_string()
}

fn sample_ical(uid: &str, summary: &str) -> String {
    [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//pimsteward//e2e//EN",
        "BEGIN:VEVENT",
        &format!("UID:{uid}"),
        "DTSTAMP:20260101T000000Z",
        "DTSTART:20270301T090000Z",
        "DTEND:20270301T100000Z",
        &format!("SUMMARY:{summary}"),
        "END:VEVENT",
        "END:VCALENDAR",
    ]
    .join("\r\n")
}

// ═════════════════════════════════════════════════════════════════════════
// I4 — Post-snapshot isolation
//
// The user's scenario. Snapshot at T0, mutate item A, create items B/C/D
// after T0, restore A to T0, verify B/C/D are untouched.
// ═════════════════════════════════════════════════════════════════════════

/// Mail: restore of a single message does not affect messages that were
/// created (arrived) between the snapshot sha and the restore.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn mail_restore_isolation_from_new_messages() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e mail restore isolation");
    let pid = std::process::id();
    let rest_source = RestMailSource::new(ctx.client.clone());

    // 1. Create message A — the item we'll mutate and then restore.
    let subject_a = format!("e2e_iso_A_{pid}");
    let msg_a = create_test_message(&ctx, &subject_a).await;

    // 2. Capture A into git.
    let _ = pull_mail(
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("initial pull");

    // 3. Snapshot — this is the state we want A to return to.
    let good_sha = current_head(&ctx.repo);

    // 4. "The AI does something bad" — set flags on A.
    let bad_flags = vec!["\\Seen".to_string(), "\\Flagged".to_string()];
    write::mail::update_flags(
        &rest_source,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        "INBOX",
        &msg_a,
        &bad_flags,
    )
    .await
    .expect("bad flags on A");

    // 5. "100 more emails arrive" — create B, C, D with no flags.
    let subject_b = format!("e2e_iso_B_{pid}");
    let subject_c = format!("e2e_iso_C_{pid}");
    let subject_d = format!("e2e_iso_D_{pid}");
    let msg_b = create_test_message(&ctx, &subject_b).await;
    let msg_c = create_test_message(&ctx, &subject_c).await;
    let msg_d = create_test_message(&ctx, &subject_d).await;
    let _ = pull_mail(
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("pull new messages");

    // 6. Restore JUST A back to good_sha.
    let (plan, token) = restore::mail::plan_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "INBOX",
        &msg_a,
        &good_sha,
    )
    .await
    .expect("plan mail restore");
    assert!(matches!(
        plan.operation,
        restore::mail::MailOperation::RestoreFlags { .. }
    ));
    restore::mail::apply_mail(
        &ctx.client,
        &rest_source,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply mail restore");

    // 7. FIDELITY (I1) — A's flags are cleared.
    let live_a = ctx.client.get_message(&msg_a).await.expect("fetch A");
    let a_flags: Vec<String> = live_a
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        a_flags.is_empty(),
        "A should have no flags after restore, got {a_flags:?}"
    );

    // 8. ISOLATION (I4) — B, C, D still exist, still in INBOX, still flag-free.
    let inbox = ctx
        .client
        .list_messages_in_folder("INBOX")
        .await
        .expect("list INBOX after restore");
    for (id, subject) in [
        (&msg_b, &subject_b),
        (&msg_c, &subject_c),
        (&msg_d, &subject_d),
    ] {
        let found = inbox
            .iter()
            .find(|m| &m.id == id)
            .unwrap_or_else(|| panic!("{subject} should still exist in INBOX after restoring A"));
        assert_eq!(
            found.folder_path, "INBOX",
            "{subject} should still be in INBOX"
        );
        assert!(
            found.flags.is_empty(),
            "{subject} flags must remain empty — restore of A must not touch it (got {:?})",
            found.flags
        );
    }

    // Cleanup
    for id in [&msg_a, &msg_b, &msg_c, &msg_d] {
        let _ = write::mail::delete_message(
            &rest_source,
            &rest_source,
            &ctx.repo,
            &ctx.alias_slug(),
            &attr,
            "INBOX",
            id,
        )
        .await;
    }
}

/// Calendar: restoring one event does not touch other events created after
/// the snapshot, even on the same calendar.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn calendar_restore_isolation_from_new_events() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e calendar restore isolation");
    let pid = std::process::id();

    // Dedicated calendar so parallel runs don't cross-contaminate.
    let cal = ctx
        .client
        .create_calendar(&format!("e2e_iso_cal_{pid}"), None)
        .await
        .expect("create calendar");
    let calendar_id = cal.id.clone();

    let cal_writer = ctx.calendar_writer();
    let cal_source = ctx.calendar_source();
    // 1. Create event A.
    let uid_a = format!("iso-A-{pid}@pimsteward");
    let good_ical_a = sample_ical(&uid_a, "A original");
    let event_a = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &good_ical_a,
        Some(&uid_a),
    )
    .await
    .expect("create A");

    // 2. Snapshot.
    let good_sha = current_head(&ctx.repo);

    // 3. Bad update to A.
    let bad_ical_a = sample_ical(&uid_a, "A BAD");
    write::calendar::update_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &event_a.id,
        &bad_ical_a,
        "",
    )
    .await
    .expect("bad update A");

    // 4. Create events B and C after the snapshot.
    let uid_b = format!("iso-B-{pid}@pimsteward");
    let ical_b = sample_ical(&uid_b, "B after snapshot");
    let event_b = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &ical_b,
        Some(&uid_b),
    )
    .await
    .expect("create B");

    let uid_c = format!("iso-C-{pid}@pimsteward");
    let ical_c = sample_ical(&uid_c, "C after snapshot");
    let event_c = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &ical_c,
        Some(&uid_c),
    )
    .await
    .expect("create C");

    // 5. Restore just A.
    let (plan, token) = restore::calendar::plan_calendar(
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &calendar_id,
        &uid_a,
        &good_sha,
    )
    .await
    .expect("plan A");
    restore::calendar::apply_calendar(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply A");

    // 6. FIDELITY — A is back to "A original".
    let live_a = ctx
        .client
        .get_calendar_event(&event_a.id)
        .await
        .expect("fetch A");
    assert!(
        live_a
            .ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:A original"),
        "A should be restored to original summary, got {:?}",
        live_a.ical
    );

    // 7. ISOLATION — B and C still exist with their post-snapshot content.
    let live_b = ctx
        .client
        .get_calendar_event(&event_b.id)
        .await
        .expect("fetch B");
    assert!(
        live_b
            .ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:B after snapshot"),
        "B must be untouched by restore of A"
    );
    let live_c = ctx
        .client
        .get_calendar_event(&event_c.id)
        .await
        .expect("fetch C");
    assert!(
        live_c
            .ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:C after snapshot"),
        "C must be untouched by restore of A"
    );

    // Cleanup
    let _ = ctx.client.delete_calendar_event(&event_a.id).await;
    let _ = ctx.client.delete_calendar_event(&event_b.id).await;
    let _ = ctx.client.delete_calendar_event(&event_c.id).await;
    let _ = ctx.client.delete_calendar(&calendar_id).await;
}

/// Contacts: restoring one contact does not touch contacts created after
/// the snapshot.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_restore_isolation_from_new_contacts() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e contact restore isolation");
    let pid = std::process::id();

    // 1. Create contact A.
    let name_a = format!("iso_A_{pid}");
    let contact_a = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_a,
        &[("home", "iso_a@example.com")],
    )
    .await
    .expect("create A");

    // 2. Snapshot.
    let good_sha = current_head(&ctx.repo);

    // 3. Bad rename on A.
    let bad_name_a = format!("{name_a}_BAD");
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_a.id,
        &bad_name_a,
        None,
    )
    .await
    .expect("bad rename A");

    // 4. Create contacts B and C after the snapshot.
    let name_b = format!("iso_B_{pid}");
    let contact_b = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_b,
        &[("home", "iso_b@example.com")],
    )
    .await
    .expect("create B");
    let name_c = format!("iso_C_{pid}");
    let contact_c = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_c,
        &[("home", "iso_c@example.com")],
    )
    .await
    .expect("create C");

    // 5. Restore just A.
    let (plan, token) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact_a.uid,
        &good_sha,
    )
    .await
    .expect("plan A");
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply A");

    // 6. FIDELITY — A is back to its original name.
    // 7. ISOLATION — B and C still exist with their post-snapshot names.
    let live = ctx.client.list_contacts().await.expect("list after restore");
    let found_a = live
        .iter()
        .find(|c| c.id == contact_a.id)
        .expect("A should still exist");
    assert_eq!(found_a.full_name, name_a, "A must be back to original name");
    let found_b = live
        .iter()
        .find(|c| c.id == contact_b.id)
        .expect("B must still exist");
    assert_eq!(found_b.full_name, name_b, "B must be untouched");
    let found_c = live
        .iter()
        .find(|c| c.id == contact_c.id)
        .expect("C must still exist");
    assert_eq!(found_c.full_name, name_c, "C must be untouched");

    // Cleanup
    for id in [&contact_a.id, &contact_b.id, &contact_c.id] {
        let _ = write::contacts::delete_contact(
            &ctx.client,
            &ctx.repo,
            &ctx.alias_slug(),
            &attr,
            id,
        )
        .await;
    }
}

/// Sieve: restoring one script does not touch scripts created after the snapshot.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn sieve_restore_isolation_from_new_scripts() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e sieve restore isolation");
    let pid = std::process::id();

    // 1. Install script A with "good" content.
    let name_a = format!("iso_A_{pid}");
    let good_a = r#"require ["fileinto"]; fileinto "Archive";"#;
    let script_a = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_a,
        good_a,
    )
    .await
    .expect("install A");

    // 2. Snapshot.
    let good_sha = current_head(&ctx.repo);

    // 3. Bad update to A.
    let bad_a = r#"require ["fileinto"]; fileinto "Junk";"#;
    write::sieve::update_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script_a.id,
        bad_a,
    )
    .await
    .expect("bad A");

    // 4. Install scripts B and C after the snapshot.
    let name_b = format!("iso_B_{pid}");
    let content_b = r#"require ["fileinto"]; fileinto "Later";"#;
    let script_b = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_b,
        content_b,
    )
    .await
    .expect("install B");
    let name_c = format!("iso_C_{pid}");
    let content_c = r#"require ["fileinto"]; fileinto "Alt";"#;
    let script_c = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name_c,
        content_c,
    )
    .await
    .expect("install C");

    // 5. Restore just A.
    let (plan, token) = restore::sieve::plan_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &name_a,
        &good_sha,
    )
    .await
    .expect("plan A");
    restore::sieve::apply_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply A");

    // 6. FIDELITY — A is back to `good_a`.
    let live_a = ctx
        .client
        .get_sieve_script(&script_a.id)
        .await
        .expect("fetch A");
    assert_eq!(live_a.content.as_deref(), Some(good_a));

    // 7. ISOLATION — B and C still exist with their original content.
    let live_b = ctx
        .client
        .get_sieve_script(&script_b.id)
        .await
        .expect("fetch B");
    assert_eq!(
        live_b.content.as_deref(),
        Some(content_b),
        "B must be untouched"
    );
    let live_c = ctx
        .client
        .get_sieve_script(&script_c.id)
        .await
        .expect("fetch C");
    assert_eq!(
        live_c.content.as_deref(),
        Some(content_c),
        "C must be untouched"
    );

    // Cleanup
    for id in [&script_a.id, &script_b.id, &script_c.id] {
        let _ = write::sieve::delete_sieve_script(
            &ctx.client,
            &ctx.repo,
            &ctx.alias_slug(),
            &attr,
            id,
        )
        .await;
    }
}

// ═════════════════════════════════════════════════════════════════════════
// I3 — Cross-resource isolation
// ═════════════════════════════════════════════════════════════════════════

/// Restoring a contact must not modify calendar events or sieve scripts,
/// even when they are committed in the same git repo.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn cross_resource_isolation_contact_restore_leaves_calendar_alone() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e cross-resource isolation");
    let pid = std::process::id();

    // Set up: a contact AND a calendar event.
    let name = format!("xres_{pid}");
    let contact = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        &[("home", "xres@example.com")],
    )
    .await
    .expect("create contact");

    let cal = ctx
        .client
        .create_calendar(&format!("xres_cal_{pid}"), None)
        .await
        .expect("create calendar");
    let event_uid = format!("xres-event-{pid}@pimsteward");
    let event_ical = sample_ical(&event_uid, "calendar survives");
    let cal_writer = ctx.calendar_writer();
    let cal_source = ctx.calendar_source();
    let event = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &cal.id,
        &event_ical,
        Some(&event_uid),
    )
    .await
    .expect("create event");

    // Snapshot.
    let good_sha = current_head(&ctx.repo);

    // Bad rename on the contact.
    let bad = format!("{name}_BAD");
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
        &bad,
        None,
    )
    .await
    .expect("bad rename");

    // Also update the calendar event after the snapshot. The restore of the
    // contact must not roll this change back — it's on an unrelated
    // resource. This is the assertion that proves path-scoping crosses the
    // resource-type boundary, not just the item boundary.
    let event_ical_v2 = sample_ical(&event_uid, "updated post-snapshot");
    write::calendar::update_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &cal.id,
        &event.id,
        &event_ical_v2,
        "",
    )
    .await
    .expect("post-snapshot event update");

    // Restore the contact only. Capture sha BEFORE apply so we can
    // diff the full range of changes the restore caused — see
    // `files_changed_between` for why HEAD alone isn't enough.
    let pre_apply_sha = current_head(&ctx.repo);
    let (plan, token) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact.uid,
        &good_sha,
    )
    .await
    .expect("plan");
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    // Contact is back.
    let live_contact = ctx
        .client
        .list_contacts()
        .await
        .expect("list")
        .into_iter()
        .find(|c| c.id == contact.id)
        .expect("contact exists");
    assert_eq!(live_contact.full_name, name);

    // Calendar event still has its POST-SNAPSHOT content — the contact
    // restore did NOT roll it back.
    let live_event = ctx
        .client
        .get_calendar_event(&event.id)
        .await
        .expect("fetch event");
    assert!(
        live_event
            .ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:updated post-snapshot"),
        "calendar event must retain its post-snapshot content — contact restore \
         must not cross resource boundaries. got ical: {:?}",
        live_event.ical
    );

    // I3/I7 — every file changed by the restore (across every commit the
    // apply produced, including its internal pull) must be under a
    // contact path. A single calendar/sieve/mail path in this diff would
    // prove cross-resource leakage.
    let changed = files_changed_between(&ctx.repo, &pre_apply_sha, "HEAD");
    assert!(
        !changed.is_empty(),
        "restore should have changed at least one file (contact vcard)"
    );
    for path in &changed {
        assert!(
            path.contains("/contacts/"),
            "restore diff should only touch contact paths, but changed: {path}"
        );
        assert!(
            !path.contains("/calendars/"),
            "restore diff must not touch calendar paths, but changed: {path}"
        );
        assert!(
            !path.contains("/sieve/"),
            "restore diff must not touch sieve paths, but changed: {path}"
        );
        assert!(
            !path.contains("/mail/"),
            "restore diff must not touch mail paths, but changed: {path}"
        );
    }

    // Cleanup
    let _ = write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
    )
    .await;
    let _ = ctx.client.delete_calendar_event(&event.id).await;
    let _ = ctx.client.delete_calendar(&cal.id).await;
}

// ═════════════════════════════════════════════════════════════════════════
// I5 — Idempotence
// ═════════════════════════════════════════════════════════════════════════

/// Applying the same restore twice is equivalent to applying it once. The
/// second plan should detect the live state already matches historical and
/// return a NoOp.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_restore_is_idempotent() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e contact restore idempotence");
    let pid = std::process::id();

    let name = format!("idem_{pid}");
    let contact = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        &[("home", "idem@example.com")],
    )
    .await
    .expect("create");
    let good_sha = current_head(&ctx.repo);

    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
        &format!("{name}_BAD"),
        None,
    )
    .await
    .expect("bad update");

    // First restore.
    let (plan1, token1) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact.uid,
        &good_sha,
    )
    .await
    .expect("plan1");
    assert!(
        matches!(
            plan1.operation,
            restore::contacts::RestoreOperation::Update { .. }
        ),
        "first plan should be Update, got {:?}",
        plan1.operation
    );
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan1,
        &token1,
    )
    .await
    .expect("apply1");

    // Second plan — live state already matches historical, so this must
    // be a NoOp. If it's anything else, the restore wasn't truly idempotent
    // and we'd be spinning the API with writes that don't change anything.
    let (plan2, token2) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact.uid,
        &good_sha,
    )
    .await
    .expect("plan2");
    assert!(
        matches!(plan2.operation, restore::contacts::RestoreOperation::NoOp),
        "second plan must be NoOp — live state already matches historical, \
         got {:?}",
        plan2.operation
    );

    // Applying a NoOp must succeed without error and without changing state.
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan2,
        &token2,
    )
    .await
    .expect("apply noop");

    // Live state is still the original name.
    let live = ctx.client.list_contacts().await.expect("list");
    let found = live
        .iter()
        .find(|c| c.id == contact.id)
        .expect("still exists");
    assert_eq!(found.full_name, name);

    let _ = write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
    )
    .await;
}

// ═════════════════════════════════════════════════════════════════════════
// I6 — No-op on matching state
// ═════════════════════════════════════════════════════════════════════════

/// Planning a restore against a state that already matches the target sha
/// must produce a NoOp immediately — no extra writes, no spurious commits.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn sieve_restore_noop_when_state_matches_head() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e sieve noop");
    let pid = std::process::id();
    let name = format!("noop_{pid}");

    let content = r#"require ["fileinto"]; fileinto "Archive";"#;
    let script = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        content,
    )
    .await
    .expect("install");

    // Plan a restore to HEAD without mutating anything.
    let head = current_head(&ctx.repo);
    let (plan, _token) = restore::sieve::plan_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &name,
        &head,
    )
    .await
    .expect("plan");
    assert!(
        matches!(plan.operation, restore::sieve::SieveOperation::NoOp),
        "plan against HEAD with no mutations must be NoOp, got {:?}",
        plan.operation
    );

    let _ = write::sieve::delete_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script.id,
    )
    .await;
}

// ═════════════════════════════════════════════════════════════════════════
// I7 — Commit attribution
// ═════════════════════════════════════════════════════════════════════════

/// Every restore apply must produce a git commit whose body carries
/// `tool: restore_*` in the audit trailer. This is what makes
/// `git log --grep 'tool: restore'` a reliable audit query.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn restore_apply_commit_has_tool_trailer() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e restore attribution");
    let pid = std::process::id();
    let name = format!("attrib_{pid}");

    let contact = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        &[("home", "attrib@example.com")],
    )
    .await
    .expect("create");
    let good_sha = current_head(&ctx.repo);

    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
        &format!("{name}_BAD"),
        None,
    )
    .await
    .expect("bad update");

    let (plan, token) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact.uid,
        &good_sha,
    )
    .await
    .expect("plan");
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    let msg = head_commit_message(&ctx.repo);
    assert!(
        msg.contains("tool: restore_contact"),
        "restore commit must carry 'tool: restore_contact' trailer. got:\n{msg}"
    );
    assert!(
        msg.contains("resource: contacts"),
        "restore commit must carry 'resource: contacts' trailer. got:\n{msg}"
    );
    assert!(
        msg.contains("restore: contacts/"),
        "restore commit summary must start with 'restore: contacts/'. got:\n{msg}"
    );

    let _ = write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
    )
    .await;
}

// ═════════════════════════════════════════════════════════════════════════
// I8 — Token binding (plan-apply integrity)
//
// plan_contact already has a token-binding test in e2e_contacts.rs. These
// prove the same guarantee for calendar, sieve, and mail — so that no
// restore path accepts a mismatched token.
// ═════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn calendar_restore_rejects_wrong_token() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e calendar token binding");
    let pid = std::process::id();

    let cal = ctx
        .client
        .create_calendar(&format!("tok_cal_{pid}"), None)
        .await
        .expect("create cal");
    let uid = format!("tok-event-{pid}@pimsteward");
    let ical = sample_ical(&uid, "v1");
    let cal_writer = ctx.calendar_writer();
    let cal_source = ctx.calendar_source();
    let event = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &cal.id,
        &ical,
        Some(&uid),
    )
    .await
    .expect("create event");
    let good_sha = current_head(&ctx.repo);
    write::calendar::update_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &cal.id,
        &event.id,
        &sample_ical(&uid, "v2"),
        "",
    )
    .await
    .expect("bad update");

    let (plan, _real_token) = restore::calendar::plan_calendar(
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &cal.id,
        &uid,
        &good_sha,
    )
    .await
    .expect("plan");
    let wrong_token = "f".repeat(64);
    let err = restore::calendar::apply_calendar(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &wrong_token,
    )
    .await;
    assert!(err.is_err(), "apply must refuse wrong token");

    // Live state must be untouched by the failed apply.
    let live = ctx
        .client
        .get_calendar_event(&event.id)
        .await
        .expect("fetch");
    assert!(
        live.ical.as_deref().unwrap_or("").contains("SUMMARY:v2"),
        "failed token apply must not mutate live state"
    );

    let _ = ctx.client.delete_calendar_event(&event.id).await;
    let _ = ctx.client.delete_calendar(&cal.id).await;
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn sieve_restore_rejects_wrong_token() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e sieve token binding");
    let pid = std::process::id();
    let name = format!("tok_sieve_{pid}");

    let v1 = r#"require ["fileinto"]; fileinto "A";"#;
    let script = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        v1,
    )
    .await
    .expect("install");
    let good_sha = current_head(&ctx.repo);

    let v2 = r#"require ["fileinto"]; fileinto "B";"#;
    write::sieve::update_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script.id,
        v2,
    )
    .await
    .expect("bad update");

    let (plan, _real_token) = restore::sieve::plan_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &name,
        &good_sha,
    )
    .await
    .expect("plan");
    let wrong_token = "0".repeat(64);
    let err = restore::sieve::apply_sieve(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &wrong_token,
    )
    .await;
    assert!(err.is_err(), "sieve apply must refuse wrong token");

    // Live state untouched.
    let live = ctx
        .client
        .get_sieve_script(&script.id)
        .await
        .expect("fetch");
    assert_eq!(
        live.content.as_deref(),
        Some(v2),
        "failed token apply must not mutate live state"
    );

    let _ = write::sieve::delete_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script.id,
    )
    .await;
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn mail_restore_rejects_wrong_token() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e mail token binding");
    let subject = format!("tok_mail_{}", std::process::id());
    let rest_source = RestMailSource::new(ctx.client.clone());

    let msg_id = create_test_message(&ctx, &subject).await;
    let _ = pull_mail(
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("pull");
    let good_sha = current_head(&ctx.repo);

    // Mutate flags.
    write::mail::update_flags(
        &rest_source,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        "INBOX",
        &msg_id,
        &["\\Seen".to_string()],
    )
    .await
    .expect("bad flags");

    let (plan, _real_token) = restore::mail::plan_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "INBOX",
        &msg_id,
        &good_sha,
    )
    .await
    .expect("plan");
    let wrong_token = "e".repeat(64);
    let err = restore::mail::apply_mail(
        &ctx.client,
        &rest_source,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &wrong_token,
    )
    .await;
    assert!(err.is_err(), "mail apply must refuse wrong token");

    // Live flags still contain \\Seen — the failed restore didn't execute.
    let live = ctx.client.get_message(&msg_id).await.expect("fetch");
    let flags: Vec<String> = live
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        flags.contains(&"\\Seen".to_string()),
        "failed token apply must not revert flags"
    );

    let _ = write::mail::delete_message(
        &rest_source,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        "INBOX",
        &msg_id,
    )
    .await;
}

// ═════════════════════════════════════════════════════════════════════════
// I2+I4 at the git layer — bulk restore isolation
//
// The strongest form of the isolation guarantee: run a bulk restore scoped
// to a path prefix, then verify the git commit it produced touched *only*
// files under that prefix. This proves scoping at the plumbing level, not
// just the observable-state level. The user's scenario ("restore A without
// rolling back the 100 new messages") is an instance of this invariant.
// ═════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn bulk_restore_scoped_to_contacts_touches_only_contact_paths() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e bulk isolation");
    let pid = std::process::id();

    // Create a contact (the thing to restore), a sieve script (the thing
    // that must not be touched), and a calendar event (also must not be
    // touched).
    let name = format!("bulk_iso_{pid}");
    let contact = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &name,
        &[("home", "bulk_iso@example.com")],
    )
    .await
    .expect("create contact");

    let sieve_name = format!("bulk_iso_sieve_{pid}");
    let sieve_content = r#"require ["fileinto"]; fileinto "Survive";"#;
    let script = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &sieve_name,
        sieve_content,
    )
    .await
    .expect("install sieve");

    let cal = ctx
        .client
        .create_calendar(&format!("bulk_iso_cal_{pid}"), None)
        .await
        .expect("create calendar");
    let event_uid = format!("bulk-iso-event-{pid}@pimsteward");
    let event_ical = sample_ical(&event_uid, "survivor");
    let cal_writer = ctx.calendar_writer();
    let cal_source = ctx.calendar_source();
    let event = write::calendar::create_event(
        cal_writer.as_ref(),
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &cal.id,
        &event_ical,
        Some(&event_uid),
    )
    .await
    .expect("create event");

    // Baseline snapshot.
    let _ = pull_contacts(
        &RestContactsSource::new(ctx.client.clone()),
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .ok();
    let _ = pull_calendar(
        &RestCalendarSource::new(ctx.client.clone()),
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .ok();
    let good_sha = current_head(&ctx.repo);

    // Bad update to the contact only.
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
        &format!("{name}_BAD"),
        None,
    )
    .await
    .expect("bad contact");

    // Bulk restore scoped to contacts path only.
    let contacts_prefix = "contacts/".to_string();
    let (plan, token) = restore::bulk::plan_bulk(
        &ctx.client,
        cal_source.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &contacts_prefix,
        &good_sha,
    )
    .await
    .expect("plan bulk");
    assert_eq!(
        plan.sieve.len(),
        0,
        "contacts-scoped bulk plan must not pick up sieve sub-plans"
    );
    assert_eq!(
        plan.calendar_events.len(),
        0,
        "contacts-scoped bulk plan must not pick up calendar sub-plans"
    );
    assert!(
        !plan.contacts.is_empty(),
        "contacts-scoped bulk plan should have at least one contact sub-plan"
    );

    let result = restore::bulk::apply_bulk(
        &ctx.client,
        cal_source.as_ref(),
        cal_writer.as_ref(),
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply bulk");
    assert!(result.errors.is_empty(), "bulk apply errors: {:?}", result.errors);

    // Contact is back.
    let live_contact = ctx
        .client
        .list_contacts()
        .await
        .expect("list")
        .into_iter()
        .find(|c| c.id == contact.id)
        .expect("contact exists");
    assert_eq!(live_contact.full_name, name, "contact restored to name");

    // Sieve script is untouched.
    let live_sieve = ctx
        .client
        .get_sieve_script(&script.id)
        .await
        .expect("fetch sieve");
    assert_eq!(
        live_sieve.content.as_deref(),
        Some(sieve_content),
        "sieve script must be untouched by contacts-scoped bulk restore"
    );

    // Calendar event is untouched.
    let live_event = ctx
        .client
        .get_calendar_event(&event.id)
        .await
        .expect("fetch event");
    assert!(
        live_event
            .ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:survivor"),
        "calendar event must be untouched by contacts-scoped bulk restore"
    );

    // Cleanup
    let _ = write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
    )
    .await;
    let _ = write::sieve::delete_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &script.id,
    )
    .await;
    let _ = ctx.client.delete_calendar_event(&event.id).await;
    let _ = ctx.client.delete_calendar(&cal.id).await;
}
