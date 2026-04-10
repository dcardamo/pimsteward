//! e2e tests for the IMAP read path against the real forwardemail
//! IMAP server (`imap.forwardemail.net:993`).
//!
//! Verifies list_folders, list_messages, fetch_message, and IMAP IDLE
//! push notifications against the real server.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::source::imap::{idle_loop, ImapConfig, ImapMailSource};
use pimsteward::source::MailSource;
use std::sync::Arc;
use tokio::sync::Notify;

/// Guard that deletes a message on drop — prevents test message leaks
/// when assertions fail before explicit cleanup.
struct MessageCleanup<'a> {
    client: &'a pimsteward::forwardemail::Client,
    id: Option<String>,
}

impl<'a> MessageCleanup<'a> {
    fn new(client: &'a pimsteward::forwardemail::Client) -> Self {
        Self { client, id: None }
    }
    fn set(&mut self, id: String) {
        self.id = Some(id);
    }
}

impl Drop for MessageCleanup<'_> {
    fn drop(&mut self) {
        if let Some(ref id) = self.id {
            // Best-effort cleanup — can't async in Drop, so spawn a
            // blocking task. The test alias tolerates leaked messages
            // but this keeps it tidy.
            let client = self.client.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                let _ = rt.block_on(client.delete_message(&id));
            });
        }
    }
}

/// Seed a test message via REST so the IMAP tests have something to read.
/// Returns the forwardemail message id (for cleanup).
async fn seed_test_message(ctx: &E2eContext) -> String {
    let subject = format!("imap_e2e_{}", std::process::id());
    let msg = pimsteward::forwardemail::writes::NewMessage {
        folder: "INBOX".to_string(),
        to: vec![ctx.alias.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        text: Some("IMAP e2e test body".to_string()),
        html: None,
        in_reply_to: None,
        references: vec![],
    };
    let result = ctx.client.create_message(&msg).await.expect("seed msg");
    result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("msg id")
        .to_string()
}

fn imap_source(ctx: &E2eContext) -> ImapMailSource {
    // Extract user/pass from the client's already-validated credentials.
    // E2eContext reads from the same credential files.
    let user = ctx.alias.clone();
    let pass = std::fs::read_to_string(
        std::env::var("PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE")
            .unwrap_or_else(|_| "/home/dan/.config/secrets/pimsteward-test-alias-password".into()),
    )
    .expect("reading password file")
    .trim()
    .to_string();

    ImapMailSource::new(ImapConfig::forwardemail(user, pass))
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_list_folders_returns_inbox() {
    let ctx = E2eContext::from_env();
    let source = imap_source(&ctx);

    let folders = source.list_folders().await.expect("list_folders");
    assert!(
        folders.iter().any(|f| f.path == "INBOX"),
        "IMAP folder list should include INBOX, got: {:?}",
        folders.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_list_messages_returns_summaries() {
    let ctx = E2eContext::from_env();
    let source = imap_source(&ctx);
    let mut cleanup = MessageCleanup::new(&ctx.client);
    let msg_id = seed_test_message(&ctx).await;
    cleanup.set(msg_id.clone());

    let result = source
        .list_messages("INBOX", None, None)
        .await
        .expect("list_messages");
    assert!(
        !result.all_ids.is_empty(),
        "INBOX should have at least one message"
    );
    // changed == all_ids when no CHANGEDSINCE hint is provided.
    assert_eq!(
        result.changed.len(),
        result.all_ids.len(),
        "without CHANGEDSINCE hint, changed should equal all_ids"
    );
    for msg in &result.changed {
        assert!(msg.uid.is_some(), "IMAP message should have a UID");
        assert!(
            msg.id.starts_with("imap-"),
            "IMAP id should start with imap-"
        );
    }
    // cleanup guard handles message deletion on drop
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_fetch_message_returns_rfc822() {
    let ctx = E2eContext::from_env();
    let source = imap_source(&ctx);
    let mut cleanup = MessageCleanup::new(&ctx.client);
    let msg_id = seed_test_message(&ctx).await;
    cleanup.set(msg_id.clone());

    let result = source
        .list_messages("INBOX", None, None)
        .await
        .expect("list_messages");
    assert!(
        !result.changed.is_empty(),
        "need at least one message to fetch"
    );

    let first = &result.changed[0];
    let fetched = source
        .fetch_message("INBOX", &first.id)
        .await
        .expect("fetch_message");
    assert!(
        !fetched.raw.is_empty(),
        "raw RFC822 bytes should not be empty"
    );
    // Should look like RFC822 — check for common headers. forwardemail
    // may use different casing (From, FROM, from) or the message may
    // start with a Return-Path or Received header.
    let text = String::from_utf8_lossy(&fetched.raw);
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("from:") || lower.contains("subject:") || lower.contains("date:"),
        "raw bytes should contain RFC822 headers, got: {}",
        &text[..text.len().min(200)]
    );
    assert!(fetched.extra.is_none(), "IMAP extra should be None");
    // cleanup guard handles message deletion on drop
}

/// IMAP IDLE push notification: start an IDLE listener on INBOX, wait
/// for the ready signal (IDLE established), create a message via REST,
/// and verify the Notify fires. Exercises the full idle_loop → Notify →
/// wake path that the daemon uses for sub-minute mail sync.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_idle_fires_on_new_message() {
    let ctx = E2eContext::from_env();
    let pass = std::fs::read_to_string(
        std::env::var("PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE")
            .unwrap_or_else(|_| "/home/dan/.config/secrets/pimsteward-test-alias-password".into()),
    )
    .expect("reading password file")
    .trim()
    .to_string();

    let idle_cfg = ImapConfig::forwardemail(ctx.alias.clone(), pass);
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();
    let ready = Arc::new(Notify::new());
    let ready_clone = ready.clone();

    // Cleanup guard — deletes the test message even if assertions panic.
    let mut cleanup = MessageCleanup::new(&ctx.client);

    // Spawn the IDLE listener with a ready signal.
    let idle_handle = tokio::spawn(async move {
        idle_loop(
            idle_cfg,
            "INBOX".to_string(),
            notify_clone,
            Some(ready_clone),
        )
        .await;
    });

    // Wait for IDLE to be fully established — no fragile sleep.
    let ready_result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        ready.notified(),
    )
    .await;
    assert!(
        ready_result.is_ok(),
        "IMAP IDLE should be established within 15s"
    );

    // Create a message via REST — triggers an IMAP EXISTS notification.
    let subject = format!("idle_e2e_{}", std::process::id());
    let msg = pimsteward::forwardemail::writes::NewMessage {
        folder: "INBOX".to_string(),
        to: vec![ctx.alias.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject,
        text: Some("IDLE test body".to_string()),
        html: None,
        in_reply_to: None,
        references: vec![],
    };
    let result = ctx.client.create_message(&msg).await.expect("create msg");
    let msg_id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("msg id")
        .to_string();
    cleanup.set(msg_id);

    // Wait for the Notify — 30s timeout is generous; real latency is <2s.
    let wake = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        notify.notified(),
    )
    .await;
    assert!(
        wake.is_ok(),
        "IMAP IDLE should have fired Notify within 30s of new message"
    );

    idle_handle.abort();
}

/// Verify that idle_loop recovers from a connection error by retrying.
/// Uses an invalid password to force an immediate login failure, then
/// checks that the loop doesn't panic — it logs and backs off. We can't
/// e2e test a mid-IDLE network drop without infrastructure to kill TCP
/// connections, but this exercises the error → backoff → retry path.
#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_idle_reconnects_after_auth_failure() {
    let _ctx = E2eContext::from_env(); // just for the safety guard
    let bad_cfg = ImapConfig::forwardemail(
        "nonexistent_test@example.com".to_string(),
        "wrong_password".to_string(),
    );
    let notify = Arc::new(Notify::new());
    let notify_clone = notify.clone();

    let handle = tokio::spawn(async move {
        idle_loop(
            bad_cfg,
            "INBOX".to_string(),
            notify_clone,
            None,
        )
        .await;
    });

    // Let it attempt a few reconnects with backoff (1s, 2s, 4s = ~7s).
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    // The loop should still be running (not panicked). Abort it.
    assert!(!handle.is_finished(), "idle_loop should survive auth failures and keep retrying");
    handle.abort();
}
