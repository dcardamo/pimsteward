//! e2e tests for mail mutations: flag update, folder move, delete, restore.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::pull::mail::pull_mail;
use pimsteward::restore;
use pimsteward::write;
use serde_json::json;

/// Upload a test message via forwardemail's POST /v1/messages (IMAP APPEND
/// equivalent). Returns the forwardemail message id. Uses curl because the
/// Client doesn't expose a raw append method yet — the test is exercising
/// mutation paths, not the append path itself.
async fn create_test_message(ctx: &E2eContext, subject: &str) -> String {
    let raw = format!(
        "From: e2e_test@example.com\r\nTo: {}\r\nSubject: {subject}\r\n\
         Message-ID: <e2e-{}-{}@example.com>\r\n\
         Date: Sun, 05 Apr 2026 08:00:00 +0000\r\n\r\nBody for {subject}.",
        ctx.alias,
        std::process::id(),
        subject
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

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn mail_flag_update_and_restore() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e mail flags");
    let subject = format!("e2e_test_{}", std::process::id());

    let msg_id = create_test_message(&ctx, &subject).await;

    // Initial pull captures the fresh message (flags = [])
    let _ = pull_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("initial pull");

    let good_sha = current_head(&ctx.repo);

    // Set flags via write path
    let flags = vec!["\\Seen".to_string(), "\\Flagged".to_string()];
    write::mail::update_flags(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &msg_id,
        &flags,
    )
    .await
    .expect("set flags");

    // Verify live
    let live = ctx.client.get_message(&msg_id).await.expect("fetch live");
    let live_flags: Vec<String> = live
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(live_flags.contains(&"\\Seen".to_string()));
    assert!(live_flags.contains(&"\\Flagged".to_string()));

    // Restore dry-run back to good_sha (flags=[])
    let (plan, token) = restore::mail::plan_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "INBOX",
        &msg_id,
        &good_sha,
    )
    .await
    .expect("plan");
    assert!(matches!(
        plan.operation,
        restore::mail::MailOperation::RestoreFlags { .. }
    ));

    restore::mail::apply_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    // Verify flags cleared
    let live2 = ctx
        .client
        .get_message(&msg_id)
        .await
        .expect("fetch after restore");
    let live_flags2: Vec<String> = live2
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        live_flags2.is_empty(),
        "flags should be reset to historical empty set, got {live_flags2:?}"
    );

    // Cleanup
    write::mail::delete_message(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &msg_id)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn mail_move_back_to_original_folder() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e mail move");
    let subject = format!("e2e_move_{}", std::process::id());
    let msg_id = create_test_message(&ctx, &subject).await;

    let _ = pull_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e",
        "e2e@pimsteward.local",
    )
    .await
    .expect("baseline");
    let good_sha = current_head(&ctx.repo);

    // Move to Archive
    write::mail::move_message(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &msg_id,
        "Archive",
    )
    .await
    .expect("move");

    // Restore back to INBOX
    let (plan, token) = restore::mail::plan_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        "INBOX",
        &msg_id,
        &good_sha,
    )
    .await
    .expect("plan");
    assert!(matches!(
        plan.operation,
        restore::mail::MailOperation::MoveBack { .. }
    ));

    restore::mail::apply_mail(
        &ctx.client,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &plan,
        &token,
    )
    .await
    .expect("apply");

    let live = ctx.client.get_message(&msg_id).await.expect("fetch");
    assert_eq!(
        live.get("folder_path").and_then(|v| v.as_str()),
        Some("INBOX"),
        "message should be back in INBOX after restore"
    );

    write::mail::delete_message(&ctx.client, &ctx.repo, &ctx.alias_slug(), &attr, &msg_id)
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
