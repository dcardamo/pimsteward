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
