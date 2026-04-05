//! Mail pull loop.
//!
//! Strategy for v1:
//!
//! 1. List all folders.
//! 2. For each folder, list message summaries (paginated).
//! 3. Diff against local cache: a message is "changed" if its `modseq`
//!    differs, or if it's new entirely. Messages missing from the remote
//!    list are deleted.
//! 4. For each changed message, fetch the full JSON and write to
//!    `mail/<folder_path>/<msg_id>.json`. The folder path is used (not the
//!    folder id) because it's stable and human-readable in git history.
//! 5. Atomic commit.
//!
//! This is a full-snapshot strategy, not true CONDSTORE delta sync. It works
//! because forwardemail's message list is cheap (the smoke test showed
//! sub-50ms for empty folders) and modseq lets us skip per-message GETs on
//! the common "no changes" path. Real CONDSTORE/modseq > N filtering can be
//! added later if it becomes expensive.

use crate::error::Error;
use crate::forwardemail::mail::{Folder, MessageSummary};
use crate::forwardemail::Client;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::store::Repo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Sidecar metadata written alongside the raw JSON. The raw JSON is the
/// source of truth; this struct exists so we can read the tiny meta file
/// to decide whether a full re-fetch is needed, without parsing the entire
/// message JSON on every pull.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageMeta {
    id: String,
    modseq: Option<i64>,
    updated_at: Option<String>,
    flags: Vec<String>,
}

pub async fn pull_mail(
    client: &Client,
    repo: &Repo,
    alias: &str,
    author_name: &str,
    author_email: &str,
) -> PullResult<PullSummary> {
    let mut summary = PullSummary {
        resource: "mail",
        ..Default::default()
    };

    // 1. List folders and write a folder manifest per folder. Folders are
    // cheap and their metadata (uid_validity, uid_next, special_use) is
    // worth tracking in git history.
    let folders = client.list_folders().await?;
    for f in &folders {
        let dir = format!(
            "sources/forwardemail/{}/mail/{}",
            alias,
            folder_safe(&f.path)
        );
        let meta_path = format!("{dir}/_folder.json");
        let body = serde_json::to_vec_pretty(f)?;
        repo.write_file(&meta_path, &body)?;
    }

    // 2. Per-folder message sync
    for f in &folders {
        let folder_dir = format!(
            "sources/forwardemail/{}/mail/{}",
            alias,
            folder_safe(&f.path)
        );
        let local_meta = read_local_message_meta(repo, &folder_dir)?;
        let remote_summaries = client.list_messages_in_folder(&f.path).await?;
        let remote_by_id: HashMap<String, &MessageSummary> =
            remote_summaries.iter().map(|m| (m.id.clone(), m)).collect();

        // 3. Detect changes and re-fetch full bodies where needed.
        for msg in &remote_summaries {
            let id = filename_safe(&msg.id);
            let prev = local_meta.get(&id);
            let needs_refetch = match prev {
                None => true,
                Some(p) => p.modseq != msg.modseq || p.flags != msg.flags,
            };

            if !needs_refetch {
                continue;
            }

            let full = client.get_message(&msg.id).await?;
            let raw_path = format!("{folder_dir}/{id}.json");
            repo.write_file(&raw_path, serde_json::to_vec_pretty(&full)?.as_slice())?;

            let meta = MessageMeta {
                id: msg.id.clone(),
                modseq: msg.modseq,
                updated_at: msg.updated_at.clone(),
                flags: msg.flags.clone(),
            };
            let meta_path = format!("{folder_dir}/{id}.meta.json");
            repo.write_file(&meta_path, serde_json::to_vec_pretty(&meta)?.as_slice())?;

            if prev.is_some() {
                summary.updated += 1;
            } else {
                summary.added += 1;
            }
        }

        // 4. Detect deletions.
        let remote_ids: HashSet<String> = remote_summaries
            .iter()
            .map(|m| filename_safe(&m.id))
            .collect();
        for local_id in local_meta.keys() {
            if !remote_ids.contains(local_id) {
                let _ =
                    std::fs::remove_file(repo.root().join(format!("{folder_dir}/{local_id}.json")));
                let _ = std::fs::remove_file(
                    repo.root()
                        .join(format!("{folder_dir}/{local_id}.meta.json")),
                );
                summary.deleted += 1;
            }
        }
        // Keep `remote_by_id` in scope until after the delete pass so the
        // compiler knows it's still alive (unused after here).
        drop(remote_by_id);
    }

    // Also detect folder removals: local folder dir exists but not in remote list
    cleanup_removed_folders(repo, alias, &folders)?;

    let msg = format!(
        "mail: +{} ~{} -{}",
        summary.added, summary.updated, summary.deleted
    );
    summary.commit_sha = repo.commit_all(author_name, author_email, &msg)?;
    Ok(summary)
}

fn read_local_message_meta(
    repo: &Repo,
    folder_dir: &str,
) -> Result<HashMap<String, MessageMeta>, Error> {
    let mut out = HashMap::new();
    let dir: PathBuf = repo.root().join(folder_dir);
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| Error::store(format!("readdir {}: {}", dir.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if !name.ends_with(".meta.json") || name == "_folder.json" {
            continue;
        }
        let stem = name.trim_end_matches(".meta.json").to_string();
        let bytes = std::fs::read(entry.path()).map_err(|e| Error::store(format!("read: {e}")))?;
        let meta: MessageMeta = serde_json::from_slice(&bytes)?;
        out.insert(stem, meta);
    }
    Ok(out)
}

fn cleanup_removed_folders(repo: &Repo, alias: &str, folders: &[Folder]) -> Result<(), Error> {
    let mail_root = repo
        .root()
        .join(format!("sources/forwardemail/{alias}/mail"));
    if !mail_root.exists() {
        return Ok(());
    }
    let current: HashSet<String> = folders.iter().map(|f| folder_safe(&f.path)).collect();
    for entry in std::fs::read_dir(&mail_root)
        .map_err(|e| Error::store(format!("readdir {}: {}", mail_root.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if !current.contains(&name) {
            // Remove the whole folder subtree
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

/// Convert a forwardemail folder path into a filesystem-safe directory name.
/// Sent Mail → "Sent Mail" (spaces are fine, git handles them); slashes
/// become underscores.
fn folder_safe(path: &str) -> String {
    path.replace('/', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_safe_escapes_slashes() {
        assert_eq!(folder_safe("INBOX"), "INBOX");
        assert_eq!(folder_safe("Sent Mail"), "Sent Mail");
        assert_eq!(folder_safe("Archive/2024"), "Archive_2024");
    }
}
