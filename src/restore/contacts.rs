//! Contact restore implementation.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::contacts::pull_contacts;
use crate::restore::read_git_blob;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use serde::{Deserialize, Serialize};

/// A plan describes exactly what a restore will do. Serialized into the
/// plan_token hash so any change invalidates the previously-returned token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestorePlan {
    /// Path to the contact's .vcf in the backup tree.
    pub path: String,
    /// Git commit from which to pull the historical state.
    pub at_sha: String,
    /// Human-readable contact identifier (the iCard UID).
    pub contact_uid: String,
    /// What the restore will do.
    pub operation: RestoreOperation,
    /// Forwardemail id of the live contact (None if the contact doesn't
    /// currently exist — restore will have to re-create).
    pub live_id: Option<String>,
    /// Free-text summary for the AI to show to the user before confirming.
    pub human_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestoreOperation {
    /// Contact existed at `at_sha` and exists now but differs.
    Update {
        target_full_name: String,
        /// Raw vCard 3.0 text from the historical backup. Used for a
        /// full-field restore (not just the name) via PUT {content}.
        historical_vcard: String,
    },
    /// Contact existed at `at_sha` but has been deleted live. Carries
    /// the full historical vCard so re-creation preserves all fields
    /// (emails, phones, addresses, notes, org, etc.), not just full_name.
    Recreate {
        full_name: String,
        /// Raw vCard 3.0 text from the historical backup.
        historical_vcard: String,
    },
    /// Nothing to do — live state already matches the historical state.
    NoOp,
}

/// Compute a restore plan for one contact. Does not touch forwardemail.
///
/// `contact_uid` is the iCard UID (the filename stem in the backup tree),
/// not the forwardemail id. UIDs are stable across versions.
pub async fn plan_contact(
    client: &Client,
    repo: &Repo,
    alias: &str,
    contact_uid: &str,
    at_sha: &str,
) -> Result<(RestorePlan, String), Error> {
    let rel_path = format!("sources/forwardemail/{alias}/contacts/default/{contact_uid}.meta.json");

    // Read historical meta + vcard from the target commit using `git show`
    let historical_meta = read_git_blob(repo, at_sha, &rel_path)?;
    let historical_meta: serde_json::Value = serde_json::from_slice(&historical_meta)?;
    let historical_name = historical_meta
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from); // not the name — meta.json stores id/etag only
    drop(historical_name);

    // Historical vCard for the full_name
    let vcf_rel = format!("sources/forwardemail/{alias}/contacts/default/{contact_uid}.vcf");
    let historical_vcf =
        String::from_utf8_lossy(&read_git_blob(repo, at_sha, &vcf_rel)?).into_owned();
    let historical_full_name =
        extract_vcard_fn(&historical_vcf).unwrap_or_else(|| contact_uid.to_string());

    // Compare to live state: look up the contact by uid in the live contact list
    let live = client.list_contacts().await?;
    let live_contact = live.iter().find(|c| c.uid == contact_uid);

    let (operation, human_summary, live_id) = match live_contact {
        None => {
            let op = RestoreOperation::Recreate {
                full_name: historical_full_name.clone(),
                historical_vcard: historical_vcf.clone(),
            };
            let summary = format!(
                "Contact '{historical_full_name}' was deleted from forwardemail. \
                 Restore will re-create it from the full historical vCard."
            );
            (op, summary, None)
        }
        // Compare the full vCard content, not just the name. If the
        // live vCard matches the historical one byte-for-byte, there's
        // nothing to do. If only the name or any other field differs,
        // we restore the full historical vCard.
        Some(live) if live.content == historical_vcf => (
            RestoreOperation::NoOp,
            format!("Contact '{historical_full_name}' already matches — nothing to do."),
            Some(live.id.clone()),
        ),
        Some(live) => {
            let op = RestoreOperation::Update {
                target_full_name: historical_full_name.clone(),
                historical_vcard: historical_vcf.clone(),
            };
            let summary = format!(
                "Contact '{}' differs from historical '{historical_full_name}'. \
                 Restore will update to the full historical vCard.",
                live.full_name
            );
            (op, summary, Some(live.id.clone()))
        }
    };

    let plan = RestorePlan {
        path: rel_path,
        at_sha: at_sha.to_string(),
        contact_uid: contact_uid.to_string(),
        operation,
        live_id,
        human_summary,
    };

    let token = crate::restore::plan_token(&plan)?;
    Ok((plan, token))
}

/// Execute a restore plan. Re-computes the token from the submitted plan
/// and refuses if it doesn't match the one supplied by the caller.
pub async fn apply_contact(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    plan: &RestorePlan,
    supplied_token: &str,
) -> Result<(), Error> {
    let computed = crate::restore::plan_token(plan)?;
    if computed != supplied_token {
        return Err(Error::config(format!(
            "restore plan_token mismatch: the plan you passed to apply doesn't \
             match the one returned by dry-run. This means the plan was \
             modified between the two calls (or you used a stale token). \
             Expected {computed}, got {supplied_token}."
        )));
    }

    // Execute the operation
    match &plan.operation {
        RestoreOperation::NoOp => {
            tracing::info!(contact = %plan.contact_uid, "restore is a no-op");
            return Ok(());
        }
        RestoreOperation::Update {
            historical_vcard, ..
        } => {
            let id = plan
                .live_id
                .as_ref()
                .ok_or_else(|| Error::config("Update op requires live_id in plan"))?;
            // PUT the full historical vCard — forwardemail parses it
            // server-side and updates all structured fields.
            client
                .update_contact_vcard(id, historical_vcard, None)
                .await?;
        }
        RestoreOperation::Recreate {
            historical_vcard, ..
        } => {
            // POST the raw historical vCard — forwardemail creates the
            // contact with all fields intact (emails, phones, addresses,
            // notes, org, etc.), no placeholder needed.
            client
                .create_contact_from_vcard(historical_vcard)
                .await?;
        }
    }

    // Refresh + attributed commit
    let rest_source = crate::source::RestContactsSource::new(client.clone());
    let _ = pull_contacts(
        &rest_source,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let audit = WriteAudit {
        attribution,
        tool: "restore_contact",
        resource: "contacts",
        resource_id: plan.contact_uid.clone(),
        args: serde_json::to_value(plan)?,
        summary: format!(
            "restore: contacts/{} from {}",
            plan.contact_uid,
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

// read_git_blob moved to restore/mod.rs as a shared helper across resources.

/// Extract the FN: line from a vCard. Minimal parser — good enough for
/// the smoke-tested forwardemail output format.
fn extract_vcard_fn(vcf: &str) -> Option<String> {
    for line in vcf.lines() {
        if let Some(rest) = line.strip_prefix("FN:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_fn_from_vcard() {
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nUID:abc\nFN:Alice Smith\nEMAIL:a@b.com\nEND:VCARD";
        assert_eq!(extract_vcard_fn(vcf), Some("Alice Smith".into()));
    }

    #[test]
    fn plan_token_is_deterministic_for_same_plan() {
        let p = RestorePlan {
            path: "a/b.vcf".into(),
            at_sha: "abc123".into(),
            contact_uid: "uid1".into(),
            operation: RestoreOperation::Update {
                target_full_name: "X".into(),
                historical_vcard: "BEGIN:VCARD\nFN:X\nEND:VCARD".into(),
            },
            live_id: Some("fel-1".into()),
            human_summary: "test".into(),
        };
        let t1 = crate::restore::plan_token(&p).unwrap();
        let t2 = crate::restore::plan_token(&p).unwrap();
        assert_eq!(t1, t2);
        assert_eq!(t1.len(), 64);
    }
}
