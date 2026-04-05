//! Contact write operations: create, update, delete.
//!
//! Each function hits the forwardemail API, then triggers a contacts pull
//! cycle to refresh the git tree, producing a commit with the caller's
//! attribution.

use crate::error::Error;
use crate::forwardemail::contacts::Contact;
use crate::forwardemail::Client;
use crate::pull::contacts::pull_contacts;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};

pub async fn create_contact(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    full_name: &str,
    emails: &[(&str, &str)],
) -> Result<Contact, Error> {
    let created = client.create_contact(full_name, emails).await?;
    let audit = WriteAudit {
        attribution,
        tool: "create_contact",
        resource: "contacts",
        resource_id: created.id.clone(),
        args: serde_json::json!({
            "full_name": full_name,
            "emails": emails.iter().map(|(t, v)| serde_json::json!({"type": t, "value": v})).collect::<Vec<_>>(),
        }),
        summary: format!("contacts: create {full_name}"),
    };
    refresh_and_commit(client, repo, alias, &audit).await?;
    Ok(created)
}

pub async fn update_contact_name(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    full_name: &str,
    if_match: Option<&str>,
) -> Result<Contact, Error> {
    let updated = client.update_contact_name(id, full_name, if_match).await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_contact_name",
        resource: "contacts",
        resource_id: id.to_string(),
        args: serde_json::json!({"full_name": full_name, "if_match": if_match}),
        summary: format!("contacts: update {id} full_name={full_name}"),
    };
    refresh_and_commit(client, repo, alias, &audit).await?;
    Ok(updated)
}

pub async fn delete_contact(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
) -> Result<(), Error> {
    client.delete_contact(id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_contact",
        resource: "contacts",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("contacts: delete {id}"),
    };
    refresh_and_commit(client, repo, alias, &audit).await?;
    Ok(())
}

/// Re-pull contacts and commit with the attribution's identity. This
/// ensures the git tree is consistent with forwardemail even if an
/// unrelated mutation happened between our write and the commit.
async fn refresh_and_commit(
    client: &Client,
    repo: &Repo,
    alias: &str,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    // The pull loop does the file diff and commits its own summary. We
    // override by calling the commit helper directly after the file write,
    // passing our attributed message.
    // Writes always use the REST source for the refresh — the REST API
    // is the single source of truth for writes regardless of the
    // daemon's read-side source choice.
    let rest_source = crate::source::RestContactsSource::new(client.clone());
    let _ = pull_contacts(
        &rest_source,
        repo,
        alias,
        &audit.attribution.caller,
        &audit.attribution.caller_email,
    )
    .await?;
    // pull_contacts already committed with its own message — amend it with
    // our richer attribution by making a "note" commit on top that carries
    // only the structured audit block. This keeps pull's commit intact for
    // history and adds a second commit that's explicitly grep-able for
    // `git log --author=<caller>`.
    //
    // Simpler alternative: do the commit ourselves. Use the commit-all
    // path on the repo directly — if pull_contacts made no changes (a
    // race where forwardemail reflects the write instantly and no diff
    // exists), we still want the audit commit to exist. Make an empty
    // commit in that case.
    let msg = audit.commit_message();
    let sha = repo.commit_all(
        &audit.attribution.caller,
        &audit.attribution.caller_email,
        &msg,
    )?;
    if sha.is_none() {
        // Nothing changed on disk (pull already captured the state).
        // Make an empty commit so the audit trail is complete.
        repo.empty_commit(
            &audit.attribution.caller,
            &audit.attribution.caller_email,
            &msg,
        )?;
    }
    Ok(())
}
