//! Mail write operations. v1 only handles safe mutations:
//! - flag updates (read/unread/flagged/etc.)
//! - move between folders
//! - delete
//!
//! Body rewrites are not supported because forwardemail silently ignores
//! them (`.eml` is effectively immutable — see docs/api-findings.md §Q4).

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::mail::pull_mail;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};

pub async fn update_flags(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    flags: &[String],
) -> Result<(), Error> {
    let _ = client.update_message_flags(id, flags).await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_flags",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({"flags": flags}),
        summary: format!("mail: update flags on {id} → {flags:?}"),
    };
    refresh(client, repo, alias, attribution, &audit).await
}

pub async fn move_message(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    folder: &str,
) -> Result<(), Error> {
    let _ = client.move_message(id, folder).await?;
    let audit = WriteAudit {
        attribution,
        tool: "move_message",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({"folder": folder}),
        summary: format!("mail: move {id} → {folder}"),
    };
    refresh(client, repo, alias, attribution, &audit).await
}

pub async fn delete_message(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
) -> Result<(), Error> {
    client.delete_message(id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_message",
        resource: "mail",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("mail: delete {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await
}

async fn refresh(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    let _ = pull_mail(
        client,
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
