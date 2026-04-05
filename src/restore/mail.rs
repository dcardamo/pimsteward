//! Mail restore.
//!
//! Mutable metadata (flags, folder) is restored by PUTing against the
//! existing message. If the message has been hard-deleted from
//! forwardemail, the restore reads the original RFC822 bytes from the
//! git `.eml` at the target SHA and **re-appends** them via
//! `POST /v1/messages` (IMAP APPEND equivalent). The restored message
//! gets a new backend id but is byte-identical to the historical version,
//! so mail clients re-syncing the folder will see it again.
//!
//! Caveat: a re-appended message has a new `uid` and forwardemail id.
//! The audit commit records the new id so the history tracks the
//! restoration explicitly rather than silently reusing the old id.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::mail::pull_mail;
use crate::restore::read_git_blob;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailRestorePlan {
    pub path: String,
    pub at_sha: String,
    pub folder: String,
    pub message_id: String,
    pub operation: MailOperation,
    pub human_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MailOperation {
    /// Message exists live but its flags differ from the historical state.
    RestoreFlags { target_flags: Vec<String> },
    /// Message exists live but is in a different folder than historically.
    MoveBack { target_folder: String },
    /// Live flags+folder match historical already.
    NoOp,
    /// Message has been deleted from forwardemail. Restore will re-append
    /// the raw RFC822 bytes read from `<folder>/<id>.eml` at `at_sha`.
    /// `raw_bytes` carries the historical payload in the plan itself so
    /// the token binds to the exact content being restored.
    Append {
        target_folder: String,
        raw_bytes: Vec<u8>,
    },
}

pub async fn plan_mail(
    client: &Client,
    repo: &Repo,
    alias: &str,
    folder: &str,
    message_id: &str,
    at_sha: &str,
) -> Result<(MailRestorePlan, String), Error> {
    let folder_safe = folder.replace('/', "_");
    let meta_path =
        format!("sources/forwardemail/{alias}/mail/{folder_safe}/{message_id}.meta.json");
    let historical_meta_bytes = read_git_blob(repo, at_sha, &meta_path)?;
    let historical_meta: serde_json::Value = serde_json::from_slice(&historical_meta_bytes)?;
    let historical_flags: Vec<String> = historical_meta
        .get("flags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Fetch live state
    let live = client.get_message(message_id).await;
    let (operation, human_summary) = match live {
        Err(Error::Api { status: 404, .. }) | Err(Error::Api { status: 410, .. }) => {
            // Message is gone from forwardemail. Read the historical
            // .eml from git at at_sha and plan an Append.
            let eml_path =
                format!("sources/forwardemail/{alias}/mail/{folder_safe}/{message_id}.eml");
            let raw_bytes = read_git_blob(repo, at_sha, &eml_path)?;
            let size = raw_bytes.len();
            (
                MailOperation::Append {
                    target_folder: folder.to_string(),
                    raw_bytes,
                },
                format!(
                    "Message {message_id} has been deleted from forwardemail. \
                     Restore will re-append the historical RFC822 ({size} bytes) \
                     to folder '{folder}'. The restored message will have a new \
                     backend id."
                ),
            )
        }
        Err(e) => return Err(e),
        Ok(live_msg) => {
            let live_flags: Vec<String> = live_msg
                .get("flags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let live_folder = live_msg
                .get("folder_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            if live_folder != folder {
                (
                    MailOperation::MoveBack {
                        target_folder: folder.to_string(),
                    },
                    format!(
                        "Message {message_id} is in '{live_folder}' but was historically in '{folder}'. Restore will move it back."
                    ),
                )
            } else if live_flags != historical_flags {
                (
                    MailOperation::RestoreFlags {
                        target_flags: historical_flags.clone(),
                    },
                    format!(
                        "Message {message_id} flags live={live_flags:?} differ from historical={historical_flags:?}. Restore will reset to historical."
                    ),
                )
            } else {
                (
                    MailOperation::NoOp,
                    format!("Message {message_id} already matches historical state."),
                )
            }
        }
    };

    let plan = MailRestorePlan {
        path: meta_path,
        at_sha: at_sha.to_string(),
        folder: folder.to_string(),
        message_id: message_id.to_string(),
        operation,
        human_summary,
    };
    let token = crate::restore::plan_token(&plan)?;
    Ok((plan, token))
}

pub async fn apply_mail(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    plan: &MailRestorePlan,
    supplied_token: &str,
) -> Result<(), Error> {
    let computed = crate::restore::plan_token(plan)?;
    if computed != supplied_token {
        return Err(Error::config(format!(
            "restore plan_token mismatch (mail): expected {computed}, got {supplied_token}"
        )));
    }

    match &plan.operation {
        MailOperation::NoOp => return Ok(()),
        MailOperation::RestoreFlags { target_flags } => {
            client
                .update_message_flags(&plan.message_id, target_flags)
                .await?;
        }
        MailOperation::MoveBack { target_folder } => {
            client.move_message(&plan.message_id, target_folder).await?;
        }
        MailOperation::Append {
            target_folder,
            raw_bytes,
        } => {
            // Re-append the historical message. Forwardemail generates a
            // new backend id; the pull loop on refresh below will capture
            // it as a brand-new .eml in the tree (the old id stays gone).
            client.append_raw_message(target_folder, raw_bytes).await?;
        }
    }

    // Refresh via REST regardless of read source (same rationale as write/mail.rs).
    let rest_source = crate::source::RestMailSource::new(client.clone());
    let _ = pull_mail(
        &rest_source,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let audit = WriteAudit {
        attribution,
        tool: "restore_mail",
        resource: "mail",
        resource_id: plan.message_id.clone(),
        args: serde_json::to_value(plan)?,
        summary: format!(
            "restore: mail/{}/{} from {}",
            plan.folder,
            plan.message_id,
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
