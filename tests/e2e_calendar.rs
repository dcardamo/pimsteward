//! e2e tests for calendar events: create/update/delete + restore.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::calendar::pull_calendar;
use pimsteward::restore;
use pimsteward::write;

fn sample_ical(uid: &str, summary: &str) -> String {
    [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//pimsteward//e2e//EN",
        "BEGIN:VEVENT",
        &format!("UID:{uid}"),
        "DTSTAMP:20260101T000000Z",
        "DTSTART:20270115T100000Z",
        "DTEND:20270115T110000Z",
        &format!("SUMMARY:{summary}"),
        "END:VEVENT",
        "END:VCALENDAR",
    ]
    .join("\r\n")
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn calendar_event_create_update_delete_lifecycle() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e calendar lifecycle");
    let pid = std::process::id();

    // Create a dedicated calendar for the test so we don't collide with
    // anything else.
    let cal = ctx
        .client
        .create_calendar(&format!("e2e_test_{pid}"), None)
        .await
        .expect("create calendar");
    let calendar_id = cal.id.clone();

    // Baseline pull
    let _ = pull_calendar(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("baseline");

    // Create event
    let uid = format!("e2e-event-{pid}@pimsteward");
    let ical_v1 = sample_ical(&uid, "original summary");
    let created = write::calendar::create_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &ical_v1,
        Some(&uid),
    )
    .await
    .expect("create event");
    assert_eq!(created.uid.as_deref(), Some(uid.as_str()));
    assert_eq!(created.summary.as_deref(), Some("original summary"));
    let event_id = created.id.clone();

    // .ics in repo
    let ics_path = format!(
        "sources/forwardemail/{}/calendars/{}/events/{}.ics",
        ctx.alias_slug(),
        calendar_id,
        uid
    );
    let ics =
        String::from_utf8_lossy(&ctx.repo.read_file(&ics_path).expect("ics in repo")).into_owned();
    assert!(ics.contains("SUMMARY:original summary"));

    // Update
    let ical_v2 = sample_ical(&uid, "updated summary");
    write::calendar::update_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &event_id,
        Some(&ical_v2),
        None,
    )
    .await
    .expect("update event");
    let ics2 = String::from_utf8_lossy(&ctx.repo.read_file(&ics_path).expect("post-update ics"))
        .into_owned();
    assert!(ics2.contains("SUMMARY:updated summary"));

    // Delete
    write::calendar::delete_event(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &event_id)
        .await
        .expect("delete event");
    assert!(ctx.repo.read_file(&ics_path).is_err());

    // Cleanup calendar
    ctx.client
        .delete_calendar(&calendar_id)
        .await
        .expect("delete calendar");
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn calendar_event_restore_from_history() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e calendar restore");
    let pid = std::process::id();

    let cal = ctx
        .client
        .create_calendar(&format!("e2e_restore_{pid}"), None)
        .await
        .expect("create calendar");
    let calendar_id = cal.id.clone();

    let uid = format!("restore-event-{pid}@pimsteward");
    let good_ical = sample_ical(&uid, "good summary");
    let created = write::calendar::create_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &good_ical,
        Some(&uid),
    )
    .await
    .expect("create event");
    let event_id = created.id.clone();

    let good_sha = current_head(&ctx.repo);

    // Bad update
    let bad_ical = sample_ical(&uid, "BAD summary");
    write::calendar::update_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &event_id,
        Some(&bad_ical),
        None,
    )
    .await
    .expect("bad update");

    // Restore dry-run + apply
    let (plan, token) = restore::calendar::plan_calendar(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &calendar_id,
        &uid,
        &good_sha,
    )
    .await
    .expect("plan");
    assert!(matches!(
        plan.operation,
        restore::calendar::CalendarOperation::UpdateIcal { .. }
    ));

    restore::calendar::apply_calendar(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    // Verify
    let live = ctx
        .client
        .get_calendar_event(&event_id)
        .await
        .expect("fetch");
    assert!(
        live.ical
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY:good summary"),
        "live ical should be restored to good summary, got: {:?}",
        live.ical
    );

    // Cleanup
    let _ = ctx.client.delete_calendar_event(&event_id).await;
    let _ = ctx.client.delete_calendar(&calendar_id).await;
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn calendar_event_restore_recreate_after_delete() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e calendar recreate");
    let pid = std::process::id();

    let cal = ctx
        .client
        .create_calendar(&format!("e2e_recreate_{pid}"), None)
        .await
        .expect("create calendar");
    let calendar_id = cal.id.clone();

    let uid = format!("recreate-event-{pid}@pimsteward");
    let ical = sample_ical(&uid, "to be recreated");
    let created = write::calendar::create_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &ical,
        Some(&uid),
    )
    .await
    .expect("create");
    let event_id = created.id.clone();
    let good_sha = current_head(&ctx.repo);

    // Delete the event
    write::calendar::delete_event(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &event_id)
        .await
        .expect("delete");

    // Restore should compute a Recreate operation
    let (plan, token) = restore::calendar::plan_calendar(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &calendar_id,
        &uid,
        &good_sha,
    )
    .await
    .expect("plan");
    assert!(matches!(
        plan.operation,
        restore::calendar::CalendarOperation::Recreate { .. }
    ));

    restore::calendar::apply_calendar(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply recreate");

    // Verify the event exists again (with the same UID)
    let live_events = ctx
        .client
        .list_calendar_events(Some(&calendar_id))
        .await
        .expect("list after restore");
    let recreated = live_events.iter().find(|e| e.uid.as_deref() == Some(&uid));
    assert!(
        recreated.is_some(),
        "event with uid={uid} should exist after recreate restore"
    );
    assert!(recreated
        .and_then(|e| e.ical.as_deref())
        .unwrap_or("")
        .contains("SUMMARY:to be recreated"));

    // Cleanup
    if let Some(r) = recreated {
        let _ = ctx.client.delete_calendar_event(&r.id).await;
    }
    let _ = ctx.client.delete_calendar(&calendar_id).await;
}

fn current_head(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
