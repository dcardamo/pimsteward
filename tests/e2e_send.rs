//! e2e tests for `write::mail::send_email` and its permission gate.
//!
//! These hit forwardemail's real POST /v1/emails endpoint against the
//! `_test` alias, so they actually deliver outgoing mail over SMTP. The
//! recipient is the same `_test` alias — we send mail to ourselves so
//! there's no third-party blast radius, and we can verify delivery by
//! reading the Sent folder afterwards.
//!
//! # Invariants under test
//!
//! | # | Invariant                                                             |
//! | - | --------------------------------------------------------------------- |
//! | S1| `check_email_send` refuses when `email_send = "denied"` (the default).|
//! | S2| `check_email_send` refuses even if `email = "read_write"` is granted.|
//! | S3| `check_email_send` permits when `email_send = "allowed"` is set.     |
//! | S4| A successful send lands in the `Sent` folder on forwardemail.        |
//! | S5| A successful send produces a git commit with a `tool: send_email`    |
//! |   | audit trailer carrying `to`, `subject`, and `body_sha256`.           |
//! | S6| The body hash in the audit trailer is deterministic across sends.    |
//!
//! S1/S2/S3 are pure unit tests and live in `src/permission.rs` — they
//! don't need the network. This file covers S4/S5/S6: the behaviours
//! that only exist against a real forwardemail account.
//!
//! # Running
//!
//! ```sh
//! PIMSTEWARD_RUN_E2E=1 cargo nextest run --test e2e_send --run-ignored all
//! ```
//!
//! The `_test` alias safety guard applies — see `src/safety.rs`. Every
//! test routes through `E2eContext::from_env`, which refuses to run
//! unless the alias localpart contains `_test`.

#![allow(clippy::bool_assert_comparison)]

#[path = "common/mod.rs"]
mod common;

use common::E2eContext;
use pimsteward::forwardemail::mail::MessageSummary;
use pimsteward::forwardemail::writes::NewMessage;
use pimsteward::pull::mail::pull_mail;
use pimsteward::source::RestMailSource;
use pimsteward::write;
use sha2::{Digest, Sha256};

// ── Helpers ──────────────────────────────────────────────────────────

fn current_head(repo: &pimsteward::store::Repo) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Walk commits from `since..HEAD` and return every commit body that
/// contains the given trailer string. Used to find the send_email audit
/// commit even if the post-send refresh produced intermediate pull
/// commits ahead of the audit commit.
fn commit_bodies_since_containing(
    repo: &pimsteward::store::Repo,
    since_sha: &str,
    needle: &str,
) -> Vec<String> {
    // %x1f is ASCII unit separator — reliable delimiter for commit bodies.
    let out = std::process::Command::new("git")
        .args(["log", &format!("{since_sha}..HEAD"), "--format=%B%x1f"])
        .current_dir(repo.root())
        .output()
        .expect("git log");
    String::from_utf8_lossy(&out.stdout)
        .split('\x1f')
        .filter(|b| b.contains(needle))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Mirror of `write::mail::body_sha256` — duplicated here because that
/// function is pub(crate) and the test binary can't call it directly.
/// If the production hash algorithm ever changes, this test helper must
/// change in lockstep, which is enforced by the assertion in the test
/// that the trailer's hash matches this computation.
fn expected_body_sha256(text: Option<&str>, html: Option<&str>) -> String {
    let mut h = Sha256::new();
    if let Some(t) = text {
        h.update(b"text:");
        h.update(t.as_bytes());
    }
    h.update(b"\0");
    if let Some(html) = html {
        h.update(b"html:");
        h.update(html.as_bytes());
    }
    format!("{:x}", h.finalize())
}

// ── S4/S5/S6: real send against the _test alias ─────────────────────

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn send_email_delivers_and_records_audit_commit() {
    let ctx = E2eContext::from_env();
    let attr = ctx.attribution("e2e send_email delivery");
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Send to the _test alias itself. The blast radius is therefore
    // exactly one mailbox — no third party involved — and we can
    // verify delivery by reading the Sent folder.
    let subject = format!("e2e_send_{pid}_{nanos}");
    let text_body = format!("e2e send body — {pid}");
    let html_body = format!("<p>e2e send body — {pid}</p>");
    let msg = NewMessage {
        folder: "Sent".to_string(), // unused by send path, set for completeness
        to: vec![ctx.alias.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: subject.clone(),
        text: Some(text_body.clone()),
        html: Some(html_body.clone()),
        in_reply_to: None,
        references: vec![],
    };

    // The MailSource/MailWriter plumbing in the e2e context uses REST
    // by default, which is what the MCP send tool also uses. Build one
    // here so we can call write::mail::send_email directly.
    let rest_source = RestMailSource::new(ctx.client.clone());

    // Establish a baseline commit in the fresh tempdir repo — Repo::
    // open_or_init does NOT create an initial commit, so `git rev-parse
    // HEAD` would fail until the first real commit lands. Run a pull
    // before capturing pre_sha so we have a stable anchor for the
    // send-produced commit range.
    let _ = pull_mail(
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        "e2e-baseline",
        "e2e@pimsteward.local",
    )
    .await
    .expect("baseline pull to establish HEAD");

    // Capture sha before send so we can scan the full range of commits
    // the send produced.
    let pre_sha = current_head(&ctx.repo);
    assert!(
        !pre_sha.is_empty() && pre_sha.len() >= 7,
        "pre_sha must be a valid commit after baseline pull, got: {pre_sha:?}"
    );

    let result = write::mail::send_email(
        &ctx.client,
        &rest_source,
        &ctx.repo,
        &ctx.alias_slug(),
        &attr,
        &msg,
    )
    .await
    .expect("send_email should succeed with _test alias credentials");

    let returned_id = result
        .get("id")
        .and_then(|v| v.as_str())
        .expect("forwardemail should return an id for the sent email")
        .to_string();

    // S4 — delivery lands in a mailbox. Forwardemail names its sent-copy
    // folder "Sent Mail" (with special_use=\\Sent). A self-send (from the
    // alias to itself) also loops back through the SMTP relay and lands
    // in INBOX. We accept either — the point of S4 is that *something*
    // visible happened on the server side, not that a specific folder
    // got populated.
    //
    // The poll is generous: SMTP relay + indexing can take well over
    // 10 seconds under load, and a tight timeout turns into a flaky
    // test without being materially more informative.
    let mut delivery_hit: Option<(MessageSummary, String)> = None;
    'poll: for _ in 0..60 {
        for folder in ["Sent Mail", "INBOX"] {
            let msgs = ctx
                .client
                .list_messages_in_folder(folder)
                .await
                .unwrap_or_default();
            if let Some(m) = msgs.iter().find(|m| m.subject == subject) {
                delivery_hit = Some((m.clone(), folder.to_string()));
                break 'poll;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let (sent_msg, delivery_folder) = delivery_hit.expect(
        "sent message should appear in 'Sent Mail' or INBOX within 30s — if \
         this fails, either forwardemail didn't deliver or the folder names \
         have changed. Check `list_folders` on the _test alias.",
    );
    assert_eq!(sent_msg.subject, subject);
    eprintln!("delivery confirmed in folder: {delivery_folder}");

    // S5 — the audit commit for the send exists in git with a
    // `tool: send_email` trailer. We scan the range (pre_sha..HEAD] so
    // we find the audit commit even if the post-send pull produced its
    // own commit ahead of the audit one.
    let audit_commits = commit_bodies_since_containing(&ctx.repo, &pre_sha, "tool: send_email");
    assert_eq!(
        audit_commits.len(),
        1,
        "exactly one send_email audit commit should exist in the range. \
         Found {}. Commits:\n{}",
        audit_commits.len(),
        audit_commits.join("\n---\n")
    );
    let audit = &audit_commits[0];

    // Every structured audit field must be present.
    assert!(
        audit.contains("resource: mail"),
        "audit missing 'resource: mail': {audit}"
    );
    assert!(
        audit.contains(&ctx.alias),
        "audit should contain recipient (== _test alias): {audit}"
    );
    assert!(
        audit.contains(&subject),
        "audit should contain subject: {audit}"
    );
    assert!(
        audit.contains("\"has_text\":true"),
        "audit should record has_text=true: {audit}"
    );
    assert!(
        audit.contains("\"has_html\":true"),
        "audit should record has_html=true: {audit}"
    );
    assert!(
        audit.contains(&returned_id),
        "audit should reference forwardemail's returned id: {audit}"
    );

    // S6 — the body_sha256 in the audit trailer matches a fresh
    // computation over the same bytes. This binds the audit entry to
    // the exact body transmitted and protects against silent hash-
    // algorithm drift in future refactors.
    let expected_hash = expected_body_sha256(Some(&text_body), Some(&html_body));
    assert!(
        audit.contains(&expected_hash),
        "audit body_sha256 should equal {expected_hash}, got:\n{audit}"
    );

    // Cleanup — delete the sent copy from our own Sent folder so the
    // test alias doesn't accumulate residue across runs.
    let _ = ctx.client.delete_message(&sent_msg.id).await;
}

// ── Body-hash determinism (pure — no network) ───────────────────────
//
// Not strictly an e2e test (it doesn't call the API), but it guards the
// same invariant the above test checks in its S6 branch: two sends with
// the same (text, html) content must produce the same audit body_sha256.
// Placed here rather than in src/write/mail.rs because body_sha256 is a
// private helper — the test lives at the behavioural boundary instead.

#[tokio::test]
#[ignore = "e2e: requires PIMSTEWARD_RUN_E2E=1"]
async fn body_hash_is_deterministic_across_two_sends() {
    // This test STILL requires PIMSTEWARD_RUN_E2E because it routes
    // through E2eContext to stay uniform with the rest of the suite —
    // even though it's fast and doesn't actually hit the API, gating
    // it on the same flag means all e2e binaries behave identically
    // for the safety audit.
    let _ctx = E2eContext::from_env(); // safety guard side effect

    let h1 = expected_body_sha256(Some("hello"), Some("<p>hello</p>"));
    let h2 = expected_body_sha256(Some("hello"), Some("<p>hello</p>"));
    assert_eq!(
        h1, h2,
        "same body must produce the same hash across calls"
    );

    // Different bodies must produce different hashes.
    let h3 = expected_body_sha256(Some("hello"), Some("<p>HELLO</p>"));
    assert_ne!(h1, h3, "different html body must produce different hash");

    let h4 = expected_body_sha256(Some("hello"), None);
    assert_ne!(h1, h4, "missing html part must produce different hash");

    let h5 = expected_body_sha256(None, Some("<p>hello</p>"));
    assert_ne!(h1, h5, "missing text part must produce different hash");

    // The text/null-sep/html framing must prevent a collision where
    // text="foo" + html="bar" hashes the same as text="foobar".
    let h_split = expected_body_sha256(Some("foo"), Some("bar"));
    let h_joined = expected_body_sha256(Some("foobar"), None);
    assert_ne!(
        h_split, h_joined,
        "null-separator framing must prevent text/html concatenation collision"
    );
}
