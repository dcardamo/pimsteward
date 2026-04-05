//! e2e tests for the IMAP read path against the real forwardemail
//! IMAP server (`imap.forwardemail.net:993`).
//!
//! Read-only: verifies list_folders, list_messages, and fetch_message
//! return valid data. Does not mutate the mailbox.

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::source::imap::{ImapConfig, ImapMailSource};
use pimsteward::source::MailSource;

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
    // Seed a message so INBOX isn't empty.
    let msg_id = seed_test_message(&ctx).await;

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

    // Cleanup
    ctx.client.delete_message(&msg_id).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn imap_fetch_message_returns_rfc822() {
    let ctx = E2eContext::from_env();
    let source = imap_source(&ctx);
    let msg_id = seed_test_message(&ctx).await;

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

    // Cleanup
    ctx.client.delete_message(&msg_id).await.expect("cleanup");
}
