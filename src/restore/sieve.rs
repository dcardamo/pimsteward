//! Sieve script restore.
//!
//! Simpler than contacts because sieve scripts have a `name` that's stable
//! and meaningful (it's the filename in the backup tree). Operations:
//! - `Recreate` — script was deleted from forwardemail
//! - `UpdateContent` — script exists but its content differs
//! - `NoOp` — live content already matches historical

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::sieve::pull_sieve;
use crate::restore::read_git_blob;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SieveRestorePlan {
    pub path: String,
    pub at_sha: String,
    pub script_name: String,
    pub operation: SieveOperation,
    pub live_id: Option<String>,
    pub human_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SieveOperation {
    UpdateContent { target_content: String },
    Recreate { content: String },
    NoOp,
}

pub async fn plan_sieve(
    client: &Client,
    repo: &Repo,
    _alias: &str,
    script_name: &str,
    at_sha: &str,
) -> Result<(SieveRestorePlan, String), Error> {
    let rel_path = format!("sieve/{script_name}.sieve");
    let historical = String::from_utf8_lossy(&read_git_blob(repo, at_sha, &rel_path)?).into_owned();

    let live = client.list_sieve_scripts().await?;
    let live_script = live.iter().find(|s| s.name == script_name);

    // Fetch full content if the live script exists (list endpoint may not
    // include content).
    let live_content = if let Some(s) = live_script {
        client
            .get_sieve_script(&s.id)
            .await?
            .content
            .unwrap_or_default()
    } else {
        String::new()
    };

    let (operation, human_summary, live_id) = match live_script {
        None => (
            SieveOperation::Recreate {
                content: historical.clone(),
            },
            format!(
                "Sieve script '{script_name}' was deleted from forwardemail. \
                 Restore will re-install it from the historical content ({} bytes).",
                historical.len()
            ),
            None,
        ),
        Some(s) if live_content == historical => (
            SieveOperation::NoOp,
            format!("Sieve script '{script_name}' already matches — nothing to do."),
            Some(s.id.clone()),
        ),
        Some(s) => (
            SieveOperation::UpdateContent {
                target_content: historical.clone(),
            },
            format!(
                "Sieve script '{script_name}' content differs. Restore will \
                 replace the live content with the historical version \
                 ({} bytes → {} bytes).",
                live_content.len(),
                historical.len()
            ),
            Some(s.id.clone()),
        ),
    };

    let plan = SieveRestorePlan {
        path: rel_path,
        at_sha: at_sha.to_string(),
        script_name: script_name.to_string(),
        operation,
        live_id,
        human_summary,
    };
    let token = crate::restore::plan_token(&plan)?;
    Ok((plan, token))
}

pub async fn apply_sieve(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    plan: &SieveRestorePlan,
    supplied_token: &str,
) -> Result<(), Error> {
    let computed = crate::restore::plan_token(plan)?;
    if computed != supplied_token {
        return Err(Error::config(format!(
            "restore plan_token mismatch (sieve): expected {computed}, got {supplied_token}"
        )));
    }

    match &plan.operation {
        SieveOperation::NoOp => return Ok(()),
        SieveOperation::UpdateContent { target_content } => {
            let id = plan
                .live_id
                .as_ref()
                .ok_or_else(|| Error::config("UpdateContent op requires live_id in plan"))?;
            client.update_sieve_script(id, target_content).await?;
        }
        SieveOperation::Recreate { content } => {
            client
                .create_sieve_script(&plan.script_name, content)
                .await?;
        }
    }

    let _ = pull_sieve(
        client,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let audit = WriteAudit {
        attribution,
        tool: "restore_sieve",
        resource: "sieve",
        resource_id: plan.script_name.clone(),
        args: serde_json::to_value(plan)?,
        summary: format!(
            "restore: sieve/{} from {}",
            plan.script_name,
            &plan.at_sha[..8.min(plan.at_sha.len())]
        ),
    };
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}
