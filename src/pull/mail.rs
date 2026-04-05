//! Mail pull loop.
//!
//! Strategy:
//!
//! 1. List all folders.
//! 2. For each folder, list message summaries (paginated).
//! 3. Diff against local cache: a message is "changed" if its `modseq`
//!    differs, or if it's new entirely. Messages missing from the remote
//!    list are deleted.
//! 4. For each changed message, fetch the full response, extract the
//!    `raw` field (byte-identical RFC822), write it to
//!    `mail/<folder_path>/<msg_id>.eml` (write-once, immutable). Write the
//!    mutable metadata (flags, folder, modseq, uid, updated_at, etc.) to
//!    a sidecar `<msg_id>.meta.json`.
//! 5. Atomic commit.
//!
//! Historical note: earlier pimsteward versions stored the entire message
//! response including the parsed `nodemailer` field to a single
//! `<msg_id>.json`. The current layout is cleaner because the .eml is the
//! authoritative bytes and nodemailer can always be re-parsed from it.
//! The pull loop transparently migrates any legacy `.json` files it finds
//! on disk by extracting the `raw` field into a new `.eml` and replacing
//! the JSON with the trimmed meta form.
//!
//! This is a full-snapshot strategy, not true CONDSTORE delta sync. It works
//! because forwardemail's message list is cheap and modseq lets us skip
//! per-message GETs on the common "no changes" path. Real CONDSTORE/modseq
//! > N filtering via native IMAP is a v2.2 concern.

use crate::error::Error;
use crate::forwardemail::mail::{Folder, MessageSummary};
use crate::forwardemail::Client;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::store::Repo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Sidecar metadata written alongside the raw .eml. Captures the mutable
/// forwardemail-specific fields that aren't in the RFC822 itself: flags,
/// folder, modseq, uid, thread_id, labels, etc. The pull loop reads this
/// to decide whether a full re-fetch is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageMeta {
    id: String,
    #[serde(default)]
    folder_id: Option<String>,
    #[serde(default)]
    folder_path: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    uid: Option<i64>,
    #[serde(default)]
    modseq: Option<i64>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    internal_date: Option<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
}

impl MessageMeta {
    /// Build a MessageMeta from the list summary + full response JSON. Fields
    /// present in the summary are preferred because they're canonical; extra
    /// fields come from the full response.
    fn from_response(summary: &MessageSummary, full: &serde_json::Value) -> Self {
        fn string_at(v: &serde_json::Value, key: &str) -> Option<String> {
            v.get(key).and_then(|x| x.as_str()).map(String::from)
        }
        fn u64_at(v: &serde_json::Value, key: &str) -> Option<u64> {
            v.get(key).and_then(|x| x.as_u64())
        }
        fn string_array_at(v: &serde_json::Value, key: &str) -> Vec<String> {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        }

        Self {
            id: summary.id.clone(),
            folder_id: Some(summary.folder_id.clone()).filter(|s| !s.is_empty()),
            folder_path: string_at(full, "folder_path"),
            thread_id: string_at(full, "thread_id"),
            uid: summary.uid,
            modseq: summary.modseq,
            size: Some(summary.size)
                .filter(|s| *s > 0)
                .or_else(|| u64_at(full, "size")),
            updated_at: summary.updated_at.clone(),
            internal_date: string_at(full, "internal_date"),
            flags: if summary.flags.is_empty() {
                string_array_at(full, "flags")
            } else {
                summary.flags.clone()
            },
            labels: string_array_at(full, "labels"),
        }
    }
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

            // Fetch the full response, extract the raw RFC822 bytes.
            // Forwardemail returns `raw` as a string in the JSON by default;
            // see docs/api-findings.md.
            let full = client.get_message(&msg.id).await?;
            let raw_bytes = full
                .get("raw")
                .and_then(|v| v.as_str())
                .map(|s| s.as_bytes().to_vec())
                .ok_or_else(|| {
                    Error::store(format!(
                        "forwardemail response for message {} missing `raw` field",
                        msg.id
                    ))
                })?;

            let eml_path = format!("{folder_dir}/{id}.eml");
            repo.write_file(&eml_path, &raw_bytes)?;

            // Remove any legacy .json sidecar left over from earlier versions.
            let legacy_json = repo.root().join(format!("{folder_dir}/{id}.json"));
            let _ = std::fs::remove_file(legacy_json);

            let meta = MessageMeta::from_response(msg, &full);
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
                    std::fs::remove_file(repo.root().join(format!("{folder_dir}/{local_id}.eml")));
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
