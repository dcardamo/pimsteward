//! Disk → index reconciliation: rebuild, verify, and stats.
//!
//! Rebuild is the authoritative "make the index match the disk" path.
//! It's idempotent, resumable, and designed to be re-run as often as
//! callers like:
//!
//!   - `RebuildOpts::incremental()` (default): skip every .eml whose
//!     row has an `indexed_at >= file mtime`.  A SIGTERM / OOM /
//!     container restart loses at most one folder's uncommitted batch
//!     of CommitBatch rows; next rebuild resumes from whatever's
//!     already in the DB.
//!
//!   - `RebuildOpts::force()`: drop + recreate the FTS indexes and
//!     clear `messages`, then do a from-scratch scan.  Used for schema
//!     migrations, suspected corruption, or explicit `index rebuild
//!     --force`.
//!
//! Both paths sweep orphans at the end: any canonical_id in the DB
//! that wasn't seen on disk during the walk is deleted.  That's how
//! "rebuild is an authoritative sync" works: disk is the source of
//! truth, so anything the DB has that disk doesn't is stale.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde::Deserialize;

use super::{SearchIndex, envelope};
use crate::error::Error;

/// Configuration for [`SearchIndex::rebuild_from_disk`].
#[derive(Debug, Clone, Copy)]
pub struct RebuildOpts {
    pub force: bool,
    /// Commit every N messages within a folder.  Caps crash loss to
    /// this many rows per folder in progress.  Default 200.
    pub commit_batch: usize,
    /// Log an info line every N scanned messages.  Default 500.
    pub log_every: usize,
    /// Max bytes of `.eml` to read into memory.  Anything larger is
    /// skipped with a warning to protect the pull process from a
    /// pathological mail bomb.  Default 50 MB.
    pub max_eml_bytes: u64,
}

impl Default for RebuildOpts {
    fn default() -> Self {
        Self {
            force: false,
            commit_batch: 200,
            log_every: 500,
            max_eml_bytes: 50 * 1024 * 1024,
        }
    }
}

impl RebuildOpts {
    pub fn incremental() -> Self {
        Self::default()
    }
    pub fn force() -> Self {
        Self {
            force: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildStats {
    /// Total `.eml` files encountered on disk.
    pub scanned: u64,
    /// Rows inserted or replaced during the scan.
    pub upserted: u64,
    /// Rows skipped because `indexed_at >= file mtime`.
    pub skipped: u64,
    /// Rows removed because no matching `.eml` remained on disk.
    pub orphaned_deleted: u64,
    /// Parse/read/insert errors encountered; scan continues past them.
    pub errors: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub db_path: String,
    pub db_size_bytes: u64,
    pub schema_version: i64,
    pub messages: u64,
    pub folders: u64,
    pub oldest_date_unix: Option<i64>,
    pub newest_date_unix: Option<i64>,
    pub last_indexed_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    /// Canonical IDs present in the DB but not on disk.
    pub orphan_rows: Vec<String>,
    /// (canonical_id, folder_safe) pairs on disk but not in the DB.
    pub unindexed_emls: Vec<(String, String)>,
    pub cleaned: bool,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.orphan_rows.is_empty() && self.unindexed_emls.is_empty()
    }
}

/// Subset of `<canonical>.meta.json` that rebuild needs.  Kept here as
/// an independent Deserialize target so rebuild doesn't drag in the
/// full `pull::mail::MessageMeta` coupling.  Every field is optional
/// because we've seen meta.jsons in the wild that pre-date newer
/// additions to the schema.
#[derive(Debug, Deserialize, Default)]
struct DiskMeta {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    folder_path: Option<String>,
    #[serde(default)]
    internal_date: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    flags: Vec<String>,
}

impl SearchIndex {
    pub fn rebuild_from_disk(
        &self,
        repo_root: &Path,
        opts: RebuildOpts,
    ) -> Result<RebuildStats, Error> {
        let started = std::time::Instant::now();
        let mail_root = repo_root.join("mail");
        if !mail_root.is_dir() {
            return Ok(RebuildStats {
                elapsed_ms: elapsed_ms(started),
                ..Default::default()
            });
        }

        if opts.force {
            self.drop_all_messages()?;
        }

        // Pre-load every (canonical_id, indexed_at) for the skip check.
        // One SQL hit at start, then pure HashMap lookups per file.
        let known = if opts.force {
            HashMap::new()
        } else {
            self.all_indexed_at()?
        };

        let mut stats = RebuildStats::default();
        let mut seen: HashSet<String> = HashSet::with_capacity(known.len());

        for folder_dir in read_folder_dirs(&mail_root)? {
            self.rebuild_one_folder(
                &folder_dir,
                &opts,
                &known,
                &mut seen,
                &mut stats,
            )?;
        }

        // Orphan sweep: delete every row whose canonical_id is NOT in
        // `seen`.  We use a temp table rather than an IN-list because
        // SQLite's parameter limit bites at ~32k items, and mailboxes
        // can easily exceed that.
        let removed = self.sweep_orphans(&seen)?;
        stats.orphaned_deleted = removed;
        stats.elapsed_ms = elapsed_ms(started);

        tracing::info!(
            scanned = stats.scanned,
            upserted = stats.upserted,
            skipped = stats.skipped,
            orphaned_deleted = stats.orphaned_deleted,
            errors = stats.errors,
            elapsed_ms = stats.elapsed_ms,
            "rebuild complete"
        );
        Ok(stats)
    }

    fn rebuild_one_folder(
        &self,
        folder_dir: &Path,
        opts: &RebuildOpts,
        known: &HashMap<String, i64>,
        seen: &mut HashSet<String>,
        stats: &mut RebuildStats,
    ) -> Result<(), Error> {
        let entries = match std::fs::read_dir(folder_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(folder = %folder_dir.display(), error = %e, "read_dir failed");
                stats.errors += 1;
                return Ok(());
            }
        };

        let mut in_txn = false;
        let mut batch = 0usize;
        let conn_guard = self.lock();

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "dir entry read");
                    stats.errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension() != Some(OsStr::new("eml")) {
                continue;
            }
            let canonical_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            stats.scanned += 1;
            seen.insert(canonical_id.clone());

            // Incremental skip: file mtime ≤ indexed_at ⇒ already current.
            if !opts.force {
                if let Some(&indexed_at) = known.get(&canonical_id) {
                    if let Ok(mt) = file_mtime_unix(&path) {
                        if mt <= indexed_at {
                            stats.skipped += 1;
                            continue;
                        }
                    }
                }
            }

            // Size guard BEFORE reading.  Huge .eml files eat RAM.
            let size = match std::fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "metadata");
                    stats.errors += 1;
                    continue;
                }
            };
            if size > opts.max_eml_bytes {
                tracing::warn!(
                    path = %path.display(),
                    size,
                    cap = opts.max_eml_bytes,
                    "skipping oversized .eml during rebuild"
                );
                stats.errors += 1;
                continue;
            }

            let meta_path = path.with_extension("meta.json");
            let disk_meta = match std::fs::read(&meta_path) {
                Ok(bytes) => match serde_json::from_slice::<DiskMeta>(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(path = %meta_path.display(), error = %e, "meta.json parse");
                        stats.errors += 1;
                        continue;
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Orphan .eml without a sidecar.  Log and skip —
                    // rebuild can't attribute a folder to it without
                    // relying on the dir name, which is lossy.
                    tracing::warn!(path = %path.display(), "eml without meta.json");
                    stats.errors += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(path = %meta_path.display(), error = %e, "meta.json read");
                    stats.errors += 1;
                    continue;
                }
            };

            let raw = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "eml read");
                    stats.errors += 1;
                    continue;
                }
            };

            let folder = disk_meta
                .folder_path
                .as_deref()
                .unwrap_or_else(|| folder_dir.file_name().and_then(|s| s.to_str()).unwrap_or(""));
            let source_id = disk_meta.id.as_deref().unwrap_or("");
            let meta_facts = envelope::MetaFacts {
                canonical_id: &canonical_id,
                folder,
                source_id,
                flags: &disk_meta.flags,
                internal_date: disk_meta.internal_date.as_deref(),
                size: disk_meta.size,
            };

            let row = match envelope::parse_eml(&raw, &meta_facts) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "parse_eml");
                    stats.errors += 1;
                    continue;
                }
            };

            // Begin a folder-scoped transaction on first upsert so the
            // cost of opening a txn is amortized across every message in
            // the folder.
            if !in_txn {
                conn_guard.execute("BEGIN IMMEDIATE", [])?;
                in_txn = true;
            }

            match super::upsert_message_on(&conn_guard, &row, now_unix()) {
                Ok(()) => stats.upserted += 1,
                Err(e) => {
                    tracing::warn!(canonical_id = %canonical_id, error = %e, "upsert");
                    stats.errors += 1;
                }
            }

            batch += 1;
            if batch >= opts.commit_batch {
                conn_guard.execute("COMMIT", [])?;
                batch = 0;
                in_txn = false;
            }
            if stats.scanned.is_multiple_of(opts.log_every as u64) {
                tracing::info!(
                    scanned = stats.scanned,
                    upserted = stats.upserted,
                    skipped = stats.skipped,
                    "rebuild progress"
                );
            }
        }

        if in_txn {
            conn_guard.execute("COMMIT", [])?;
        }
        Ok(())
    }

    fn drop_all_messages(&self) -> Result<(), Error> {
        let conn = self.lock();
        // Deleting all rows cascades through AD trigger to purge FTS.
        // Using DELETE + sqlite_sequence reset keeps triggers firing so
        // FTS stays consistent; DROP+CREATE would desync the FTS
        // shadow tables.
        conn.execute("DELETE FROM messages", [])?;
        Ok(())
    }

    fn sweep_orphans(&self, seen: &HashSet<String>) -> Result<u64, Error> {
        let conn = self.lock();
        // Build a temp table of canonical_ids seen on disk, then
        // anti-join against messages.  Stays within SQLite's parameter
        // budget for mailboxes of arbitrary size.
        conn.execute("CREATE TEMP TABLE IF NOT EXISTS _rebuild_seen (canonical_id TEXT PRIMARY KEY)", [])?;
        conn.execute("DELETE FROM _rebuild_seen", [])?;
        {
            let mut stmt = conn.prepare("INSERT INTO _rebuild_seen (canonical_id) VALUES (?1)")?;
            for id in seen {
                stmt.execute(params![id])?;
            }
        }
        let n = conn.execute(
            "DELETE FROM messages WHERE canonical_id NOT IN (SELECT canonical_id FROM _rebuild_seen)",
            [],
        )?;
        conn.execute("DROP TABLE _rebuild_seen", [])?;
        Ok(n as u64)
    }

    pub fn stats(&self) -> Result<IndexStats, Error> {
        let conn = self.lock();
        let db_path = conn.path().map(|p| p.to_string()).unwrap_or_default();
        let db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
        let schema_version: i64 = conn
            .query_row("SELECT v FROM schema_version LIMIT 1", [], |r| r.get(0))
            .unwrap_or(0);
        let messages: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        let folders: i64 = conn
            .query_row("SELECT COUNT(DISTINCT folder) FROM messages", [], |r| r.get(0))?;
        let oldest_date_unix: Option<i64> = conn
            .query_row(
                "SELECT MIN(date_unix) FROM messages WHERE date_unix IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .ok();
        let newest_date_unix: Option<i64> = conn
            .query_row(
                "SELECT MAX(date_unix) FROM messages WHERE date_unix IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .ok();
        let last_indexed_at: Option<i64> = conn
            .query_row("SELECT MAX(indexed_at) FROM messages", [], |r| r.get(0))
            .ok();
        Ok(IndexStats {
            db_path,
            db_size_bytes,
            schema_version,
            messages: messages as u64,
            folders: folders as u64,
            oldest_date_unix,
            newest_date_unix,
            last_indexed_at,
        })
    }

    pub fn verify(&self, repo_root: &Path, clean: bool) -> Result<VerifyReport, Error> {
        let mail_root = repo_root.join("mail");
        let mut on_disk: HashSet<String> = HashSet::new();
        let mut on_disk_by_id: HashMap<String, String> = HashMap::new();
        if mail_root.is_dir() {
            for folder_dir in read_folder_dirs(&mail_root)? {
                let folder_safe = folder_dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                for entry in std::fs::read_dir(&folder_dir)?.flatten() {
                    let p = entry.path();
                    if p.extension() != Some(OsStr::new("eml")) {
                        continue;
                    }
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        on_disk.insert(stem.to_string());
                        on_disk_by_id.insert(stem.to_string(), folder_safe.clone());
                    }
                }
            }
        }

        let in_db: HashSet<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT canonical_id FROM messages")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = HashSet::new();
            for r in rows {
                set.insert(r?);
            }
            set
        };

        let orphan_rows: Vec<String> = in_db.difference(&on_disk).cloned().collect();
        let unindexed_emls: Vec<(String, String)> = on_disk
            .difference(&in_db)
            .map(|id| (id.clone(), on_disk_by_id.get(id).cloned().unwrap_or_default()))
            .collect();

        if clean {
            let conn = self.lock();
            for id in &orphan_rows {
                conn.execute(
                    "DELETE FROM messages WHERE canonical_id = ?1",
                    params![id],
                )?;
            }
            // unindexed .eml files need a full parse+upsert cycle — drop
            // the lock and delegate to rebuild_from_disk in incremental
            // mode, which will no-op the already-indexed rows and pick
            // up the missing ones.  That also refreshes indexed_at on
            // the just-written rows, which avoids a second verify pass
            // flagging them again.
            drop(conn);
            if !unindexed_emls.is_empty() {
                self.rebuild_from_disk(repo_root, RebuildOpts::incremental())?;
            }
        }

        Ok(VerifyReport {
            orphan_rows,
            unindexed_emls,
            cleaned: clean,
        })
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn read_folder_dirs(mail_root: &Path) -> Result<Vec<std::path::PathBuf>, Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(mail_root)?.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            // Skip top-level hidden dirs and `_attachments` (content-
            // addressed blob store maintained by the pull loop).
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with('_') || name.starts_with('.') {
                    continue;
                }
            }
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn file_mtime_unix(p: &Path) -> Result<i64, Error> {
    let mt = std::fs::metadata(p)?.modified()?;
    Ok(mt
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const SAMPLE_EML: &[u8] = b"From: alice@apple.com\r\n\
                                To: dan@hld.ca\r\n\
                                Subject: your order CAEN\r\n\
                                Message-ID: <m1@test>\r\n\
                                Date: Mon, 20 Apr 2026 11:02:18 +0000\r\n\
                                Content-Type: text/plain; charset=utf-8\r\n\
                                \r\n\
                                thanks for shopping at apple";

    fn seed_mail_tree(root: &Path) -> Vec<(String, String)> {
        // Layout: mail/<folder_safe>/<canonical_id>.{eml,meta.json}
        let entries = vec![
            ("INBOX", "0000000000000001", "INBOX"),
            ("Archive_2026", "0000000000000002", "Archive/2026"),
            ("Archive_2026", "0000000000000003", "Archive/2026"),
            ("Archive_2025", "0000000000000004", "Archive/2025"),
        ];
        for (dir, id, real_folder) in &entries {
            let d = root.join("mail").join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("{id}.eml")), SAMPLE_EML).unwrap();
            let meta = serde_json::json!({
                "id": format!("src-{id}"),
                "folder_path": real_folder,
                "internal_date": "2026-04-20T11:02:18Z",
                "size": SAMPLE_EML.len(),
                "flags": ["\\Seen"],
            });
            std::fs::write(
                d.join(format!("{id}.meta.json")),
                serde_json::to_string(&meta).unwrap(),
            )
            .unwrap();
        }
        entries
            .into_iter()
            .map(|(d, id, _)| (d.to_string(), id.to_string()))
            .collect()
    }

    #[test]
    fn rebuild_full_scan_from_empty_db() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.upserted, 4);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.orphaned_deleted, 0);
        assert_eq!(stats.errors, 0);
        assert_eq!(idx.message_count().unwrap(), 4);
    }

    #[test]
    fn rebuild_incremental_second_run_skips_all() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        // Bump indexed_at above all file mtimes, forcing the skip path.
        {
            let conn = idx.lock();
            conn.execute("UPDATE messages SET indexed_at = ?1", params![i64::MAX / 2])
                .unwrap();
        }
        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.upserted, 0);
        assert_eq!(stats.skipped, 4);
    }

    #[test]
    fn rebuild_sweeps_orphans() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        // Pre-insert a phantom row that won't be found on disk.
        idx.upsert_message(&envelope::MessageRow {
            canonical_id: "deadbeefdeadbeef".into(),
            folder: "Ghost".into(),
            source_id: "src".into(),
            message_id: Some("<x@x>".into()),
            flags: vec![],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(idx.message_count().unwrap(), 1);
        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        assert_eq!(stats.orphaned_deleted, 1);
        assert_eq!(idx.message_count().unwrap(), 4);
    }

    #[test]
    fn rebuild_uses_real_folder_path_from_meta() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        // Folder on disk is Archive_2026; real folder is Archive/2026.
        let r = idx
            .search(&super::super::SearchQuery {
                folder: Some(super::super::FolderFilter::Prefix("Archive".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 3, "Archive/2026 + Archive/2025");
    }

    #[test]
    fn rebuild_force_wipes_and_repopulates() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        assert_eq!(idx.message_count().unwrap(), 4);
        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::force())
            .unwrap();
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.upserted, 4);
        assert_eq!(stats.skipped, 0);
        // force still sweeps orphans (there are none after the wipe,
        // since everything on disk was freshly re-upserted).
        assert_eq!(stats.orphaned_deleted, 0);
    }

    #[test]
    fn rebuild_resumability_after_partial_commit() {
        // Simulate interruption: seed, rebuild with commit_batch=1 so
        // every upsert commits immediately, then delete half the rows
        // to simulate a crash that lost work, run rebuild again — only
        // the missing ones should be re-upserted.  This is how an
        // OOM'd rebuild picks up where it left off.
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        let opts = RebuildOpts {
            commit_batch: 1,
            ..RebuildOpts::incremental()
        };
        idx.rebuild_from_disk(td.path(), opts).unwrap();
        assert_eq!(idx.message_count().unwrap(), 4);

        // Delete two rows, representing "only half made it to disk
        // before crash."  The orphan sweep on next rebuild shouldn't
        // touch them because they're gone from the DB, not disk.
        {
            let conn = idx.lock();
            conn.execute(
                "DELETE FROM messages WHERE canonical_id IN ('0000000000000003', '0000000000000004')",
                [],
            )
            .unwrap();
        }
        assert_eq!(idx.message_count().unwrap(), 2);

        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        // 4 scanned; 2 skipped (still current), 2 upserted (newly indexed);
        // 0 orphaned deleted.
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.upserted, 2);
        assert_eq!(stats.skipped, 2);
        assert_eq!(idx.message_count().unwrap(), 4);
    }

    #[test]
    fn rebuild_missing_meta_json_is_error_not_crash() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        // Drop one meta.json to simulate corruption.
        std::fs::remove_file(
            td.path()
                .join("mail/Archive_2026/0000000000000002.meta.json"),
        )
        .unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        let stats = idx
            .rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.upserted, 3);
        assert_eq!(stats.errors, 1);
    }

    #[test]
    fn stats_reflects_current_contents() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        let s = idx.stats().unwrap();
        assert_eq!(s.messages, 4);
        assert_eq!(s.folders, 3);
        assert!(s.db_size_bytes > 0);
        assert_eq!(s.schema_version, super::super::SCHEMA_VERSION);
        assert!(s.oldest_date_unix.is_some());
        assert!(s.newest_date_unix.is_some());
    }

    #[test]
    fn verify_clean_matches() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        let report = idx.verify(td.path(), false).unwrap();
        assert!(report.is_clean(), "report: {report:?}");
    }

    #[test]
    fn verify_detects_orphan_rows_and_unindexed_emls() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();

        // Orphan: delete an .eml off disk (row stays in DB).
        std::fs::remove_file(td.path().join("mail/INBOX/0000000000000001.eml")).unwrap();
        // Unindexed: drop its row from the DB while leaving the .eml.
        // Pick a different row so orphan detection has something too.
        {
            let conn = idx.lock();
            conn.execute(
                "DELETE FROM messages WHERE canonical_id = '0000000000000003'",
                [],
            )
            .unwrap();
        }

        let report = idx.verify(td.path(), false).unwrap();
        assert_eq!(report.orphan_rows, vec!["0000000000000001".to_string()]);
        assert_eq!(report.unindexed_emls.len(), 1);
        assert_eq!(report.unindexed_emls[0].0, "0000000000000003");
    }

    #[test]
    fn verify_clean_fixes_both_sides() {
        let td = tempdir().unwrap();
        seed_mail_tree(td.path());
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.rebuild_from_disk(td.path(), RebuildOpts::incremental())
            .unwrap();
        // Seed divergence.
        std::fs::remove_file(td.path().join("mail/INBOX/0000000000000001.eml")).unwrap();
        std::fs::remove_file(td.path().join("mail/INBOX/0000000000000001.meta.json")).unwrap();
        {
            let conn = idx.lock();
            conn.execute(
                "DELETE FROM messages WHERE canonical_id = '0000000000000003'",
                [],
            )
            .unwrap();
        }
        let report = idx.verify(td.path(), true).unwrap();
        assert!(report.cleaned);
        // After clean, verify again should be spotless.
        let follow_up = idx.verify(td.path(), false).unwrap();
        assert!(
            follow_up.is_clean(),
            "post-clean verify dirty: {follow_up:?}"
        );
    }
}
