//! Sieve script write operations.

use crate::error::Error;
use crate::forwardemail::sieve::SieveScript;
use crate::forwardemail::Client;
use crate::pull::sieve::pull_sieve;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};

pub async fn install_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    name: &str,
    content: &str,
) -> Result<SieveScript, Error> {
    let created = client.create_sieve_script(name, content).await?;
    // Early warning: surface server-side validation issues before committing.
    if !created.is_valid {
        return Err(Error::Api {
            status: 422,
            message: format!(
                "sieve script '{name}' was accepted by forwardemail but flagged as invalid: {:?}",
                created.validation_errors
            ),
        });
    }
    let audit = WriteAudit {
        attribution,
        tool: "install_sieve_script",
        resource: "sieve",
        resource_id: created.id.clone(),
        args: serde_json::json!({"name": name, "content_bytes": content.len()}),
        summary: format!("sieve: install {name}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(created)
}

pub async fn update_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    content: &str,
) -> Result<SieveScript, Error> {
    let updated = client.update_sieve_script(id, content).await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_sieve_script",
        resource: "sieve",
        resource_id: id.to_string(),
        args: serde_json::json!({"content_bytes": content.len()}),
        summary: format!("sieve: update {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

pub async fn delete_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
) -> Result<(), Error> {
    client.delete_sieve_script(id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_sieve_script",
        resource: "sieve",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("sieve: delete {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(())
}

async fn refresh(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    let _ = pull_sieve(
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
