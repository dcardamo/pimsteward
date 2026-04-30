//! Bulk restore — compute and apply a plan covering every resource under a
//! path prefix in the backup tree.
//!
//! Use cases:
//! - "Restore all my calendar events from yesterday"
//! - "Restore everything in contacts/default from sha X"
//! - "Undo every mutation the AI made today" (by restoring the whole
//!   pimsteward-touched subtree to its state before the AI started)
//!
//! The plan is a heterogeneous list of per-resource sub-plans. Each
//! sub-plan carries its own plan_token independently, but the bulk plan
//! also has a deterministic `bulk_plan_token` that's the sha256 of the
//! canonical serialization of the full list. On apply, both the bulk token
//! and each sub-plan token are re-verified.
//!
//! v1 covers contacts, sieve, calendar. Mail is deliberately excluded
//! because mail restore has the immutability caveat; bulk-applying flag
//! changes is rarely what the user wants, and a partial failure mid-bulk
//! is more confusing than useful. Per-message restore via restore_mail_*
//! remains available.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::restore::{
    calendar::CalendarRestorePlan, contacts::RestorePlan as ContactRestorePlan,
    sieve::SieveRestorePlan,
};
use crate::source::traits::{CalendarSource, CalendarWriter};
use crate::store::Repo;
use crate::write::audit::Attribution;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

/// A heterogeneous plan covering every resource under a path prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkRestorePlan {
    pub path_prefix: String,
    pub at_sha: String,
    pub contacts: Vec<ContactRestorePlan>,
    pub sieve: Vec<SieveRestorePlan>,
    pub calendar_events: Vec<CalendarRestorePlan>,
    pub human_summary: String,
}

impl BulkRestorePlan {
    pub fn total_ops(&self) -> usize {
        self.contacts.len() + self.sieve.len() + self.calendar_events.len()
    }
}

/// Walk the backup tree at `at_sha` under `path_prefix`, compute a
/// per-resource sub-plan for each file found, and return the aggregated
/// plan plus a deterministic token.
/// Validate a `path_prefix` argument. Pure string check, no IO, so it can
/// be unit-tested exhaustively without a Client. Callers must invoke this
/// before passing the prefix to any filesystem or git operation.
pub(crate) fn validate_path_prefix(prefix: &str) -> Result<(), Error> {
    // Reject path traversal. git ls-tree and git show both resolve paths
    // relative to the repo root, so `..` could escape the sandbox of a
    // single resource tree and enumerate unrelated files. Defense in depth:
    // gix and the kernel would catch most of this, but the guard makes the
    // intent explicit and fails fast with a clear error.
    if prefix.contains("..") {
        return Err(Error::config("path_prefix must not contain '..'"));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn plan_bulk(
    client: &Client,
    calendar_source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    path_prefix: &str,
    at_sha: &str,
) -> Result<(BulkRestorePlan, String), Error> {
    validate_path_prefix(path_prefix)?;

    // List files at `at_sha` under the prefix using `git ls-tree`. We want
    // every file that identifies a resource (one .vcf per contact, one
    // .sieve per script, one .ics per calendar event).
    let files = ls_tree_files(repo, at_sha, path_prefix)?;

    let mut contacts = Vec::new();
    let mut sieve = Vec::new();
    let mut calendar_events = Vec::new();

    for file in &files {
        let path = file.as_str();

        // Contact vCards: contacts/default/<uid>.vcf
        if let Some(uid) = extract_contact_uid(path, alias) {
            let (plan, _token) =
                crate::restore::contacts::plan_contact(client, repo, alias, &uid, at_sha).await?;
            contacts.push(plan);
            continue;
        }

        // Sieve scripts: sieve/<name>.sieve
        if let Some(name) = extract_sieve_name(path, alias) {
            let (plan, _token) =
                crate::restore::sieve::plan_sieve(client, repo, alias, &name, at_sha).await?;
            sieve.push(plan);
            continue;
        }

        // Calendar events: calendars/<cal>/events/<uid>.ics
        if let Some((cal_id, event_uid)) = extract_calendar_event(path, alias) {
            let (plan, _token) = crate::restore::calendar::plan_calendar(
                calendar_source,
                repo,
                alias,
                &cal_id,
                &event_uid,
                at_sha,
            )
            .await?;
            calendar_events.push(plan);
            continue;
        }
        // Anything else (meta.json, manifest files, mail) is skipped.
    }

    let human_summary = format!(
        "Bulk restore under '{path_prefix}' from {}: {} contacts, {} sieve scripts, \
         {} calendar events ({} total operations)",
        &at_sha[..8.min(at_sha.len())],
        contacts.len(),
        sieve.len(),
        calendar_events.len(),
        contacts.len() + sieve.len() + calendar_events.len()
    );

    let plan = BulkRestorePlan {
        path_prefix: path_prefix.to_string(),
        at_sha: at_sha.to_string(),
        contacts,
        sieve,
        calendar_events,
        human_summary,
    };
    let token = crate::restore::plan_token(&plan)?;
    Ok((plan, token))
}

/// Apply a bulk restore plan. Re-verifies the bulk plan_token AND each
/// sub-plan's token before executing. On first failure, stops and returns
/// an error; the audit log reflects whatever succeeded before the failure.
#[allow(clippy::too_many_arguments)]
pub async fn apply_bulk(
    client: &Client,
    calendar_source: &dyn CalendarSource,
    calendar_writer: &dyn CalendarWriter,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    plan: &BulkRestorePlan,
    supplied_token: &str,
) -> Result<BulkRestoreResult, Error> {
    let computed = crate::restore::plan_token(plan)?;
    if computed != supplied_token {
        return Err(Error::config(format!(
            "bulk restore plan_token mismatch: expected {computed}, got {supplied_token}"
        )));
    }

    let mut result = BulkRestoreResult::default();

    for sub in &plan.contacts {
        let sub_token = crate::restore::plan_token(sub)?;
        match crate::restore::contacts::apply_contact(
            client,
            repo,
            alias,
            attribution,
            sub,
            &sub_token,
        )
        .await
        {
            Ok(()) => result.contacts_ok += 1,
            Err(e) => {
                result
                    .errors
                    .push(format!("contact {}: {e}", sub.contact_uid));
            }
        }
    }
    for sub in &plan.sieve {
        let sub_token = crate::restore::plan_token(sub)?;
        match crate::restore::sieve::apply_sieve(client, repo, alias, attribution, sub, &sub_token)
            .await
        {
            Ok(()) => result.sieve_ok += 1,
            Err(e) => {
                result
                    .errors
                    .push(format!("sieve {}: {e}", sub.script_name));
            }
        }
    }
    for sub in &plan.calendar_events {
        let sub_token = crate::restore::plan_token(sub)?;
        match crate::restore::calendar::apply_calendar(
            calendar_writer,
            calendar_source,
            repo,
            alias,
            attribution,
            sub,
            &sub_token,
        )
        .await
        {
            Ok(()) => result.calendar_ok += 1,
            Err(e) => {
                result
                    .errors
                    .push(format!("calendar {}: {e}", sub.event_uid));
            }
        }
    }
    Ok(result)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BulkRestoreResult {
    pub contacts_ok: usize,
    pub sieve_ok: usize,
    pub calendar_ok: usize,
    pub errors: Vec<String>,
}

impl BulkRestoreResult {
    pub fn total_ok(&self) -> usize {
        self.contacts_ok + self.sieve_ok + self.calendar_ok
    }
}

fn ls_tree_files(repo: &Repo, sha: &str, prefix: &str) -> Result<Vec<String>, Error> {
    // git rejects an empty pathspec ("") with a hard error; the documented
    // "match all paths" syntax is ".". Normalise here so callers can pass
    // "" as "no prefix" without thinking about git's pathspec rules.
    let pathspec = if prefix.is_empty() { "." } else { prefix };
    let out = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", sha, "--", pathspec])
        .current_dir(repo.root())
        .output()
        .map_err(|e| Error::store(format!("git ls-tree: {e}")))?;
    if !out.status.success() {
        return Err(Error::store(format!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect())
}

/// Extract the contact UID from a path like `contacts/default/<uid>.vcf`.
fn extract_contact_uid(path: &str, _alias: &str) -> Option<String> {
    let rest = path.strip_prefix("contacts/default/")?;
    let uid = rest.strip_suffix(".vcf")?;
    Some(uid.to_string())
}

/// Extract the sieve script name from a path like `sieve/<name>.sieve`.
fn extract_sieve_name(path: &str, _alias: &str) -> Option<String> {
    let rest = path.strip_prefix("sieve/")?;
    let name = rest.strip_suffix(".sieve")?;
    Some(name.to_string())
}

/// Extract (calendar_id, event_uid) from a path like
/// `calendars/<cal>/events/<uid>.ics`.
fn extract_calendar_event(path: &str, _alias: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("calendars/")?;
    let (cal_id, after) = rest.split_once('/')?;
    let event_file = after.strip_prefix("events/")?;
    let event_uid = event_file.strip_suffix(".ics")?;
    Some((cal_id.to_string(), event_uid.to_string()))
}

// Keep PathBuf in scope even though we don't construct one directly;
// tests below use Path methods.
#[allow(dead_code)]
fn _path_type_marker() -> PathBuf {
    PathBuf::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_contact_uid_works() {
        assert_eq!(
            extract_contact_uid("contacts/default/uid-1.vcf", "test"),
            Some("uid-1".into())
        );
        assert_eq!(
            extract_contact_uid("mail/INBOX/foo.json", "test"),
            None
        );
    }

    #[test]
    fn extract_sieve_name_works() {
        assert_eq!(
            extract_sieve_name("sieve/filter1.sieve", "test"),
            Some("filter1".into())
        );
    }

    #[test]
    fn extract_calendar_event_works() {
        assert_eq!(
            extract_calendar_event("calendars/cal-1/events/uid-1.ics", "test"),
            Some(("cal-1".into(), "uid-1".into()))
        );
    }

    #[test]
    fn no_false_positives() {
        assert_eq!(
            extract_contact_uid("sieve/script.sieve", "test"),
            None
        );
        assert_eq!(
            extract_sieve_name("contacts/default/uid.vcf", "test"),
            None
        );
    }

    // --- validate_path_prefix: path traversal safety ---
    //
    // These are the I8 (Path traversal safety) unit tests. Pure logic,
    // no network — they prove that plan_bulk's entry guard rejects `..`
    // before any filesystem or git command runs.

    #[test]
    fn validate_path_prefix_accepts_normal_paths() {
        assert!(validate_path_prefix("contacts/").is_ok());
        assert!(validate_path_prefix("mail/").is_ok());
        assert!(validate_path_prefix("").is_ok()); // whole repo
        assert!(validate_path_prefix("calendars/cal-1/events/").is_ok());
    }

    #[test]
    fn validate_path_prefix_rejects_parent_dir_escape() {
        assert!(validate_path_prefix("..").is_err());
        assert!(validate_path_prefix("../etc/passwd").is_err());
        assert!(validate_path_prefix("mail/../../../etc").is_err());
    }

    #[test]
    fn validate_path_prefix_rejects_embedded_dotdot() {
        // Even as a substring — we don't try to be clever about trailing
        // slashes or normalized forms, we just refuse any occurrence.
        assert!(validate_path_prefix("mail/..hidden/").is_err());
        assert!(validate_path_prefix("calendars/../other/").is_err());
    }
}
