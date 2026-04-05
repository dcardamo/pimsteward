//! Contact pull loop.
//!
//! Strategy: list all contacts, diff against the existing tree under
//! `sources/forwardemail/<alias>/contacts/default/`, write new/changed vCards,
//! remove deleted ones, commit as a single batch.
//!
//! vCard bytes come from the `content` field of the API response and are
//! stored verbatim. No canonicalisation is needed — the smoke test confirmed
//! sequential GETs return byte-identical content, and the etag is stable.
//!
//! Sidecar metadata (the forwardemail-specific id, the etag, the
//! updated_at) lives at `<uid>.meta.json` alongside the `.vcf` file. The
//! vCard is the "user's data" and the meta is "pimsteward's bookkeeping."

use crate::error::Error;
use crate::forwardemail::contacts::Contact;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::source::ContactsSource;
use crate::store::Repo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContactMeta {
    id: String,
    uid: String,
    etag: String,
    updated_at: Option<String>,
}

/// Run one pull cycle for contacts.
///
/// `alias` is the forwardemail alias (used as the path segment under
/// `sources/forwardemail/`). Typically the alias email with `@` replaced by
/// `-`, e.g. `dan-hld.ca`.
pub async fn pull_contacts(
    source: &dyn ContactsSource,
    repo: &Repo,
    alias: &str,
    author_name: &str,
    author_email: &str,
) -> PullResult<PullSummary> {
    let remote = source.list_contacts().await?;
    let local = read_local_contacts(repo, alias)?;

    let remote_by_uid: HashMap<String, &Contact> =
        remote.iter().map(|c| (c.uid.clone(), c)).collect();

    let mut summary = PullSummary {
        resource: "contacts",
        ..Default::default()
    };

    let subdir = format!("sources/forwardemail/{}/contacts/default", alias);

    // Upserts
    for c in &remote {
        let uid = filename_safe(&c.uid);
        let existing = local.get(&uid);
        let etag_changed = existing.map(|m| m.etag != c.etag).unwrap_or(true);

        if etag_changed {
            let vcf_path = format!("{subdir}/{uid}.vcf");
            let meta_path = format!("{subdir}/{uid}.meta.json");

            repo.write_file(&vcf_path, c.content.as_bytes())?;
            let meta = ContactMeta {
                id: c.id.clone(),
                uid: c.uid.clone(),
                etag: c.etag.clone(),
                updated_at: c.updated_at.clone(),
            };
            let meta_bytes = serde_json::to_vec_pretty(&meta)?;
            repo.write_file(&meta_path, &meta_bytes)?;

            if existing.is_some() {
                summary.updated += 1;
            } else {
                summary.added += 1;
            }
        }
    }

    // Deletions: anything local that's no longer remote
    for uid in local.keys() {
        if !remote_by_uid.contains_key(uid) {
            let vcf_path = repo.root().join(format!("{subdir}/{uid}.vcf"));
            let meta_path = repo.root().join(format!("{subdir}/{uid}.meta.json"));
            let _ = std::fs::remove_file(&vcf_path);
            let _ = std::fs::remove_file(&meta_path);
            summary.deleted += 1;
        }
    }

    // Commit
    let msg = format!(
        "contacts: +{} ~{} -{}",
        summary.added, summary.updated, summary.deleted
    );
    summary.commit_sha = repo.commit_all(author_name, author_email, &msg)?;
    Ok(summary)
}

fn read_local_contacts(repo: &Repo, alias: &str) -> Result<HashMap<String, ContactMeta>, Error> {
    let mut out = HashMap::new();
    let dir: PathBuf = repo
        .root()
        .join(format!("sources/forwardemail/{}/contacts/default", alias));
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| Error::store(format!("readdir {}: {}", dir.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let name = name.trim_end_matches(".meta"); // <uid>.meta.json → <uid>
        let bytes = std::fs::read(&path)
            .map_err(|e| Error::store(format!("read {}: {}", path.display(), e)))?;
        let meta: ContactMeta = serde_json::from_slice(&bytes)?;
        out.insert(name.to_string(), meta);
    }
    Ok(out)
}
