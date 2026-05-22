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

/// Update a contact with a full vCard replacement. Carries the structured
/// fields (emails/phones/etc.) that `update_contact_name` can't express —
/// the caller builds (or synthesizes) the vCard, we extract the FN for
/// the `full_name` field the PUT endpoint insists on, and forwardemail's
/// contact row is rewritten from the parsed card.
pub async fn update_contact_vcard(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    vcard: &str,
    if_match: Option<&str>,
) -> Result<Contact, Error> {
    let full_name = extract_vcard_fn(vcard).unwrap_or_else(|| id.to_string());
    let updated = client
        .update_contact_vcard(id, vcard, &full_name, if_match)
        .await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_contact_vcard",
        resource: "contacts",
        resource_id: id.to_string(),
        args: serde_json::json!({
            "full_name": full_name,
            "if_match": if_match,
            "vcard_bytes": vcard.len(),
        }),
        summary: format!("contacts: update {id} (vcard, full_name={full_name})"),
    };
    refresh_and_commit(client, repo, alias, &audit).await?;
    Ok(updated)
}

/// Extract the `FN:` line value from a vCard. Unfolds RFC 6350 line
/// continuations (a leading space on the next line means "joined to the
/// previous one") but does not otherwise interpret escapes.
fn extract_vcard_fn(vcard: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in vcard.lines() {
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            let last = lines.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            lines.push(line.to_string());
        }
    }
    for line in lines {
        let upper = line.to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("FN:") {
            // Find the matching position in the original line (preserves
            // the original case of the value).
            let idx = line.len() - rest.len();
            let val = line[idx..].trim().to_string();
            if !val.is_empty() {
                return Some(val);
            }
        } else if upper.starts_with("FN;") {
            // Parameterised FN (e.g. FN;CHARSET=UTF-8:Alex Kim). Take
            // everything after the first `:`.
            if let Some((_, val)) = line.split_once(':') {
                let val = val.trim().to_string();
                if !val.is_empty() {
                    return Some(val);
                }
            }
        }
    }
    None
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
