//! Local search index for mail — SQLite + FTS5 sidecar of the git-tracked
//! mail backup.
//!
//! See `docs/specs/2026-04-20-local-search-index-design.md` for the full
//! design. Brief recap:
//!
//! - File lives at `<repo_root>/.pimsteward/search_index.sqlite` (git-
//!   ignored). Never touched by anything outside pimsteward's process.
//! - `messages` table holds one row per `.eml` on disk, keyed by
//!   canonical_id (the filename stem).
//! - `messages_body` holds extracted plain-text body (capped at 128 KB),
//!   kept in a separate table so the main row stays small.
//! - `messages_fts` is a contentless FTS5 virtual table over subject +
//!   from_name + to_addrs + body; triggers keep it in sync.
//! - All operations are idempotent by canonical_id. Rebuild is resumable
//!   because the only progress state is the rows themselves.
//! - A single `Mutex<Connection>` is shared between the pull-loop writer
//!   and MCP readers. WAL mode keeps contention low.

pub mod envelope;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Error;

pub use envelope::MessageRow;

/// The current schema version.  Bump this every time migrations are added
/// and add a new arm in `migrate_up` that carries the DB forward from the
/// previous version.
pub const SCHEMA_VERSION: i64 = 1;

/// Plain-text body-extract cap, in bytes, applied before insert into
/// `messages_body`.  Keeps the DB bounded against HTML newsletters, mail
/// bombs, or accidentally-huge base64 payloads while leaving enough
/// headroom for real content (Ottawa Lookout strips to 50–80 KB).
pub const BODY_CAP_BYTES: usize = 128 * 1024;

/// The SQLite search index.
///
/// Instances are cheap to clone-conceptually but the underlying connection
/// is wrapped in a `Mutex` so all mutating methods take `&self`.  Pull
/// writes and MCP reads share one connection; rusqlite is not `Sync`
/// across a single handle, so we serialize access at the mutex rather
/// than maintaining a pool.
pub struct SearchIndex {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl SearchIndex {
    /// Open (or create) the search index rooted at `<repo_root>/.pimsteward/`.
    ///
    /// Creates the directory if missing.  Appends `.pimsteward/` to the
    /// repo's top-level `.gitignore` idempotently so the SQLite files
    /// never land in a commit.  Runs any pending schema migrations up to
    /// `SCHEMA_VERSION`.
    pub fn open(repo_root: &Path) -> Result<Self, Error> {
        let dir = repo_root.join(".pimsteward");
        std::fs::create_dir_all(&dir)?;
        ensure_gitignored(repo_root)?;

        let db_path = dir.join("search_index.sqlite");
        let conn = Connection::open(&db_path)?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
        })
    }

    /// Number of rows in `messages` — cheap, useful for drift checks and
    /// stats output.
    pub fn message_count(&self) -> Result<u64, Error> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Upsert a single message row.  Idempotent by canonical_id.
    /// Refreshes `indexed_at` to `now_unix` on every call so incremental
    /// rebuild's mtime comparison works correctly.  Triggers mirror the
    /// change into `messages_fts`.
    pub fn upsert_message(&self, row: &MessageRow) -> Result<(), Error> {
        let conn = self.lock();
        upsert_message_on(&conn, row, now_unix())
    }

    /// Delete a single row by canonical_id.  Returns the number of rows
    /// deleted (0 if not present).
    pub fn delete_message(&self, canonical_id: &str) -> Result<u64, Error> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM messages WHERE canonical_id = ?1",
            params![canonical_id],
        )?;
        Ok(n as u64)
    }

    /// Delete all rows whose folder matches `folder` exactly OR is a
    /// descendant (prefix match with a `/` separator).  Returns the
    /// number of rows removed.  Cascades through the AD trigger into
    /// `messages_fts`.
    pub fn delete_folder(&self, folder: &str) -> Result<u64, Error> {
        let conn = self.lock();
        let like = format!("{folder}/%");
        let n = conn.execute(
            "DELETE FROM messages WHERE folder = ?1 OR folder LIKE ?2",
            params![folder, like],
        )?;
        Ok(n as u64)
    }

    /// Return the `indexed_at` timestamp for a given canonical_id, or
    /// None if the row is not present.  Used by rebuild to short-circuit
    /// re-parsing .eml files that are already current.
    pub fn indexed_at(&self, canonical_id: &str) -> Result<Option<i64>, Error> {
        let conn = self.lock();
        let t = conn
            .query_row(
                "SELECT indexed_at FROM messages WHERE canonical_id = ?1",
                params![canonical_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(t)
    }

    /// Load every (canonical_id, indexed_at) pair into memory.  Cheap at
    /// mailbox scale (10k–100k messages, tens of MB) and lets rebuild do
    /// per-file "is this already indexed?" checks with zero SQL.
    pub fn all_indexed_at(&self) -> Result<std::collections::HashMap<String, i64>, Error> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT canonical_id, indexed_at FROM messages")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // PoisonError here would mean a previous writer panicked mid-txn.
        // The DB is transactionally safe, so we just unpoison and carry
        // on — no silent corruption risk.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
}

// ── write helpers ─────────────────────────────────────────────────────────

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// INSERT ... ON CONFLICT DO UPDATE for a single row.  Takes an
/// explicit `indexed_at` so callers that want deterministic timestamps
/// (rebuild tests, synthetic corpora) can supply one.
fn upsert_message_on(conn: &Connection, row: &MessageRow, indexed_at: i64) -> Result<(), Error> {
    let flags_json = serde_json::to_string(&row.flags)
        .map_err(|e| Error::index(format!("flags serialize: {e}")))?;
    conn.execute(
        r#"
        INSERT INTO messages (
            canonical_id, folder, source_id, message_id,
            from_addr, from_name, to_addrs, cc_addrs,
            subject, body, date_unix, size, flags, has_attach, indexed_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15
        )
        ON CONFLICT(canonical_id) DO UPDATE SET
            folder      = excluded.folder,
            source_id   = excluded.source_id,
            message_id  = excluded.message_id,
            from_addr   = excluded.from_addr,
            from_name   = excluded.from_name,
            to_addrs    = excluded.to_addrs,
            cc_addrs    = excluded.cc_addrs,
            subject     = excluded.subject,
            body        = excluded.body,
            date_unix   = excluded.date_unix,
            size        = excluded.size,
            flags       = excluded.flags,
            has_attach  = excluded.has_attach,
            indexed_at  = excluded.indexed_at
        "#,
        params![
            row.canonical_id,
            row.folder,
            row.source_id,
            row.message_id,
            row.from_addr,
            row.from_name,
            row.to_addrs,
            row.cc_addrs,
            row.subject,
            row.body_text,
            row.date_unix,
            row.size,
            flags_json,
            row.has_attachments as i64,
            indexed_at,
        ],
    )?;
    Ok(())
}

// ── migrations ────────────────────────────────────────────────────────────

fn apply_pragmas(conn: &Connection) -> Result<(), Error> {
    // journal_mode=WAL: readers + a single writer can proceed concurrently
    // without the writer blocking readers.  synchronous=NORMAL is
    // conventional with WAL: durable up to the most recent checkpoint,
    // one fewer fsync per transaction than FULL.  foreign_keys=ON is off
    // by default in SQLite; we want cascading deletes for messages_body.
    // temp_store=MEMORY keeps sort/hash scratch off disk.
    //
    // journal_mode is the only pragma that returns a row the caller must
    // consume (it echoes the mode back).  `query_row` swallows it; the
    // others use `execute` and don't produce rows.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.execute("PRAGMA synchronous = NORMAL", [])?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    conn.execute("PRAGMA temp_store = MEMORY", [])?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), Error> {
    // schema_version is a single-row meta table.  Create it lazily so
    // open() on a brand-new DB doesn't need a separate init step.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (v INTEGER NOT NULL);",
    )?;
    let current: Option<i64> = conn
        .query_row("SELECT v FROM schema_version LIMIT 1", [], |r| r.get(0))
        .ok();

    match current {
        None => {
            migrate_v0_to_v1(conn)?;
            conn.execute("INSERT INTO schema_version (v) VALUES (?1)", [SCHEMA_VERSION])?;
        }
        Some(v) if v == SCHEMA_VERSION => { /* nothing to do */ }
        Some(v) if v < SCHEMA_VERSION => {
            // When v2 lands, dispatch here by v.
            return Err(Error::index(format!(
                "migration from v{v} to v{SCHEMA_VERSION} not implemented (DB was created by a newer pimsteward?)"
            )));
        }
        Some(v) => {
            return Err(Error::index(format!(
                "search_index.sqlite is schema v{v}, newer than this build (v{SCHEMA_VERSION}) — upgrade pimsteward or delete the file and let it rebuild"
            )));
        }
    }
    Ok(())
}

fn migrate_v0_to_v1(conn: &Connection) -> Result<(), Error> {
    // One batch so the file atomically ends up either "v1 complete" or
    // "empty".  Body lives inline in `messages` (capped at BODY_CAP_BYTES
    // by envelope.rs) so there's a single source of truth per row and
    // upserts don't need to sequence two writes.  FTS5 uses external
    // content against `messages` so INSERT/UPDATE/DELETE on the row
    // drives the index via triggers; pure column-to-index mirroring.
    conn.execute_batch(
        r#"
        BEGIN;

        CREATE TABLE messages (
            canonical_id  TEXT PRIMARY KEY,
            folder        TEXT NOT NULL,
            source_id     TEXT NOT NULL,
            message_id    TEXT,
            from_addr     TEXT,
            from_name     TEXT,
            to_addrs      TEXT,
            cc_addrs      TEXT,
            subject       TEXT,
            body          TEXT,
            date_unix     INTEGER,
            size          INTEGER,
            flags         TEXT NOT NULL DEFAULT '[]',
            has_attach    INTEGER NOT NULL DEFAULT 0,
            indexed_at    INTEGER NOT NULL
        );
        CREATE INDEX idx_messages_folder     ON messages(folder);
        CREATE INDEX idx_messages_from_addr  ON messages(from_addr);
        CREATE INDEX idx_messages_date_unix  ON messages(date_unix);
        CREATE INDEX idx_messages_message_id ON messages(message_id);

        CREATE VIRTUAL TABLE messages_fts USING fts5(
            subject, from_name, to_addrs, body,
            content='messages',
            content_rowid='rowid',
            tokenize = 'unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
            VALUES (new.rowid, new.subject, new.from_name, new.to_addrs, new.body);
        END;

        CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs, old.body);
        END;

        CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs, old.body);
            INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
            VALUES (new.rowid, new.subject, new.from_name, new.to_addrs, new.body);
        END;

        COMMIT;
        "#,
    )?;
    Ok(())
}

/// Append `.pimsteward/` to the repo's top-level `.gitignore` if it isn't
/// already matched.  Creates the file if missing.  The match is line-
/// level so `.pimsteward/` and `.pimsteward` both count as "already
/// present" — conservative; we never remove existing entries.
fn ensure_gitignored(repo_root: &Path) -> Result<(), Error> {
    let path = repo_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let already = existing.lines().any(|l| {
        let t = l.trim();
        t == ".pimsteward/" || t == ".pimsteward" || t == "/.pimsteward/" || t == "/.pimsteward"
    });
    if already {
        return Ok(());
    }
    let mut new = existing;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(".pimsteward/\n");
    std::fs::write(&path, new)?;
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_creates_dir_and_tables() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        assert!(td.path().join(".pimsteward/search_index.sqlite").exists());

        let conn = idx.lock();
        // All expected objects exist.
        for obj in ["messages", "messages_fts", "schema_version"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    [obj],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{obj} should exist");
        }
        let v: i64 = conn
            .query_row("SELECT v FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn open_is_idempotent() {
        let td = tempdir().unwrap();
        let _a = SearchIndex::open(td.path()).unwrap();
        // Drop the first handle so the second open doesn't race a lock
        // we wouldn't otherwise contend.
        drop(_a);
        let b = SearchIndex::open(td.path()).unwrap();
        assert_eq!(b.message_count().unwrap(), 0);
    }

    #[test]
    fn gitignore_appends_once() {
        let td = tempdir().unwrap();
        let _a = SearchIndex::open(td.path()).unwrap();
        let _b = SearchIndex::open(td.path()).unwrap();
        let contents = std::fs::read_to_string(td.path().join(".gitignore")).unwrap();
        let n = contents.lines().filter(|l| l.trim() == ".pimsteward/").count();
        assert_eq!(n, 1, "gitignore entry must be appended at most once");
    }

    #[test]
    fn gitignore_preserves_existing_entries() {
        let td = tempdir().unwrap();
        std::fs::write(td.path().join(".gitignore"), "target/\n*.log\n").unwrap();
        let _a = SearchIndex::open(td.path()).unwrap();
        let contents = std::fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert!(contents.contains("target/"));
        assert!(contents.contains("*.log"));
        assert!(contents.contains(".pimsteward/"));
    }

    #[test]
    fn gitignore_recognizes_prior_variants() {
        let td = tempdir().unwrap();
        std::fs::write(td.path().join(".gitignore"), "/.pimsteward\n").unwrap();
        let _a = SearchIndex::open(td.path()).unwrap();
        let contents = std::fs::read_to_string(td.path().join(".gitignore")).unwrap();
        // Should not double-add: the existing entry is a valid match.
        let n = contents.lines().filter(|l| l.contains("pimsteward")).count();
        assert_eq!(n, 1);
    }

    fn row(canonical_id: &str, folder: &str, from: &str, subject: &str) -> MessageRow {
        MessageRow {
            canonical_id: canonical_id.to_string(),
            folder: folder.to_string(),
            source_id: "src-1".to_string(),
            message_id: Some(format!("<{canonical_id}@test>")),
            from_addr: Some(from.to_string()),
            from_name: Some("Sender".to_string()),
            to_addrs: Some("dan@hld.ca".to_string()),
            cc_addrs: None,
            subject: Some(subject.to_string()),
            date_unix: Some(1700000000),
            size: Some(1234),
            flags: vec!["\\Seen".to_string()],
            has_attachments: false,
            body_text: Some(format!("body of {subject}")),
        }
    }

    #[test]
    fn upsert_inserts_then_replaces() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "INBOX", "x@y", "hello")).unwrap();
        assert_eq!(idx.message_count().unwrap(), 1);
        // Second upsert with changed fields updates in place, doesn't
        // duplicate.
        idx.upsert_message(&row("a1", "Archive", "x@y", "hello updated"))
            .unwrap();
        assert_eq!(idx.message_count().unwrap(), 1);
        let conn = idx.lock();
        let (folder, subj): (String, String) = conn
            .query_row(
                "SELECT folder, subject FROM messages WHERE canonical_id = 'a1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(folder, "Archive");
        assert_eq!(subj, "hello updated");
    }

    #[test]
    fn upsert_keeps_fts_in_sync() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "INBOX", "x@y", "apple invoice"))
            .unwrap();
        idx.upsert_message(&row("a2", "INBOX", "x@y", "banana receipt"))
            .unwrap();
        let conn = idx.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'apple'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'banana'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        drop(conn);
        // After an upsert that CHANGES the body, the old FTS tokens are
        // gone and the new ones are present.  FTS5 tokenizes on word
        // boundaries (unicode61), so "apple" as a token is cleanly
        // removed — it does not substring-match "pineapple".
        let mut r = row("a1", "INBOX", "x@y", "apple invoice");
        r.body_text = Some("rewritten body with watermelon instead".into());
        r.subject = Some("watermelon now".into());
        idx.upsert_message(&r).unwrap();
        let conn = idx.lock();
        let n_apple: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'apple'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_invoice: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'invoice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_watermelon: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'watermelon'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_apple, 0, "old subject token should be gone");
        assert_eq!(n_invoice, 0, "old body token should be gone");
        assert_eq!(n_watermelon, 1, "new tokens should be indexed");
    }

    #[test]
    fn delete_removes_from_messages_and_fts() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "INBOX", "x@y", "apple invoice"))
            .unwrap();
        idx.upsert_message(&row("a2", "INBOX", "x@y", "banana receipt"))
            .unwrap();
        let deleted = idx.delete_message("a1").unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(idx.message_count().unwrap(), 1);
        let conn = idx.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'apple'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "deleted row's FTS entry must be gone");
    }

    #[test]
    fn delete_message_missing_is_zero_not_error() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        assert_eq!(idx.delete_message("nonexistent").unwrap(), 0);
    }

    #[test]
    fn delete_folder_exact_and_prefix() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "INBOX", "x@y", "inbox-msg"))
            .unwrap();
        idx.upsert_message(&row("a2", "Archive", "x@y", "archive-msg"))
            .unwrap();
        idx.upsert_message(&row("a3", "Archive/2026", "x@y", "archive-2026"))
            .unwrap();
        idx.upsert_message(&row("a4", "Archive/2025", "x@y", "archive-2025"))
            .unwrap();
        let n = idx.delete_folder("Archive").unwrap();
        assert_eq!(n, 3, "Archive + Archive/* should sweep three rows");
        assert_eq!(idx.message_count().unwrap(), 1);
    }

    #[test]
    fn delete_folder_does_not_match_unrelated_prefix() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "Archive", "x@y", "arch"))
            .unwrap();
        idx.upsert_message(&row("a2", "ArchiveSiblings", "x@y", "sib"))
            .unwrap();
        let n = idx.delete_folder("Archive").unwrap();
        assert_eq!(n, 1, "ArchiveSiblings must not match Archive prefix");
        assert_eq!(idx.message_count().unwrap(), 1);
    }

    #[test]
    fn indexed_at_present_and_absent() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        assert!(idx.indexed_at("missing").unwrap().is_none());
        idx.upsert_message(&row("a1", "INBOX", "x@y", "hi")).unwrap();
        assert!(idx.indexed_at("a1").unwrap().is_some());
    }

    #[test]
    fn all_indexed_at_round_trip() {
        let td = tempdir().unwrap();
        let idx = SearchIndex::open(td.path()).unwrap();
        idx.upsert_message(&row("a1", "INBOX", "x@y", "one")).unwrap();
        idx.upsert_message(&row("a2", "INBOX", "x@y", "two")).unwrap();
        let map = idx.all_indexed_at().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("a1"));
        assert!(map.contains_key("a2"));
    }
}
