//! e2e tests for contacts: full lifecycle + restore against the real
//! forwardemail API.
//!
//! Gated on `PIMSTEWARD_RUN_E2E=1` (via `common::E2eContext::from_env`).
//! The safety guard hard-fails unless the alias contains `_test`.
//!
//! Run with:
//!     PIMSTEWARD_RUN_E2E=1 cargo nextest run --run-ignored only --test e2e_contacts

#![allow(clippy::bool_assert_comparison)]

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::contacts::pull_contacts;
use pimsteward::restore;
use pimsteward::source::RestContactsSource;
use pimsteward::write;

/// Full lifecycle: create → pull → update → pull → delete → pull → verify
/// each step landed in both forwardemail and the git repo.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_create_update_delete_lifecycle() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e contact lifecycle");

    // 1. Initial pull so we have a baseline (captures any pre-existing
    // contacts on the test alias). Use unique test names to avoid
    // collisions with leftover state.
    let _ = pull_contacts(
        &RestContactsSource::new(ctx.client.clone()),
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("baseline pull");

    // 2. Create a unique test contact via the write path.
    let marker = format!("e2e-test-{}", std::process::id());
    let emails = [("home", "e2e_test@example.com")];
    let created = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &marker,
        &emails,
    )
    .await
    .expect("create contact");
    assert_eq!(created.full_name, marker);
    let contact_id = created.id.clone();
    let contact_uid = created.uid.clone();

    // 3. Verify the .vcf is in the git tree
    let vcf_path = format!(
        "sources/forwardemail/{}/contacts/default/{}.vcf",
        ctx.alias_slug(),
        contact_uid
    );
    let vcf_bytes = ctx.repo.read_file(&vcf_path).expect("vcf in repo");
    let vcf = String::from_utf8_lossy(&vcf_bytes);
    assert!(
        vcf.contains(&marker),
        "vcard should contain the marker name"
    );

    // 4. Update via write path
    let updated_name = format!("{marker}-updated");
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
        &updated_name,
        None,
    )
    .await
    .expect("update contact");

    let vcf2 = String::from_utf8_lossy(&ctx.repo.read_file(&vcf_path).expect("post-update vcf"))
        .into_owned();
    assert!(
        vcf2.contains(&updated_name),
        "vcard should reflect the updated name after write + auto-pull"
    );

    // 5. Delete
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
    )
    .await
    .expect("delete contact");

    assert!(
        ctx.repo.read_file(&vcf_path).is_err(),
        "vcf should be gone from the repo after delete"
    );
}

/// Restore flow: create → update → restore to pre-update SHA → verify the
/// contact is back to its original name.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_restore_undoes_a_bad_rename() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e contact restore");

    let marker = format!("e2e-restore-{}", std::process::id());

    // Create
    let created = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &marker,
        &[("home", "restore_test@example.com")],
    )
    .await
    .expect("create");
    let contact_id = created.id.clone();
    let contact_uid = created.uid.clone();

    // Snapshot sha BEFORE the bad update — this is the restore target
    let good_sha = current_head(&ctx.repo);

    // Simulate a bad write
    let bad_name = format!("{marker}-BAD");
    write::contacts::update_contact_name(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
        &bad_name,
        None,
    )
    .await
    .expect("bad update");

    // Dry-run restore
    let (plan, token) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact_uid,
        &good_sha,
    )
    .await
    .expect("plan restore");
    assert!(
        matches!(
            plan.operation,
            restore::contacts::RestoreOperation::Update { .. }
        ),
        "expected Update op, got {:?}",
        plan.operation
    );

    // Wrong token should be refused
    let wrong_token = "0".repeat(64);
    let err = restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &wrong_token,
    )
    .await;
    assert!(
        err.is_err(),
        "apply must refuse a mismatched plan_token (got {err:?})"
    );

    // Correct token succeeds
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("restore apply");

    // Verify the live contact's full_name is the original marker
    let live = ctx
        .client
        .list_contacts()
        .await
        .expect("list contacts after restore");
    let found = live.iter().find(|c| c.id == contact_id);
    assert!(found.is_some(), "contact should still exist after restore");
    assert_eq!(found.unwrap().full_name, marker);

    // Cleanup
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
    )
    .await
    .expect("cleanup");
}

/// Restore recreate: create → snapshot → delete → restore from snapshot →
/// verify the contact exists again with all vCard fields intact.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_restore_recreate_after_delete() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e contact recreate");

    let marker = format!("e2e-recreate-{}", std::process::id());
    let created = write::contacts::create_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &marker,
        &[("home", "recreate_test@example.com")],
    )
    .await
    .expect("create");
    let contact_id = created.id.clone();
    let contact_uid = created.uid.clone();

    let good_sha = current_head(&ctx.repo);

    // Delete
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
    )
    .await
    .expect("delete");

    // Plan restore — should be Recreate
    let (plan, token) = restore::contacts::plan_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &contact_uid,
        &good_sha,
    )
    .await
    .expect("plan");
    assert!(
        matches!(
            plan.operation,
            restore::contacts::RestoreOperation::Recreate { .. }
        ),
        "expected Recreate op, got {:?}",
        plan.operation
    );

    // Apply
    restore::contacts::apply_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply recreate");

    // Verify the contact exists live with the original name
    let live = ctx
        .client
        .list_contacts()
        .await
        .expect("list after recreate");
    let found = live.iter().find(|c| c.full_name == marker);
    assert!(
        found.is_some(),
        "contact should exist after recreate restore"
    );
    // Verify the vCard content was preserved (not a placeholder)
    assert!(
        found.unwrap().content.contains(&marker),
        "recreated vCard should contain the original full_name"
    );

    // Cleanup — new id after recreate
    let new_id = &found.unwrap().id;
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        new_id,
    )
    .await
    .expect("cleanup");
}

/// vCard round-trip: create from raw vCard, verify all fields, update
/// via raw vCard, verify the update.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn contact_vcard_round_trip() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e vcard round-trip");

    let marker = format!("e2e-vcard-{}", std::process::id());
    let vcard = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{marker}\r\n\
         EMAIL;TYPE=home:vcard_test@example.com\r\n\
         TEL;TYPE=cell:+15551234567\r\n\
         ORG:Test Corp\r\n\
         NOTE:e2e test contact\r\nEND:VCARD"
    );

    let created = ctx
        .client
        .create_contact_from_vcard(&vcard)
        .await
        .expect("create from vcard");
    assert_eq!(created.full_name, marker);
    let contact_id = created.id.clone();

    // Verify the live vCard has all the fields we sent
    let live = ctx.client.list_contacts().await.expect("list");
    let found = live.iter().find(|c| c.id == contact_id).expect("found");
    assert!(found.content.contains("TEL"), "vCard should have phone");
    assert!(found.content.contains("Test Corp"), "vCard should have org");

    // Update via vCard — change the phone number
    let updated_vcard = format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:{marker}\r\n\
         EMAIL;TYPE=home:vcard_test@example.com\r\n\
         TEL;TYPE=cell:+15559999999\r\n\
         ORG:Test Corp\r\n\
         NOTE:updated by e2e\r\nEND:VCARD"
    );
    ctx.client
        .update_contact_vcard(&contact_id, &updated_vcard, &marker, None)
        .await
        .expect("update vcard");

    let live2 = ctx.client.list_contacts().await.expect("list after update");
    let found2 = live2.iter().find(|c| c.id == contact_id).expect("found2");
    assert!(
        found2.content.contains("+15559999999"),
        "updated phone should be in vCard"
    );

    // Cleanup
    write::contacts::delete_contact(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &contact_id,
    )
    .await
    .expect("cleanup");
}

fn current_head(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
