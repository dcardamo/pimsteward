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
    alias: &str,
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

    // 2. Per-folder message sync
    for f in &folders {
        let folder_dir = format!(
            "sources/forwardemail/{}/mail/{}",
            alias,
            folder_safe(&f.path)
        );
        let local_meta = read_local_message_meta(repo, &folder_dir)?;

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
            match extract_attachments(&fetched, alias, repo)? {
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
                summary.updated += 1;
            } else {
                summary.added += 1;
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
                summary.deleted += 1;
            }
        }
        // 5. Write/update _folder.json with the latest CONDSTORE state so
        //    the next pull can pass a stable since_modseq hint. We fold
        //    the source-returned ListResult values into the Folder from
        //    list_folders() — REST populates modify_index/uid_validity up
        //    front; IMAP leaves them None and we fill them here from
        //    ListResult.
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
        // `_attachments` is the content-addressed blob store, not a folder;
        // never treat it as a stale folder even though it isn't in the
        // remote folder list.
        if name == "_attachments" {
            continue;
        }
        if !current.contains(&name) {
            // Remove the whole folder subtree
            let _ = std::fs::remove_dir_all(entry.path());
        }
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

fn hash_to_canonical(input: &str) -> String {
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
        if line.starts_with("Message-ID:") || line.starts_with("Message-Id:") || line.starts_with("message-id:") {
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
    alias: &str,
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

        let blob_path = format!("sources/forwardemail/{alias}/mail/_attachments/{sha}");
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

        let refs = extract_attachments(&fetched, "alias", &repo)
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
            .join(format!("sources/forwardemail/alias/mail/_attachments/{}", refs[0].sha256));
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
        assert!(extract_attachments(&fetched, "alias", &repo)
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
        let refs = extract_attachments(&fetched, "alias", &repo)
            .unwrap()
            .unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn read_prev_folder_state_reads_uid_validity_and_modify_index() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(tmp.path()).unwrap();
        let folder_dir = "sources/forwardemail/alias/mail/INBOX";
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
        let (uv, mi) = read_prev_folder_state(&repo, "sources/forwardemail/alias/mail/INBOX");
        assert_eq!((uv, mi), (None, None));
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
