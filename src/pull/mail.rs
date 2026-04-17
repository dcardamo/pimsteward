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
//!    `mail/<folder>/<msg_id>.eml` (write-once, immutable). Write the
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
use crate::forwardemail::mail::Folder;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::source::MailSource;
use crate::store::Repo;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Sidecar metadata written alongside the raw .eml. Captures the mutable
/// forwardemail-specific fields that aren't in the RFC822 itself: flags,
/// folder, modseq, uid, thread_id, labels, etc. The pull loop reads this
/// to decide whether a full re-fetch is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MessageMeta {
    /// Source-specific id: REST ObjectId or `imap-<uid>`. Used for API
    /// calls (MailWriter). Not the filename — see `canonical_id`.
    pub id: String,
    /// Canonical identifier: `sha256(Message-ID header)[..16]`. This is
    /// the filename stem for .eml, .meta.json, .attachments.json. Source-
    /// agnostic so switching between REST and IMAP backends against the
    /// same backup tree preserves file identity.
    #[serde(default)]
    pub canonical_id: Option<String>,
    #[serde(default)]
    folder_id: Option<String>,
    #[serde(default)]
    pub folder_path: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub uid: Option<i64>,
    #[serde(default)]
    pub modseq: Option<i64>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub internal_date: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// One entry in `<id>.attachments.json`. The raw bytes live at
/// `_attachments/<sha256>` so multiple messages referencing the same blob
/// (common for forwarded chains) share a single copy on disk. The .eml is
/// left untouched — clients that want the original attachment bytes can
/// still re-parse the MIME from the .eml, but tooling can also load
/// `_attachments/<sha256>` directly without MIME walking.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentRef {
    sha256: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_disposition: Option<String>,
    /// True if the attachment is `inline` (e.g. embedded HTML image).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    inline: bool,
}

impl MessageMeta {
    /// Build a MessageMeta from the fetched message. The summary carries
    /// canonical diff keys (id, modseq, flags, uid). The optional `extra`
    /// JSON (populated only by REST source) fills in fields that forwardemail
    /// exposes but IMAP doesn't (thread_id, labels, folder_path, internal_date).
    fn from_fetched(fetched: &crate::source::FetchedMessage) -> Self {
        let s = &fetched.summary;
        let extra = fetched.extra.as_ref();

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
            id: s.id.clone(),
            canonical_id: None, // Set by caller after derive_canonical_id
            folder_id: Some(s.folder_id.clone()).filter(|v| !v.is_empty()),
            folder_path: extra
                .and_then(|e| string_at(e, "folder_path"))
                .or_else(|| Some(s.folder_path.clone()).filter(|v| !v.is_empty())),
            thread_id: extra.and_then(|e| string_at(e, "thread_id")),
            uid: s.uid,
            modseq: s.modseq,
            size: Some(s.size)
                .filter(|v| *v > 0)
                .or_else(|| extra.and_then(|e| u64_at(e, "size"))),
            updated_at: s.updated_at.clone(),
            internal_date: extra.and_then(|e| string_at(e, "internal_date")),
            flags: if s.flags.is_empty() {
                extra
                    .map(|e| string_array_at(e, "flags"))
                    .unwrap_or_default()
            } else {
                s.flags.clone()
            },
            labels: extra
                .map(|e| string_array_at(e, "labels"))
                .unwrap_or_default(),
        }
    }
}

pub async fn pull_mail(
    source: &dyn MailSource,
    repo: &Repo,
    _alias: &str,
    author_name: &str,
    author_email: &str,
) -> PullResult<PullSummary> {
    let mut summary = PullSummary {
        resource: "mail",
        ..Default::default()
    };

    // 1. List folders. We defer writing `_folder.json` until after
    //    list_messages runs so we can fold in the CONDSTORE values
    //    (uid_validity, highest_modseq) the source returns.
    let folders = source.list_folders().await?;

    // 2. Per-folder message sync. Each folder commits independently so
    //    large mailboxes make incremental progress — a crash mid-sync
    //    loses at most one folder's work, and git history records each
    //    folder's state as it's captured.
    for f in &folders {
        sync_one_folder(source, repo, f, author_name, author_email, &mut summary).await?;
    }

    // Detect folder removals and commit if anything was cleaned up.
    cleanup_removed_folders(repo, &folders)?;
    // Final commit captures folder removals + any stragglers. If all
    // folders already committed their changes above, this is a no-op.
    let msg = format!(
        "mail: +{} ~{} -{}",
        summary.added, summary.updated, summary.deleted
    );
    // Only overwrite if the final commit actually produced a new sha
    // (folder removals or other stragglers). Per-folder shas are preserved.
    if let Some(sha) = repo.commit_all(author_name, author_email, &msg)? {
        summary.commit_sha = Some(sha);
    }
    Ok(summary)
}

/// Pull state for only the specified folders.
///
/// Used by the write path (`write::mail::refresh`) so that a single
/// `create_draft`/`move_email`/etc. MCP call only re-syncs the folders
/// it actually affected instead of the entire mailbox. The background
/// puller still uses [`pull_mail`] for its full-mailbox passes.
///
/// Calls `source.list_folders()` once to get fresh metadata for the
/// requested folders (for `_folder.json` special_use / created_at /
/// updated_at fields). Unknown folder names are silently skipped — the
/// caller always passes names derived from its own tool arguments, so
/// a missing folder means the user passed a bad path, which is a
/// different error surface that the write itself will have already
/// reported.
///
/// Intentionally does NOT call [`cleanup_removed_folders`] — that's a
/// whole-mailbox-scope operation and would be unsafe to run against a
/// filtered folder list.
pub async fn sync_folders(
    source: &dyn MailSource,
    repo: &Repo,
    _alias: &str,
    author_name: &str,
    author_email: &str,
    folder_paths: &[&str],
) -> PullResult<PullSummary> {
    let mut summary = PullSummary {
        resource: "mail",
        ..Default::default()
    };
    if folder_paths.is_empty() {
        return Ok(summary);
    }

    // Fetch full folder metadata once so we can populate _folder.json
    // with special_use / created_at / updated_at fields (same shape the
    // background puller writes).
    let all_folders = source.list_folders().await?;
    let wanted: HashSet<&str> = folder_paths.iter().copied().collect();

    for f in all_folders.iter().filter(|f| wanted.contains(f.path.as_str())) {
        sync_one_folder(source, repo, f, author_name, author_email, &mut summary).await?;
    }

    Ok(summary)
}

/// Sync a single folder into the repo: list messages, fetch any that
/// changed, detect local deletions, update `_folder.json`, and commit
/// the folder's changes under the supplied author. Shared between
/// [`pull_mail`] (whole-mailbox passes) and [`sync_folders`] (write-
/// path refreshes).
async fn sync_one_folder(
    source: &dyn MailSource,
    repo: &Repo,
    f: &Folder,
    author_name: &str,
    author_email: &str,
    summary: &mut PullSummary,
) -> PullResult<()> {
    let folder_dir = format!("mail/{}", folder_safe(&f.path));
    let local_meta = read_local_message_meta(repo, &folder_dir)?;
    let mut folder_added = 0usize;
    let mut folder_updated = 0usize;
    let mut folder_deleted = 0usize;

    // Build a reverse index: source-specific id → canonical filename
    // stem. This lets us match remote summaries (keyed by source id)
    // to local files (keyed by canonical id).
    let source_to_canonical: HashMap<String, String> = local_meta
        .iter()
        .map(|(canonical, meta)| (filename_safe(&meta.id), canonical.clone()))
        .collect();

    // Read the previous _folder.json (if any) to recover the last
    // observed uid_validity + modify_index. These drive CONDSTORE
    // delta sync on sources that support it. If uid_validity has
    // changed under us, the source will ignore the modseq hint.
    let (prev_uid_validity, prev_modseq) = read_prev_folder_state(repo, &folder_dir);
    let list_result = source
        .list_messages(&f.path, prev_modseq, prev_uid_validity)
        .await?;

    // 3. Detect changes and re-fetch full bodies where needed.
    for msg in &list_result.changed {
        let source_id = filename_safe(&msg.id);
        let prev_canonical = source_to_canonical.get(&source_id);
        let prev = prev_canonical.and_then(|c| local_meta.get(c));
        let needs_refetch = match prev {
            None => true,
            Some(p) => p.modseq != msg.modseq || p.flags != msg.flags,
        };

        if !needs_refetch {
            continue;
        }

        // Source-agnostic fetch: REST extracts `raw` from the JSON
        // response; IMAP uses `UID FETCH BODY[]`. Both return raw
        // RFC822 bytes in FetchedMessage.raw.
        let fetched = source.fetch_message(&f.path, &msg.id).await?;
        let canonical = derive_canonical_id(&fetched);

        let eml_path = format!("{folder_dir}/{canonical}.eml");
        repo.write_file(&eml_path, &fetched.raw)?;

        // Remove any legacy file using the old source-specific id as
        // filename stem (migration from pre-canonical naming).
        if prev_canonical.is_none() {
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{source_id}.eml")),
            );
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{source_id}.meta.json")),
            );
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{source_id}.json")),
            );
            let _ = std::fs::remove_file(
                repo.root()
                    .join(format!("{folder_dir}/{source_id}.attachments.json")),
            );
        }

        let mut meta = MessageMeta::from_fetched(&fetched);
        meta.canonical_id = Some(canonical.clone());
        let meta_path = format!("{folder_dir}/{canonical}.meta.json");
        repo.write_file(&meta_path, serde_json::to_vec_pretty(&meta)?.as_slice())?;

        // Attachment index: parse nodemailer.attachments[], write each
        // blob to _attachments/<sha256>, and write the reference list as
        // a sidecar. REST-only — IMAP's `extra` is None.
        let attachments_sidecar = format!("{folder_dir}/{canonical}.attachments.json");
        let attachments_sidecar_fs = repo.root().join(&attachments_sidecar);
        match extract_attachments(&fetched, repo)? {
            Some(refs) if !refs.is_empty() => {
                repo.write_file(
                    &attachments_sidecar,
                    serde_json::to_vec_pretty(&refs)?.as_slice(),
                )?;
            }
            _ => {
                let _ = std::fs::remove_file(attachments_sidecar_fs);
            }
        }

        if prev.is_some() {
            folder_updated += 1;
        } else {
            folder_added += 1;
        }
    }

    // 4. Detect deletions. Build the set of source ids currently on
    //    the remote, then find local entries whose source id is no
    //    longer present.
    let remote_source_ids: HashSet<String> = list_result
        .all_ids
        .iter()
        .map(|id| filename_safe(id))
        .collect();
    for (canonical, meta) in &local_meta {
        let source_id = filename_safe(&meta.id);
        if !remote_source_ids.contains(&source_id) {
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{canonical}.eml")),
            );
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{canonical}.json")),
            );
            let _ = std::fs::remove_file(
                repo.root().join(format!("{folder_dir}/{canonical}.meta.json")),
            );
            let _ = std::fs::remove_file(
                repo.root()
                    .join(format!("{folder_dir}/{canonical}.attachments.json")),
            );
            folder_deleted += 1;
        }
    }
    // 5. Write/update _folder.json with the latest CONDSTORE state so
    //    the next pull can pass a stable since_modseq hint.
    let mut folder_meta = f.clone();
    if list_result.highest_modseq.is_some() {
        folder_meta.modify_index = list_result.highest_modseq;
    }
    if list_result.uid_validity.is_some() {
        folder_meta.uid_validity = list_result.uid_validity;
    }
    let folder_manifest_path = format!("{folder_dir}/_folder.json");
    repo.write_file(
        &folder_manifest_path,
        serde_json::to_vec_pretty(&folder_meta)?.as_slice(),
    )?;

    // 6. Commit this folder's changes immediately. On a large initial
    //    sync this means each folder is a separate git commit —
    //    partial progress is preserved across crashes and the git
    //    history shows per-folder snapshots.
    summary.added += folder_added;
    summary.updated += folder_updated;
    summary.deleted += folder_deleted;
    if folder_added > 0 || folder_updated > 0 || folder_deleted > 0 {
        let folder_msg = format!(
            "mail/{}: +{} ~{} -{}",
            folder_safe(&f.path),
            folder_added,
            folder_updated,
            folder_deleted
        );
        if let Some(sha) = repo.commit_all(author_name, author_email, &folder_msg)? {
            summary.commit_sha = Some(sha);
        }
    }
    Ok(())
}

/// Load the previously written `_folder.json` (if any) and extract the
/// `(uid_validity, modify_index)` pair. Used to feed CONDSTORE hints back
/// into the source on the next pull. Returns `(None, None)` if the file
/// doesn't exist or is malformed — the source will just do a full fetch.
fn read_prev_folder_state(repo: &Repo, folder_dir: &str) -> (Option<i64>, Option<i64>) {
    let path = repo.root().join(format!("{folder_dir}/_folder.json"));
    let Ok(bytes) = std::fs::read(&path) else {
        return (None, None);
    };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };
    let uid_validity = val.get("uid_validity").and_then(|v| v.as_i64());
    let modify_index = val.get("modify_index").and_then(|v| v.as_i64());
    (uid_validity, modify_index)
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

/// Maximum fraction of on-disk folders that a single pull is allowed
/// to remove. If the current server-reported folder list would cause
/// us to delete more than this fraction of what's on disk, the cleanup
/// is refused and logged loudly — the input is treated as a transient
/// server glitch rather than a legitimate rename/delete.
///
/// Chosen at 0.5 (50%) so that a user can still delete half their
/// folders in a single manual action if they really want to, while
/// a bogus "server returned only INBOX" skeleton list against a
/// 20-folder account (which would want to remove 19/20 = 95%) is
/// blocked.
const MAX_CLEANUP_REMOVAL_FRACTION: f64 = 0.5;

/// Delete folders from the backup tree that are no longer on the
/// server. Guarded heavily against transient IMAP glitches that would
/// otherwise nuke large portions of the archive — see the Apr 11 2026
/// incident where forwardemail's IMAP server restart caused
/// `list_folders()` to return a sparse list mid-recovery and this
/// routine deleted every Archive/YYYY folder (2005–2020+) under a
/// `mail: +0 ~0 -0` commit message. The deletion was committed and the
/// puller then spent hours re-syncing everything from scratch.
///
/// Safeguards, in order:
///   1. Empty folder list → refuse. An active IMAP account always has
///      at least INBOX; an empty result is definitively bogus.
///   2. Removal would exceed [`MAX_CLEANUP_REMOVAL_FRACTION`] of the
///      folders currently on disk → refuse. Real user actions touch
///      one folder at a time; catastrophic server state returns a
///      skeleton list that would remove most of them.
///   3. Otherwise, remove the unexpected folders as before.
///
/// Refusal is NOT an error — it's logged at WARN and the pull
/// continues normally. A refusal today does not prevent cleanup from
/// running next pull with a correct folder list.
fn cleanup_removed_folders(repo: &Repo, folders: &[Folder]) -> Result<(), Error> {
    let mail_root = repo.root().join("mail");
    if !mail_root.exists() {
        return Ok(());
    }

    // Safeguard #1: never act on an empty folder list.
    if folders.is_empty() {
        tracing::warn!(
            "cleanup_removed_folders: list_folders returned empty set, \
             refusing to delete anything (likely transient IMAP state)"
        );
        return Ok(());
    }

    let current: HashSet<String> = folders.iter().map(|f| folder_safe(&f.path)).collect();

    // Enumerate on-disk folders first (and stash them) so we can make
    // the removal-fraction decision before mutating anything.
    let mut on_disk: Vec<PathBuf> = Vec::new();
    let mut candidates_for_removal: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&mail_root)
        .map_err(|e| Error::store(format!("readdir {}: {}", mail_root.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let name = entry.file_name().into_string().unwrap_or_default();
        // `_attachments` is the content-addressed blob store, not a folder;
        // never treat it as a stale folder even though it isn't in the
        // remote folder list.
        if name == "_attachments" {
            continue;
        }
        on_disk.push(entry.path());
        if !current.contains(&name) {
            candidates_for_removal.push(entry.path());
        }
    }

    // Safeguard #2: refuse a mass-removal that looks like a transient
    // glitch.
    if !on_disk.is_empty() && !candidates_for_removal.is_empty() {
        let removal_fraction =
            candidates_for_removal.len() as f64 / on_disk.len() as f64;
        if removal_fraction > MAX_CLEANUP_REMOVAL_FRACTION {
            tracing::warn!(
                on_disk = on_disk.len(),
                candidates = candidates_for_removal.len(),
                removal_fraction = removal_fraction,
                max_allowed = MAX_CLEANUP_REMOVAL_FRACTION,
                "cleanup_removed_folders: refusing mass folder deletion \
                 (remote folder list likely transient). Folders preserved; \
                 cleanup will re-evaluate on the next pull with a fresh \
                 list_folders result."
            );
            return Ok(());
        }
    }

    for path in candidates_for_removal {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(())
}

/// Derive a canonical message identifier from the RFC822 Message-ID
/// header. Source-agnostic: the same email produces the same canonical
/// ID regardless of whether it was pulled via REST or IMAP.
///
/// Returns a 16-hex-char (8-byte) sha256 prefix. At typical mailbox
/// sizes (<100k messages) the collision probability is negligible.
///
/// Falls back to hashing the source-specific id if no Message-ID header
/// is available (drafts, broken mailers).
fn derive_canonical_id(fetched: &crate::source::FetchedMessage) -> String {
    // Try REST's parsed header_message_id first.
    if let Some(mid) = fetched
        .extra
        .as_ref()
        .and_then(|e| e.get("header_message_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return hash_to_canonical(mid);
    }
    // Parse from raw RFC822 bytes (IMAP path, or REST fallback).
    if let Some(mid) = extract_message_id_header(&fetched.raw) {
        return hash_to_canonical(&mid);
    }
    // Last resort: hash the source-specific id.
    hash_to_canonical(&fetched.summary.id)
}

pub(crate) fn hash_to_canonical(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    // 16 hex chars = 8 bytes. Unique enough for mailbox-scale collections.
    format!("{:x}", hash)[..16].to_string()
}

/// Extract the Message-ID header value from raw RFC822 bytes. Handles
/// folded headers (continuation lines starting with whitespace). Returns
/// the angle-bracketed value (e.g. `<abc@example.com>`) trimmed.
fn extract_message_id_header(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut in_header = false;
    let mut value = String::new();
    for line in text.lines() {
        // Blank line = end of headers.
        if line.is_empty() {
            break;
        }
        let lower = line.get(..11).map(|s| s.to_ascii_lowercase());
        if lower.as_deref() == Some("message-id:") {
            value = line.split_once(':')?.1.trim().to_string();
            in_header = true;
        } else if in_header && (line.starts_with(' ') || line.starts_with('\t')) {
            // Folded header continuation.
            value.push_str(line.trim());
        } else {
            in_header = false;
        }
    }
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Pull `nodemailer.attachments[]` out of a fetched REST message, write each
/// distinct blob to `_attachments/<sha256>` under the alias's mail root, and
/// return the list of references to embed in the sidecar. Returns `Ok(None)`
/// if the source didn't provide a parsed `nodemailer` field (e.g. IMAP). The
/// blob directory is per-alias so blobs are scoped the same way as the rest
/// of the pulled state.
fn extract_attachments(
    fetched: &crate::source::FetchedMessage,
    repo: &Repo,
) -> Result<Option<Vec<AttachmentRef>>, Error> {
    let Some(extra) = fetched.extra.as_ref() else {
        return Ok(None);
    };
    let Some(atts) = extra
        .get("nodemailer")
        .and_then(|n| n.get("attachments"))
        .and_then(|a| a.as_array())
    else {
        return Ok(Some(Vec::new()));
    };

    let b64 = base64::engine::general_purpose::STANDARD;
    let mut refs = Vec::with_capacity(atts.len());

    for att in atts {
        // Nodemailer encodes `content` as a base64 string. Skip entries that
        // don't have one — forwardemail occasionally returns structural-only
        // parts (e.g. message/rfc822 wrappers) with no raw body.
        let Some(content_b64) = att.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let bytes = b64
            .decode(content_b64.as_bytes())
            .map_err(|e| Error::store(format!("attachment base64 decode: {e}")))?;

        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let sha = format!("{:x}", hasher.finalize());

        let blob_path = format!("mail/_attachments/{sha}");
        // Write-once: if a blob with the same content hash already exists on
        // disk the write is a no-op, which keeps the repo churn-free.
        if !repo.root().join(&blob_path).exists() {
            repo.write_file(&blob_path, &bytes)?;
        }

        refs.push(AttachmentRef {
            sha256: sha,
            size: bytes.len() as u64,
            filename: att
                .get("filename")
                .and_then(|v| v.as_str())
                .map(String::from),
            content_type: att
                .get("contentType")
                .and_then(|v| v.as_str())
                .map(String::from),
            content_id: att
                .get("contentId")
                .and_then(|v| v.as_str())
                .map(String::from),
            content_disposition: att
                .get("contentDisposition")
                .and_then(|v| v.as_str())
                .map(String::from),
            inline: att
                .get("contentDisposition")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("inline"))
                .unwrap_or(false),
        });
    }

    Ok(Some(refs))
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

    use crate::forwardemail::mail::MessageSummary;
    use crate::source::FetchedMessage;
    use crate::store::Repo;

    fn blank_summary() -> MessageSummary {
        MessageSummary {
            id: "m1".into(),
            folder_id: "f1".into(),
            folder_path: "INBOX".into(),
            subject: String::new(),
            size: 0,
            uid: None,
            modseq: None,
            updated_at: None,
            flags: Vec::new(),
        }
    }

    #[test]
    fn extract_attachments_writes_blob_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();

        // base64("hello world") = "aGVsbG8gd29ybGQ="
        let extra = serde_json::json!({
            "nodemailer": {
                "attachments": [
                    {
                        "filename": "a.txt",
                        "contentType": "text/plain",
                        "contentDisposition": "attachment",
                        "content": "aGVsbG8gd29ybGQ="
                    },
                    {
                        "filename": "b.txt",
                        "contentType": "text/plain",
                        "contentDisposition": "inline",
                        "contentId": "<cid@x>",
                        "content": "aGVsbG8gd29ybGQ="
                    }
                ]
            }
        });
        let fetched = FetchedMessage {
            summary: blank_summary(),
            raw: b"".to_vec(),
            extra: Some(extra),
        };

        let refs = extract_attachments(&fetched, &repo)
            .unwrap()
            .unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].sha256, refs[1].sha256, "same bytes → same sha");
        assert_eq!(refs[0].size, 11);
        assert_eq!(refs[0].filename.as_deref(), Some("a.txt"));
        assert!(!refs[0].inline);
        assert!(refs[1].inline);
        assert_eq!(refs[1].content_id.as_deref(), Some("<cid@x>"));

        let blob = repo
            .root()
            .join(format!("mail/_attachments/{}", refs[0].sha256));
        assert!(blob.exists());
        assert_eq!(std::fs::read(&blob).unwrap(), b"hello world");
    }

    #[test]
    fn extract_attachments_no_extra_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let fetched = FetchedMessage {
            summary: blank_summary(),
            raw: b"".to_vec(),
            extra: None,
        };
        assert!(extract_attachments(&fetched, &repo)
            .unwrap()
            .is_none());
    }

    #[test]
    fn extract_attachments_missing_nodemailer_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let fetched = FetchedMessage {
            summary: blank_summary(),
            raw: b"".to_vec(),
            extra: Some(serde_json::json!({})),
        };
        let refs = extract_attachments(&fetched, &repo)
            .unwrap()
            .unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn read_prev_folder_state_reads_uid_validity_and_modify_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let folder_dir = "mail/INBOX";
        let body = serde_json::json!({
            "id": "INBOX",
            "path": "INBOX",
            "name": "INBOX",
            "uid_validity": 42,
            "modify_index": 9001,
            "subscribed": true
        });
        repo.write_file(
            format!("{folder_dir}/_folder.json"),
            serde_json::to_vec(&body).unwrap().as_slice(),
        )
        .unwrap();

        let (uv, mi) = read_prev_folder_state(&repo, folder_dir);
        assert_eq!(uv, Some(42));
        assert_eq!(mi, Some(9001));
    }

    #[test]
    fn read_prev_folder_state_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let (uv, mi) = read_prev_folder_state(&repo, "mail/INBOX");
        assert_eq!((uv, mi), (None, None));
    }

    // ── cleanup_removed_folders safety guards ────────────────────────
    //
    // Regression tests for the Apr 11 2026 "nuke every Archive folder
    // on a transient IMAP glitch" incident. The whole set is tagged
    // with what it's guarding against — future refactorers please
    // don't delete these lightly.

    fn seed_folder(repo: &Repo, name: &str) {
        repo.write_file(
            format!("mail/{name}/_folder.json"),
            br#"{"id":"placeholder"}"#,
        )
        .unwrap();
    }

    fn folder_for(name: &str) -> Folder {
        Folder {
            id: name.to_string(),
            path: name.to_string(),
            name: name.to_string(),
            uid_validity: None,
            uid_next: None,
            modify_index: None,
            subscribed: true,
            special_use: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn folder_exists_on_disk(repo: &Repo, safe_name: &str) -> bool {
        repo.root().join("mail").join(safe_name).exists()
    }

    #[test]
    fn cleanup_refuses_empty_folder_list() {
        // Scenario: list_folders() returned Ok(vec![]) because
        // forwardemail's IMAP server was still coming back up from a
        // restart. Without the guard, this used to wipe every folder
        // on disk. With the guard, everything is preserved and a
        // warning is logged.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        seed_folder(&repo, "INBOX");
        seed_folder(&repo, "Archive_2010");
        seed_folder(&repo, "Archive_2011");

        cleanup_removed_folders(&repo, &[]).unwrap();

        assert!(folder_exists_on_disk(&repo, "INBOX"));
        assert!(folder_exists_on_disk(&repo, "Archive_2010"));
        assert!(folder_exists_on_disk(&repo, "Archive_2011"));
    }

    #[test]
    fn cleanup_refuses_mass_removal_above_threshold() {
        // Scenario: 20 folders on disk, list_folders returned only
        // INBOX. That would remove 19/20 = 95% of the archive — way
        // above the 50% threshold. Refuse.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        for y in 2005..=2019 {
            seed_folder(&repo, &format!("Archive_{y}"));
        }
        seed_folder(&repo, "INBOX");
        seed_folder(&repo, "Drafts");
        seed_folder(&repo, "Sent");
        seed_folder(&repo, "Spam");
        seed_folder(&repo, "Trash");

        // Server only reports INBOX — the rest should look "missing"
        // but the guard refuses to act.
        cleanup_removed_folders(&repo, &[folder_for("INBOX")]).unwrap();

        for y in 2005..=2019 {
            assert!(
                folder_exists_on_disk(&repo, &format!("Archive_{y}")),
                "Archive_{y} should have survived the refused mass-removal"
            );
        }
        assert!(folder_exists_on_disk(&repo, "Drafts"));
        assert!(folder_exists_on_disk(&repo, "Sent"));
    }

    #[test]
    fn cleanup_allows_small_removal() {
        // Scenario: user actually deleted one folder. Removal of 1/20
        // is 5%, well under the 50% threshold → cleanup proceeds.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        for y in 2005..=2019 {
            seed_folder(&repo, &format!("Archive_{y}"));
        }
        seed_folder(&repo, "INBOX");
        seed_folder(&repo, "Drafts");
        seed_folder(&repo, "Old_Project");
        seed_folder(&repo, "Sent");
        seed_folder(&repo, "Spam");

        // Server reports everything except Old_Project.
        let mut folders: Vec<Folder> = (2005..=2019)
            .map(|y| folder_for(&format!("Archive/{y}")))
            .collect();
        folders.push(folder_for("INBOX"));
        folders.push(folder_for("Drafts"));
        folders.push(folder_for("Sent"));
        folders.push(folder_for("Spam"));

        cleanup_removed_folders(&repo, &folders).unwrap();

        assert!(!folder_exists_on_disk(&repo, "Old_Project"),
                "single-folder removal should have been allowed");
        // Everything else preserved.
        for y in 2005..=2019 {
            assert!(folder_exists_on_disk(&repo, &format!("Archive_{y}")));
        }
        assert!(folder_exists_on_disk(&repo, "INBOX"));
    }

    #[test]
    fn cleanup_refuses_at_exactly_over_threshold() {
        // Boundary test: 10 folders on disk, removing 6 = 60% > 50%
        // threshold → refuse. Removing 5 = 50% is NOT strictly above
        // the threshold and would be allowed (sibling test below).
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        for i in 0..10 {
            seed_folder(&repo, &format!("Folder_{i}"));
        }
        // Server reports only 4 → would remove 6/10 = 60%.
        let folders: Vec<Folder> = (0..4).map(|i| folder_for(&format!("Folder/{i}"))).collect();

        cleanup_removed_folders(&repo, &folders).unwrap();

        // All 10 preserved — the 60% removal was refused.
        for i in 0..10 {
            assert!(
                folder_exists_on_disk(&repo, &format!("Folder_{i}")),
                "Folder_{i} should have survived (removal fraction exceeds threshold)"
            );
        }
    }

    #[test]
    fn cleanup_allows_at_exactly_threshold() {
        // Boundary test: 10 folders on disk, removing 5 = 50% which
        // is NOT strictly greater than MAX_CLEANUP_REMOVAL_FRACTION
        // (0.5), so cleanup proceeds. This is a deliberate choice:
        // if a user really wants to delete half their folders in one
        // sweep, allow it. Anything more extreme looks like a bug.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        for i in 0..10 {
            seed_folder(&repo, &format!("Folder_{i}"));
        }
        let folders: Vec<Folder> = (0..5).map(|i| folder_for(&format!("Folder/{i}"))).collect();

        cleanup_removed_folders(&repo, &folders).unwrap();

        // Folder_0..4 preserved, Folder_5..9 removed.
        for i in 0..5 {
            assert!(folder_exists_on_disk(&repo, &format!("Folder_{i}")));
        }
        for i in 5..10 {
            assert!(!folder_exists_on_disk(&repo, &format!("Folder_{i}")));
        }
    }

    #[test]
    fn cleanup_preserves_attachments_directory() {
        // _attachments is the content-addressed blob store, not a
        // folder. It must never be touched by cleanup regardless of
        // what list_folders returned. (This was already handled by
        // the original code; the test pins the behavior so the
        // guard refactor didn't regress it.)
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        repo.write_file("mail/_attachments/some_blob", b"data").unwrap();
        seed_folder(&repo, "INBOX");

        cleanup_removed_folders(&repo, &[folder_for("INBOX")]).unwrap();

        assert!(folder_exists_on_disk(&repo, "_attachments"));
        assert!(folder_exists_on_disk(&repo, "INBOX"));
    }

    #[test]
    fn cleanup_no_mail_root_is_ok() {
        // Fresh repo with no mail/ subdir yet — nothing to clean up.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        cleanup_removed_folders(&repo, &[folder_for("INBOX")]).unwrap();
    }

    // ── sync_folders only touches the requested folders ────────────
    //
    // Regression tests for the Apr 11 2026 "writes block on full
    // mailbox resync" bug. The old refresh() called pull_mail which
    // iterated every folder in the mailbox, so a single create_draft
    // call blocked for minutes during an initial-sync backlog while
    // list_messages/fetch_message churned on Archive/2013/2015/2016.
    // sync_folders must only call list_messages on the exact paths
    // requested — never on unrelated folders.

    use crate::source::traits::{ListResult, MailSource};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CountingSource {
        /// Every folder path passed to list_messages, in call order.
        list_messages_calls: Mutex<Vec<String>>,
        /// Every folder path passed to fetch_message.
        fetch_message_calls: Mutex<Vec<String>>,
        /// What list_folders should return.
        folders: Vec<Folder>,
    }

    #[async_trait::async_trait]
    impl MailSource for CountingSource {
        fn tag(&self) -> &'static str {
            "counting"
        }
        async fn list_folders(&self) -> Result<Vec<Folder>, Error> {
            Ok(self.folders.clone())
        }
        async fn list_messages(
            &self,
            folder: &str,
            _since: Option<i64>,
            _uv: Option<i64>,
        ) -> Result<ListResult, Error> {
            self.list_messages_calls
                .lock()
                .unwrap()
                .push(folder.to_string());
            // Return empty — no messages to refetch.
            Ok(ListResult::default())
        }
        async fn fetch_message(
            &self,
            folder: &str,
            _id: &str,
        ) -> Result<FetchedMessage, Error> {
            self.fetch_message_calls
                .lock()
                .unwrap()
                .push(folder.to_string());
            Err(Error::store("fetch_message should not be called in these tests"))
        }
    }

    fn folder_for_sync(path: &str) -> Folder {
        let mut f = folder_for(path);
        f.path = path.to_string();
        f
    }

    #[tokio::test]
    async fn sync_folders_only_touches_requested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();

        let source = CountingSource {
            folders: vec![
                folder_for_sync("INBOX"),
                folder_for_sync("Drafts"),
                folder_for_sync("Junk"),
                folder_for_sync("Archive/2013"),
                folder_for_sync("Archive/2014"),
                folder_for_sync("Archive/2015"),
            ],
            ..Default::default()
        };

        // Caller asks for Drafts only — simulating create_draft.
        sync_folders(&source, &repo, "dan", "test", "test@example.com", &["Drafts"])
            .await
            .unwrap();

        let calls = source.list_messages_calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["Drafts".to_string()],
            "sync_folders must only call list_messages on the requested \
             folder; got {calls:?}. A regression here means writes would \
             again block on Archive/2013+ during an initial-sync backlog."
        );
    }

    #[tokio::test]
    async fn sync_folders_handles_multi_folder_move() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();

        let source = CountingSource {
            folders: vec![
                folder_for_sync("INBOX"),
                folder_for_sync("Junk"),
                folder_for_sync("Archive/2014"),
            ],
            ..Default::default()
        };

        // move_message refreshes both source_folder and target_folder.
        sync_folders(
            &source,
            &repo,
            "dan",
            "test",
            "test@example.com",
            &["INBOX", "Junk"],
        )
        .await
        .unwrap();

        let mut calls = source.list_messages_calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(
            calls,
            vec!["INBOX".to_string(), "Junk".to_string()],
            "sync_folders should hit both INBOX and Junk for a cross-folder move"
        );
    }

    #[tokio::test]
    async fn sync_folders_empty_list_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let source = CountingSource {
            folders: vec![folder_for_sync("INBOX")],
            ..Default::default()
        };
        sync_folders(&source, &repo, "dan", "test", "test@example.com", &[])
            .await
            .unwrap();
        // Empty input: list_folders should not even be called, and no
        // list_messages either.
        let calls = source.list_messages_calls.lock().unwrap().clone();
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn sync_folders_silently_skips_unknown_paths() {
        // If a caller passes a folder name that doesn't exist on the
        // server, sync_folders must not error — we just skip it. This
        // matches the send_email case where we try ["Sent Mail",
        // "Sent"] and expect whichever one exists to match.
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let source = CountingSource {
            folders: vec![folder_for_sync("Sent Mail"), folder_for_sync("INBOX")],
            ..Default::default()
        };
        sync_folders(
            &source,
            &repo,
            "dan",
            "test",
            "test@example.com",
            &["Sent Mail", "Sent", "Outbox"],
        )
        .await
        .unwrap();
        let calls = source.list_messages_calls.lock().unwrap().clone();
        // Only "Sent Mail" exists on the server; "Sent" and "Outbox"
        // don't — no list_messages for those.
        assert_eq!(calls, vec!["Sent Mail".to_string()]);
    }

    #[test]
    fn canonical_id_from_message_id_header() {
        let fetched = FetchedMessage {
            summary: blank_summary(),
            raw: b"Message-ID: <abc@example.com>\r\nSubject: test\r\n\r\nbody".to_vec(),
            extra: None,
        };
        let c1 = derive_canonical_id(&fetched);
        assert_eq!(c1.len(), 16, "canonical id should be 16 hex chars");

        // Same Message-ID from REST extra field should produce same hash.
        let fetched_rest = FetchedMessage {
            summary: blank_summary(),
            raw: b"".to_vec(),
            extra: Some(serde_json::json!({"header_message_id": "<abc@example.com>"})),
        };
        let c2 = derive_canonical_id(&fetched_rest);
        assert_eq!(c1, c2, "IMAP and REST should produce the same canonical id");
    }

    #[test]
    fn canonical_id_falls_back_to_source_id() {
        let fetched = FetchedMessage {
            summary: blank_summary(),
            raw: b"Subject: no message-id\r\n\r\nbody".to_vec(),
            extra: None,
        };
        let c = derive_canonical_id(&fetched);
        assert_eq!(c.len(), 16);
    }

    #[test]
    fn extract_message_id_from_raw() {
        let raw = b"From: a@b.com\r\nMessage-ID: <test-123@x.com>\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(
            extract_message_id_header(raw),
            Some("<test-123@x.com>".into())
        );
    }

    #[test]
    fn extract_message_id_handles_folded_header() {
        let raw = b"From: a@b.com\r\nMessage-ID:\r\n <folded@x.com>\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(
            extract_message_id_header(raw),
            Some("<folded@x.com>".into())
        );
    }

    #[test]
    fn extract_message_id_missing_returns_none() {
        let raw = b"From: a@b.com\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(extract_message_id_header(raw), None);
    }

    #[test]
    fn folder_safe_escapes_slashes() {
        assert_eq!(folder_safe("INBOX"), "INBOX");
        assert_eq!(folder_safe("Sent Mail"), "Sent Mail");
        assert_eq!(folder_safe("Archive/2024"), "Archive_2024");
    }
}
