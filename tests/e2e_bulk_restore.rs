//! e2e test for bulk restore: create multiple resources of different types,
//! mutate them all, then bulk-restore and verify every one came back.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::{calendar::pull_calendar, contacts::pull_contacts, sieve::pull_sieve};
use pimsteward::restore;
use pimsteward::source::{RestCalendarSource, RestContactsSource};
use pimsteward::write;

fn sample_ical(uid: &str, summary: &str) -> String {
    [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//pimsteward//e2e//EN",
        "BEGIN:VEVENT",
        &format!("UID:{uid}"),
        "DTSTAMP:20260101T000000Z",
        "DTSTART:20270201T090000Z",
        "DTEND:20270201T100000Z",
        &format!("SUMMARY:{summary}"),
        "END:VEVENT",
        "END:VCALENDAR",
    ]
    .join("\r\n")
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn bulk_restore_contacts_sieve_and_calendar() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e bulk restore");
    let pid = std::process::id();

    // 1. Create one resource of each type
    let contact_name = format!("bulk_e2e_{pid}");
    let contact = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_name,
        &[("home", "bulk_test@example.com")],
    )
    .await
    .expect("create contact");

    let sieve_name = format!("bulk_sieve_{pid}");
    let good_sieve = r#"require ["fileinto"]; fileinto "Archive";"#;
    let sieve_script = write::sieve::install_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &sieve_name,
        good_sieve,
    )
    .await
    .expect("install sieve");

    let cal = ctx
        .client
        .create_calendar(&format!("bulk_cal_{pid}"), None)
        .await
        .expect("create cal");
    let calendar_id = cal.id.clone();
    let event_uid = format!("bulk_event_{pid}@pimsteward");
    let good_ical = sample_ical(&event_uid, "bulk original");
    let event = write::calendar::create_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &calendar_id,
        &good_ical,
        Some(&event_uid),
    )
    .await
    .expect("create event");

    // 2. Baseline pull to ensure everything is captured, then snapshot sha
    let _ = pull_contacts(
        &RestContactsSource::new(ctx.client.clone()),
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .ok();
    let _ = pull_sieve(
        &ctx.client,
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

    // 3. Mutate all three
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact.id,
        &format!("{contact_name}_BAD"),
        None,
    )
    .await
    .expect("bad contact update");

    let bad_sieve = r#"require ["fileinto"]; fileinto "Junk";"#;
    write::sieve::update_sieve_script(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &sieve_script.id,
        bad_sieve,
    )
    .await
    .expect("bad sieve update");

    let bad_ical = sample_ical(&event_uid, "bulk BAD");
    write::calendar::update_event(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &event.id,
        Some(&bad_ical),
        None,
        None,
    )
    .await
    .expect("bad event update");

    // 4. Bulk restore across the whole alias subtree
    let path_prefix = "".to_string();
    let (plan, token) = restore::bulk::plan_bulk(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &path_prefix,
        &good_sha,
    )
    .await
    .expect("bulk plan");

    println!(
        "bulk plan: {} contacts, {} sieve, {} events (total {} ops)",
        plan.contacts.len(),
        plan.sieve.len(),
        plan.calendar_events.len(),
        plan.total_ops()
    );
    assert!(
        plan.total_ops() >= 3,
        "plan should include our 3 mutated resources"
    );

    // Verify plan_token mismatch rejects
    let wrong_token = "f".repeat(64);
    let err = restore::bulk::apply_bulk(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &wrong_token,
    )
    .await;
    assert!(err.is_err(), "bulk apply must refuse mismatched token");

    // Apply with correct token
    let result = restore::bulk::apply_bulk(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("bulk apply");
    println!(
        "bulk result: contacts_ok={}, sieve_ok={}, calendar_ok={}, errors={:?}",
        result.contacts_ok, result.sieve_ok, result.calendar_ok, result.errors
    );
    assert!(
        result.errors.is_empty(),
        "bulk restore should have no errors, got: {:?}",
        result.errors
    );

    // 5. Verify each resource is back to its good state
    let live_contact = ctx
        .client
        .list_contacts()
        .await
        .expect("list contacts")
        .into_iter()
        .find(|c| c.id == contact.id)
        .expect("contact still exists");
    assert_eq!(live_contact.full_name, contact_name);

    let live_sieve = ctx
        .client
        .get_sieve_script(&sieve_script.id)
        .await
        .expect("fetch sieve");
    assert_eq!(live_sieve.content.as_deref(), Some(good_sieve));

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
            .contains("SUMMARY:bulk original"),
        "event should be restored to original summary"
    );

    // 6. Cleanup
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
        &sieve_script.id,
    )
    .await;
    let _ = ctx.client.delete_calendar_event(&event.id).await;
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
