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

use rusqlite::Connection;

use crate::error::Error;

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

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // PoisonError here would mean a previous writer panicked mid-txn.
        // The DB is transactionally safe, so we just unpoison and carry
        // on — no silent corruption risk.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }
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
    // Must be one batch: the triggers reference messages_fts which
    // depends on messages; all DDL in the same transaction keeps the
    // file atomically either "v1 complete" or "empty".
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

        CREATE TABLE messages_body (
            rowid INTEGER PRIMARY KEY,
            body  TEXT NOT NULL
        );

        CREATE VIRTUAL TABLE messages_fts USING fts5(
            subject, from_name, to_addrs, body,
            content='messages',
            content_rowid='rowid',
            tokenize = 'unicode61 remove_diacritics 2'
        );

        CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
            VALUES (new.rowid, new.subject, new.from_name, new.to_addrs,
                    COALESCE((SELECT body FROM messages_body WHERE rowid = new.rowid), ''));
        END;

        CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs,
                    COALESCE((SELECT body FROM messages_body WHERE rowid = old.rowid), ''));
            DELETE FROM messages_body WHERE rowid = old.rowid;
        END;

        CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
            VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs,
                    COALESCE((SELECT body FROM messages_body WHERE rowid = old.rowid), ''));
            INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
            VALUES (new.rowid, new.subject, new.from_name, new.to_addrs,
                    COALESCE((SELECT body FROM messages_body WHERE rowid = new.rowid), ''));
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
        for obj in ["messages", "messages_body", "messages_fts", "schema_version"] {
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
}
