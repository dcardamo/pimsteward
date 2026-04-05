//! Sieve script pull loop. Same pattern as contacts but simpler — scripts
//! are small, few, and sha256 of the content is good enough for diff.

use crate::error::Error;
use crate::forwardemail::Client;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::store::Repo;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub async fn pull_sieve(
    client: &Client,
    repo: &Repo,
    alias: &str,
    author_name: &str,
    author_email: &str,
) -> PullResult<PullSummary> {
    let list = client.list_sieve_scripts().await?;

    // list endpoint may not include `content`; fetch each individually.
    let mut full = Vec::with_capacity(list.len());
    for s in &list {
        let one = client.get_sieve_script(&s.id).await?;
        full.push(one);
    }

    let subdir = format!("sources/forwardemail/{}/sieve", alias);
    let mut local_hashes = read_local_sieve_hashes(repo, alias)?;
    let mut seen = std::collections::HashSet::new();
    let mut summary = PullSummary {
        resource: "sieve",
        ..Default::default()
    };

    for s in &full {
        let name = filename_safe(&s.name);
        let content = s.content.as_deref().unwrap_or("");
        let hash = content_sha256(content.as_bytes());
        seen.insert(name.clone());

        let prev_hash = local_hashes.remove(&name);
        let changed = prev_hash.as_ref() != Some(&hash);

        if changed {
            // Write the script
            let body_path = format!("{subdir}/{name}.sieve");
            repo.write_file(&body_path, content.as_bytes())?;

            // Write a small meta JSON
            let meta = serde_json::json!({
                "id": s.id,
                "name": s.name,
                "is_active": s.is_active,
                "is_valid": s.is_valid,
                "required_capabilities": s.required_capabilities,
                "security_warnings": s.security_warnings,
                "validation_errors": s.validation_errors,
                "updated_at": s.updated_at,
                "sha256": hash,
            });
            let meta_path = format!("{subdir}/{name}.meta.json");
            repo.write_file(&meta_path, serde_json::to_vec_pretty(&meta)?.as_slice())?;

            if prev_hash.is_some() {
                summary.updated += 1;
            } else {
                summary.added += 1;
            }
        }
    }

    // Deletions — anything in local_hashes that wasn't seen
    for name in local_hashes.keys() {
        if !seen.contains(name) {
            let body_path = repo.root().join(format!("{subdir}/{name}.sieve"));
            let meta_path = repo.root().join(format!("{subdir}/{name}.meta.json"));
            let _ = std::fs::remove_file(&body_path);
            let _ = std::fs::remove_file(&meta_path);
            summary.deleted += 1;
        }
    }

    let msg = format!(
        "sieve: +{} ~{} -{}",
        summary.added, summary.updated, summary.deleted
    );
    summary.commit_sha = repo.commit_all(author_name, author_email, &msg)?;
    Ok(summary)
}

fn content_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn read_local_sieve_hashes(repo: &Repo, alias: &str) -> Result<HashMap<String, String>, Error> {
    let mut out = HashMap::new();
    let dir = repo
        .root()
        .join(format!("sources/forwardemail/{}/sieve", alias));
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| Error::store(format!("readdir {}: {}", dir.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.ends_with(".meta.json") {
            continue;
        }
        let stem = name.trim_end_matches(".meta.json");
        let bytes = std::fs::read(&path).map_err(|e| Error::store(format!("read: {}", e)))?;
        let v: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(h) = v.get("sha256").and_then(|x| x.as_str()) {
            out.insert(stem.to_string(), h.to_string());
        }
    }
    Ok(out)
}
