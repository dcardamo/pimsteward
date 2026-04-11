//! Mail write operations: flag updates, folder moves, deletes, draft
//! creation. Mutations route through a [`MailWriter`] trait so the same
//! MCP tools and write functions work for both REST and IMAP backends.
//! Post-write refresh uses the caller-supplied [`MailSource`] to re-sync
//! the backup tree.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::mail::sync_folders;
use crate::source::{MailSource, MailWriter};
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use sha2::{Digest, Sha256};

/// Create a draft email via REST. Draft creation is REST-only because it
/// requires constructing a message from structured fields — IMAP would
/// need raw RFC822 bytes and an APPEND command, which is a different
/// (and larger) feature.
pub async fn create_draft(
    client: &Client,
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    msg: &crate::forwardemail::writes::NewMessage,
) -> Result<serde_json::Value, Error> {
    let result = client.create_message(msg).await?;
    let msg_id = result
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let audit = WriteAudit {
        attribution,
        tool: "create_draft",
        resource: "mail",
        resource_id: msg_id.to_string(),
        args: serde_json::json!({
            "folder": &msg.folder,
            "to": &msg.to,
            "subject": &msg.subject,
        }),
        summary: format!("mail: create draft in {} → {}", msg.folder, msg.subject),
    };
    // Only refresh the folder the draft landed in (usually "Drafts").
    refresh(source, repo, alias, attribution, &audit, &[&msg.folder]).await?;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_flags(
    writer: &dyn MailWriter,
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    folder: &str,
    id: &str,
    flags: &[String],
) -> Result<(), Error> {
    writer.update_flags(folder, id, flags).await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_flags",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({"flags": flags}),
        summary: format!("mail: update flags on {id} → {flags:?}"),
    };
    // Only the folder holding this message changed.
    refresh(source, repo, alias, attribution, &audit, &[folder]).await
}

#[allow(clippy::too_many_arguments)]
pub async fn move_message(
    writer: &dyn MailWriter,
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    source_folder: &str,
    id: &str,
    target_folder: &str,
) -> Result<(), Error> {
    writer
        .move_message(source_folder, id, target_folder)
        .await?;
    let audit = WriteAudit {
        attribution,
        tool: "move_message",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({"folder": target_folder}),
        summary: format!("mail: move {id} → {target_folder}"),
    };
    // Both source and target folders changed: source loses the msg,
    // target gains it. Refresh both so the backup tree stays
    // consistent with the live state.
    refresh(
        source,
        repo,
        alias,
        attribution,
        &audit,
        &[source_folder, target_folder],
    )
    .await
}

/// Send an email via forwardemail's SMTP bridge (POST /v1/emails) and
/// record an audit commit in git.
///
/// # Audit trail
///
/// The commit message carries a `tool: send_email` trailer (consumed by
/// `git log --grep='tool: send_email'`). Structured fields captured:
///
/// * `to`, `cc`, `bcc` — full recipient lists. These are the part of a
///   send you most want to be able to prove after the fact, so they land
///   in cleartext.
/// * `subject` — cleartext.
/// * `body_sha256` — SHA-256 of a canonical concatenation of the text and
///   html parts. The full body bytes land in git via the follow-up pull
///   that captures Sent. Storing just the hash in the audit trailer keeps
///   the commit message compact while still binding the audit entry to
///   the exact body that was transmitted.
/// * `has_text`, `has_html` — booleans indicating which MIME parts were
///   provided at the call site.
/// * `returned_id` — the forwardemail id of the created email record.
///
/// The commit is produced even when the post-send pull finds no new
/// messages yet (empty commit fallback) — the audit entry must exist
/// whether or not Sent has propagated.
pub async fn send_email(
    client: &Client,
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    msg: &crate::forwardemail::writes::NewMessage,
) -> Result<serde_json::Value, Error> {
    let body_hash = body_sha256(msg);
    let result = client.send_email(msg).await?;
    let returned_id = result
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let audit = WriteAudit {
        attribution,
        tool: "send_email",
        resource: "mail",
        resource_id: returned_id.clone(),
        args: serde_json::json!({
            "to": &msg.to,
            "cc": &msg.cc,
            "bcc": &msg.bcc,
            "subject": &msg.subject,
            "body_sha256": &body_hash,
            "has_text": msg.text.is_some(),
            "has_html": msg.html.is_some(),
            "returned_id": &returned_id,
        }),
        summary: format!("mail: SEND → {:?}: {}", msg.to, msg.subject),
    };
    // A send lands in Sent (or the Sent-like folder the server uses).
    // We don't know the exact path for every provider, but
    // forwardemail uses "Sent Mail". Pass that so we refresh only
    // that folder; if the provider uses a different name,
    // sync_folders silently skips the unknown path and we still get
    // the audit commit via the empty_commit fallback.
    refresh(
        source,
        repo,
        alias,
        attribution,
        &audit,
        &["Sent Mail", "Sent"],
    )
    .await?;
    Ok(result)
}

/// Canonical SHA-256 of a send's body bytes. Hashes `text\0html` with a
/// null separator so text-only and html-only bodies with the same visible
/// content produce distinct hashes, and so reordering fields between API
/// revisions can't collide two different sends onto one hash.
fn body_sha256(msg: &crate::forwardemail::writes::NewMessage) -> String {
    let mut h = Sha256::new();
    if let Some(ref t) = msg.text {
        h.update(b"text:");
        h.update(t.as_bytes());
    }
    h.update(b"\0");
    if let Some(ref html) = msg.html {
        h.update(b"html:");
        h.update(html.as_bytes());
    }
    format!("{:x}", h.finalize())
}

pub async fn delete_message(
    writer: &dyn MailWriter,
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    folder: &str,
    id: &str,
) -> Result<(), Error> {
    writer.delete_message(folder, id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_message",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("mail: delete {id}"),
    };
    refresh(source, repo, alias, attribution, &audit, &[folder]).await
}

/// Refresh the local backup tree after a write, but only for the
/// folders the write actually touched. Previously this called
/// [`pull_mail`] which syncs every folder in the mailbox — for a
/// mailbox with 20+ folders and an ongoing initial-sync backlog,
/// that turned every `create_draft`/`move_email` into a 2-10 minute
/// blocking call per Apr 11 2026 investigation. Writes now only
/// refresh the folder(s) they operated on (e.g. `create_draft` →
/// `["Drafts"]`, `move_email` → `[source_folder, target_folder]`)
/// which is near-instant on a healthy folder and bounded even during
/// a backlog.
///
/// An empty slice is valid — callers that can't name a specific
/// folder (e.g. unusual send flows) still get an audit commit via
/// the empty_commit fallback below without attempting any sync.
pub async fn refresh(
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
    folders: &[&str],
) -> Result<(), Error> {
    // Refresh the backup tree for only the affected folders so we
    // stay consistent with what the daemon reads, without paying the
    // full-mailbox resync cost on every write.
    if !folders.is_empty() {
        let _ = sync_folders(
            source,
            repo,
            alias,
            &attribution.caller,
            &attribution.caller_email,
            folders,
        )
        .await?;
    }
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}
