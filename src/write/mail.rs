//! Mail write operations: flag updates, folder moves, deletes, draft
//! creation. Mutations route through a [`MailWriter`] trait so the same
//! MCP tools and write functions work for both REST and IMAP backends.
//! Post-write refresh uses the caller-supplied [`MailSource`] to re-sync
//! the backup tree.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::mail::pull_mail;
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
    refresh(source, repo, alias, attribution, &audit).await?;
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
    refresh(source, repo, alias, attribution, &audit).await
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
    refresh(source, repo, alias, attribution, &audit).await
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
    refresh(source, repo, alias, attribution, &audit).await?;
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
    refresh(source, repo, alias, attribution, &audit).await
}

async fn refresh(
    source: &dyn MailSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    // Refresh the backup tree using the same source the daemon reads
    // from, so IDs, folder layout, and metadata stay consistent.
    let _ = pull_mail(
        source,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}
