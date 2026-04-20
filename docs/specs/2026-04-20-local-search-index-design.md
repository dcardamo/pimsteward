# Local search index for mail

**Status:** design approved, pre-implementation
**Date:** 2026-04-20
**Scope:** mail only in v1; schema & file layout leave room for calendar and contacts later

## Problem

Pimsteward's `search_email` MCP tool is a thin pass-through to forwardemail's `/v1/messages` REST endpoint. That endpoint has three properties that make it unfit as an agent's primary mail-search interface:

1. **Scoped to INBOX when `folder` is omitted.** There is no "all folders" mode. Mail moved to Archive, Archive/2026, or any user-created folder is invisible to the default search call.
2. **Inefficient.** Pimsteward already mirrors every message to disk as `.eml` + `.meta.json`. Round-tripping to forwardemail to re-search bytes we already own is waste.
3. **Inconsistent with the local store.** When a pull produces local divergence (flag changes, moves) the REST search lags until forwardemail reindexes.

Rocky (the hermes agent driving pimsteward) hit this looking for `your_order_CAEN@orders.apple.com` in `Archive/2026`. Forwardemail returned zero; the emails were in fact there on disk. The agent has no clean workaround: it can't iterate folders (there are many), and it can't reach around forwardemail without violating the isolation boundary.

## Goals

1. Replace the current `search_email` tool with a local-first implementation backed by a SQLite + FTS5 index sidecar of the git-tracked mail backup.
2. Regenerable from the git repo any time, without any network call or server reachability.
3. Kept continuously in sync with the on-disk state: new messages, moves, flag changes, deletes.
4. Resumable: rebuild over tens of thousands of messages survives OOM, SIGTERM, container restart without losing earlier progress.
5. Self-healing: pimsteward's daemon detects drift between disk and index and auto-rebuilds.
6. No change to the security boundary. Clients continue to interact only through the MCP HTTP interface. No outside process reads the SQLite file.

## Non-goals (v1)

- Calendar and contacts search. Design leaves room for them; shipping mail first.
- Thread grouping, conversation view, participant rollups, reply-chain summaries.
- Multi-process write contention. The daemon is the only writer.
- SQLite corruption recovery. Defense is `pimsteward index rebuild --force`.

## Decisions locked during brainstorming

| Decision                      | Choice                                                                   |
| ----------------------------- | ------------------------------------------------------------------------ |
| Tool replacement strategy     | Replace `search_email` outright. No REST fallback.                        |
| Scope                         | Mail only now. Schema and file structured to admit calendar/contacts.    |
| Index file location           | `<repo_root>/.pimsteward/search_index.sqlite`. `.gitignore` excluded.    |
| Rebuild orphan handling       | Sweep orphan rows at end of a full rebuild.                               |
| Commit cadence during rebuild | Every 200 messages within a folder, committed per-folder at boundary.    |
| MCP interface                 | Redesigned from scratch; not constrained by the forwardemail shape.      |
| `FlagFilter` shape            | Full `{any_of, all_of, none_of}` in v1.                                  |
| Body-text cap                 | 128 KB per message in the FTS table.                                     |
| Connection sharing            | Single `Mutex<Connection>` inside `SearchIndex`, shared reader+writer.   |
| Self-heal thresholds          | Auto-rebuild when `|idx - disk| > 2% AND > 100 rows`.                    |

## Architecture

### File layout

```
<repo_root>/                          # git-tracked backup
  mail/
    <folder_safe>/
      <canonical_id>.eml              # source of truth
      <canonical_id>.meta.json        # flags, modseq, uid, folder_path, ...
      <canonical_id>.attachments.json
      _folder.json
  .pimsteward/                        # NOT git-tracked
    search_index.sqlite
    search_index.sqlite-wal
    search_index.sqlite-shm
  .gitignore                          # includes `.pimsteward/`
```

`.pimsteward/` is created by the index module on first open. If the repo's `.gitignore` does not already contain `.pimsteward/`, pimsteward appends a line. Existing contents are never rewritten.

### Module layout

```
src/
  index/
    mod.rs         # SearchIndex type, schema, search/upsert/delete/rebuild/verify/stats
    envelope.rs    # .eml → MessageRow parsing (mailparse-based)
```

### Core type

```rust
pub struct SearchIndex {
    conn: std::sync::Mutex<rusqlite::Connection>,
    root: PathBuf,
}

impl SearchIndex {
    pub fn open(repo_root: &Path) -> Result<Self>;
    pub fn upsert_message(&self, r: &MessageRow) -> Result<()>;
    pub fn delete_message(&self, canonical_id: &str) -> Result<()>;
    pub fn delete_folder(&self, folder: &str) -> Result<u64>;
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResult>;
    pub fn rebuild_from_disk(&self, repo_root: &Path, opts: RebuildOpts) -> Result<RebuildStats>;
    pub fn stats(&self) -> Result<IndexStats>;
    pub fn verify(&self, repo_root: &Path, clean: bool) -> Result<VerifyReport>;
    pub fn begin_folder_txn(&self) -> Result<FolderTxn<'_>>;
    pub fn message_count(&self) -> Result<u64>;
}
```

The `Mutex` wraps a single long-lived `rusqlite::Connection`. Pull-loop writes and MCP reads both acquire the mutex briefly. SQLite's WAL journal mode lets reads proceed concurrently *within* SQLite, but rusqlite isn't `Send + Sync` across a single connection, so a Mutex around one is simpler than a pool and fits pimsteward's single-process model. All `SearchIndex` methods take `&self` — the interior `Mutex` makes the type `Sync` without requiring every caller to thread `&mut`.

**Per-alias indexes.** Each alias's `storage.repo_path` has its own `.pimsteward/search_index.sqlite`. Today's pimsteward deployment runs one process per alias (rocky@hld.ca on port 8100, dan@hld.ca on port 8101), so a single process holds a single index. If a future deployment multiplexes aliases in one process, each gets its own `SearchIndex` keyed by repo_path.

### Dependencies

Added to `Cargo.toml`:

- `rusqlite = { version = "0.32", features = ["bundled"] }` — bundled SQLite statically linked. Guarantees FTS5 is compiled in and avoids NixOS shared-library linkage surprises.
- `mailparse = "0.15"` — RFC822 parsing + MIME walking for envelope and body-text extraction.

## Schema

```sql
CREATE TABLE schema_version (v INTEGER NOT NULL);
INSERT INTO schema_version VALUES (1);

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

-- Triggers: keep messages_fts and messages_body synchronized with messages.
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
  VALUES (new.rowid, new.subject, new.from_name, new.to_addrs,
          (SELECT body FROM messages_body WHERE rowid = new.rowid));
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
  VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs,
          (SELECT body FROM messages_body WHERE rowid = old.rowid));
  DELETE FROM messages_body WHERE rowid = old.rowid;
END;

CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, subject, from_name, to_addrs, body)
  VALUES ('delete', old.rowid, old.subject, old.from_name, old.to_addrs,
          (SELECT body FROM messages_body WHERE rowid = old.rowid));
  INSERT INTO messages_fts(rowid, subject, from_name, to_addrs, body)
  VALUES (new.rowid, new.subject, new.from_name, new.to_addrs,
          (SELECT body FROM messages_body WHERE rowid = new.rowid));
END;
```

**Why a split `messages_body` table**: FTS5 contentless mode keeps non-body columns mirrored from `messages` automatically, but the body text is large and we don't want it duplicated in the non-FTS row. Keeping it in a separate `messages_body` table, inserted in the same transaction as `messages`, lets the triggers reach it when refreshing FTS.

**Body cap**: the envelope parser truncates the extracted plain text to 128 KB before insert. Ottawa Lookout-grade HTML newsletters strip to ~50–80 KB of text, so 128 KB comfortably fits real content while bounding DB growth against spam or mail-bombs.

**Pragmas set on open**: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, `temp_store=MEMORY`.

### MessageRow

```rust
pub struct MessageRow {
    pub canonical_id: String,
    pub folder: String,
    pub source_id: String,
    pub message_id: Option<String>,
    pub from_addr: Option<String>,       // lowercased
    pub from_name: Option<String>,
    pub to_addrs: Option<String>,        // lowercased, comma-joined
    pub cc_addrs: Option<String>,        // lowercased, comma-joined
    pub subject: Option<String>,
    pub date_unix: Option<i64>,
    pub size: Option<i64>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    pub body_text: Option<String>,       // capped at 128 KB by envelope.rs
}
```

## Pull-loop integration

`src/pull/mail.rs::pull_mail` opens the index once at start and passes `&SearchIndex` down.

```rust
let index = SearchIndex::open(repo.root())?;
for folder in folders {
    sync_one_folder(source, repo, &folder, &index, ...).await?;
}
cleanup_removed_folders(repo, &folders, &index)?;
```

Inside `sync_one_folder`:

1. Open a per-folder transaction: `let txn = index.begin_folder_txn()?;`
2. After each successful `.eml` + `meta.json` write (existing code at line ~324): parse the envelope, call `txn.upsert_message(&row)`. Commit the txn every 200 messages (intra-folder flush) to bound crash loss.
3. After each deletion (existing per-canonical loop at lines ~358–376): `txn.delete_message(&canonical)`.
4. `txn.commit()` at end of folder.

Inside `cleanup_removed_folders`: for each folder being purged, `index.delete_folder(&folder)?`. Runs outside per-folder txn scope because it crosses folders.

**Error policy**: index write failures are logged as `tracing::warn!` and **do not fail the pull**. The `.eml` on disk is the source of truth; a broken index is always recoverable via `pimsteward index rebuild`. Mail backup must never be gated on search-index health.

**Move semantics**: a move in forwardemail is observed as a delete from the old folder + an add in the new folder. Because `canonical_id = sha256(Message-ID)[..16]` is folder-independent, the same message ends up with a new row (new folder) and the old row is deleted — all within the same `pull-mail` run. Index stays consistent across moves with no special handling.

**Flag changes**: pull-loop's existing `needs_refetch` check already triggers a re-fetch on flag changes. The new .eml write triggers an upsert, which refreshes the `flags` column.

## MCP tool — `search_email` (replaced, not added)

### Request

```rust
pub struct SearchEmailParams {
    pub query:            Option<String>,   // FTS5 syntax over subject + body + from_name + to_addrs
    pub from:             Option<String>,   // substring on from_addr OR from_name, case-insensitive
    pub to:               Option<String>,   // substring on to_addrs + cc_addrs
    pub subject:          Option<String>,   // substring on subject
    pub folder:           Option<String>,   // exact path, glob "Archive/*", or "*" for all (default)
    pub since:            Option<String>,   // RFC3339; inclusive lower bound on date
    pub before:           Option<String>,   // RFC3339; exclusive upper bound
    pub flags:            Option<FlagFilter>,
    pub unread:           Option<bool>,     // shortcut for flags.none_of=["\\Seen"]
    pub has_attachments:  Option<bool>,
    pub offset:           Option<u32>,      // 0-indexed; default 0
    pub limit:            Option<u32>,      // default 25, max 200
    pub sort:             Option<Sort>,     // date_desc (default) | date_asc | relevance
    pub count_only:       Option<bool>,     // if true, hits is [] and only total_matches is returned
}

pub struct FlagFilter {
    pub any_of:  Option<Vec<String>>,
    pub all_of:  Option<Vec<String>>,
    pub none_of: Option<Vec<String>>,
}

pub enum Sort { DateDesc, DateAsc, Relevance }
```

Validation: `Sort::Relevance` requires `query` to be set; otherwise the call returns a `McpError` with a helpful message.

### Response

```json
{
  "total_matches": 142,
  "returned": 25,
  "offset": 0,
  "hits": [
    {
      "canonical_id": "776f139e04287b94",
      "folder": "Archive/2026",
      "date": "2026-04-20T11:02:18Z",
      "from": {"address": "hello@ottawalookout.com", "name": "Ottawa Lookout"},
      "to":   [{"address": "dan@hld.ca", "name": null}],
      "subject": "Twenty-five years later...",
      "message_id": "<abc123@mx.ottawalookout.com>",
      "flags": ["\\Seen"],
      "size": 612543,
      "has_attachments": false,
      "preview": "Happy Monday. Today marks 25 years since amalgamation..."
    }
  ]
}
```

- `preview` is the first 200 characters of the indexed body text, newlines and runs of whitespace squashed to single spaces. Enough to triage without a second MCP call.
- No `nodemailer`. No `modseq`, no `uid`. Clients that need IMAP primitives call `get_email`.
- Same `{total_matches, returned, offset, hits}` envelope will apply to future `search_calendar` / `search_contacts`.

### Query translation

| Param                    | SQL fragment                                                              |
| ------------------------ | ------------------------------------------------------------------------- |
| `query`                  | `rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)`    |
| `from`                   | `(from_addr LIKE ? OR from_name LIKE ?)`, both with wildcard wrap         |
| `to`                     | `(to_addrs LIKE ? OR cc_addrs LIKE ?)`                                    |
| `subject`                | `subject LIKE ? COLLATE NOCASE`                                           |
| `folder="*"` or None     | (no predicate)                                                            |
| `folder="X"` (no glob)   | `folder = ?`                                                              |
| `folder="X/*"`           | `(folder = 'X' OR folder LIKE 'X/%')`                                     |
| `since`/`before`         | `date_unix >= ?` / `date_unix < ?`                                        |
| `flags.any_of`           | JSON scan: `EXISTS (… json_each(flags) WHERE value IN (…))`               |
| `flags.all_of`           | One `EXISTS` per flag                                                     |
| `flags.none_of`          | `NOT EXISTS (… json_each(flags) WHERE value IN (…))`                      |
| `unread`                 | same as `flags.none_of=["\\Seen"]`                                        |
| `has_attachments`        | `has_attach = 1` or `= 0`                                                 |
| `sort=date_desc`         | `ORDER BY date_unix DESC, canonical_id ASC`                                |
| `sort=date_asc`          | `ORDER BY date_unix ASC, canonical_id ASC`                                 |
| `sort=relevance`         | `ORDER BY bm25(messages_fts)` (requires `query`)                          |

`total_matches` is computed by running the same WHERE clause wrapped in `SELECT COUNT(*)` prior to the paged SELECT. Single extra query, cheap given all filter columns are indexed.

## CLI — `pimsteward index`

```
pimsteward index rebuild [--force] [--alias <email>]
pimsteward index stat    [--alias <email>]
pimsteward index verify  [--clean] [--alias <email>]
```

- **`rebuild`**: incremental by default. For each `.eml` on disk, skip if a row already exists with `indexed_at >= file mtime`. Else parse + upsert. Commit every 200 messages per folder. After the full scan, sweep orphans: `DELETE FROM messages WHERE canonical_id NOT IN (<seen set>)`. Emits a JSON summary on stdout: `{scanned, upserted, skipped, orphaned_deleted, elapsed_ms, errors}`. `--force` drops `messages` + `messages_body` + FTS first.
- **`stat`**: JSON summary of DB path, byte size, schema version, row count, folder count, oldest/newest `date_unix`, and `last_indexed_at`.
- **`verify`**: dry-run inventory match. Lists `orphan_rows` and `unindexed_emls`. Exits 0 if both empty, else exits 1 (cron-friendly). `--clean` deletes orphans and upserts missing. Does not use mtime (that's rebuild's job); verify is a strict set comparison on `canonical_id`.

## Daemon self-heal

Inside `Command::Daemon` startup, after opening the index:

```rust
if !args.skip_index_rebuild {
    let disk = count_eml_files(repo.root())?;
    let idx  = index.message_count()?;
    let force = args.force_index_rebuild;
    let empty = idx == 0 && disk > 0;
    let drift = drift_triggers_rebuild(idx, disk, drift_pct_threshold, drift_row_threshold);
    if force || empty || drift {
        tracing::warn!(disk, idx, empty, drift, force, "index rebuild on startup");
        index.rebuild_from_disk(repo.root(), RebuildOpts::incremental())?;
    }
}
```

`drift_triggers_rebuild` returns true iff **both** `|idx - disk| / disk > 2%` AND `|idx - disk| > 100`. Both thresholds must trigger; prevents spurious rebuilds on small mailboxes where a single message is 1 % drift.

Env-var overrides (optional, unset → defaults):

- `PIMSTEWARD_INDEX_DRIFT_THRESHOLD_PCT` (default 2.0)
- `PIMSTEWARD_INDEX_DRIFT_THRESHOLD_ROWS` (default 100)
- `PIMSTEWARD_INDEX_COMMIT_BATCH` (default 200)

Daemon flags:

- `--skip-index-rebuild`: never rebuild at startup.
- `--force-index-rebuild`: rebuild at startup regardless of drift.

During rebuild, `tracing::info!` every 500 messages with `{scanned, upserted, skipped, elapsed_ms, rate_per_sec}`.

## Robustness characteristics

- **Resumable.** Incremental rebuild relies only on `(canonical_id, indexed_at)` already in the DB. SIGTERM / OOM / container restart drops at most the current in-flight folder's uncommitted batch of 200.
- **Regenerable.** `pimsteward index rebuild --force` produces the same DB state from disk. No network, no server, no external state.
- **Kept fresh.** Pull-loop upserts on every `.eml` write; deletes on every file removal; `delete_folder` on folder disappearance.
- **Self-healing.** Daemon startup detects `(idx_count, disk_count)` drift and auto-rebuilds incrementally.
- **Non-fatal.** Index write failures warn but never abort a pull.
- **Isolated.** SQLite file lives inside the pimsteward container's filesystem. Only pimsteward's in-process code touches it. MCP clients see only tool responses.

## Testing strategy

### Unit tests (in `src/index/*`)

- `envelope::parse_eml`: plain text, multipart/alternative, multipart/mixed with attachments, quoted-printable and base64 bodies, MIME-encoded Subject, missing Date (fall back to `internal_date` from meta), missing Message-ID (skip row and log), UTF-8 with emoji through FTS tokenization.
- `SearchIndex::open`: first call creates schema v1, second call is a no-op.
- Upsert idempotency: same row inserted twice yields one row with refreshed `indexed_at`.
- Delete removes from `messages`, `messages_body`, and FTS shadow (verified via explicit `SELECT` on all three).
- `delete_folder` removes exactly the rows for that folder, no collateral.
- Filter matrix: each of `from`/`to`/`subject`/`folder` glob / `since` / `before` / `flags{any_of, all_of, none_of}` / `unread` / `has_attachments` / `count_only` exercised alone, then combined.
- FTS: `query: "apple NOT receipt"`, phrase `"twenty-five years"`, column prefix `subject:invoice`, each returns expected subset.
- `sort=relevance` ranks by `bm25`; errors without `query`.
- Pagination: `offset + limit` traversal across a 100-row fixture, `total_matches` stable across pages.
- Rebuild incremental: two consecutive rebuilds — second reports 0 upserts.
- Rebuild resumability: drop `SearchIndex` mid-folder, reopen, resume — all files indexed, no duplicates.
- Rebuild sweeps orphans: pre-seed stale row, run rebuild, row gone.
- Verify detects both orphan rows and unindexed .eml.
- `drift_triggers_rebuild` threshold combinations.

### Integration tests (`tests/e2e_mail_search.rs`)

- Full pull → index populated → `search` returns expected hits.
- Move simulation: message moves from A to B; after next pull, search finds it only in B.
- Delete simulation: message gone upstream; after next pull, row gone.
- Folder-level delete: whole folder disappears; `delete_folder` sweeps rows.
- Cross-folder default: seed 3 folders each with an Apple email; `search(from="apple")` returns all three.
- Daemon startup auto-rebuild: wipe DB, start daemon-equivalent, confirm rebuild ran.
- Daemon startup no-rebuild: counts match, no rebuild invoked.

### MCP-level tests (`tests/e2e_mail.rs`, extension)

- Call the new `search_email` tool over MCP; verify response envelope.
- `count_only: true` returns `{total_matches, returned: 0, hits: []}`.

### Performance sanity (`#[ignore]`, run with `--release -- --ignored`)

- 10k-message synthetic corpus: rebuild < 60 s, searches < 50 ms. Not CI-gating; a regression anchor.

### Fixtures

New `tests/fixtures/mail/` directory with hand-crafted `.eml` files covering the parsing edge cases. Deterministic, no network.

## Open items deliberately not addressed

- Thread grouping / conversation view.
- Calendar and contacts search tools. Schema leaves a place for them; MCP envelope (`{total_matches, returned, offset, hits}`) designed to be reused.
- Multi-writer contention. Daemon is the sole writer today; revisit if that changes.
- SQLite corruption recovery beyond "rebuild it."

## Implementation order

1. `Cargo.toml` deps + empty `src/index/mod.rs` and `src/index/envelope.rs`.
2. Schema + `SearchIndex::open` + migrations table. Unit test that a second `open()` is idempotent.
3. `envelope::parse_eml` with the fixture matrix.
4. `upsert_message`, `delete_message`, `delete_folder`. Unit tests per operation.
5. `search` with the full filter matrix. Unit tests per filter, combined tests.
6. `rebuild_from_disk` incremental + `--force`. Resumability test.
7. `verify`, `stats`. Unit tests.
8. CLI subcommands (`pimsteward index {rebuild, stat, verify}`).
9. Pull-loop integration: pass `&SearchIndex` down, wire upsert/delete/delete_folder, per-folder transactions, 200-row intra-folder commits.
10. Daemon self-heal (drift detection + auto-rebuild) + `--skip-index-rebuild` / `--force-index-rebuild` flags.
11. MCP tool rewrite: replace `search_email` implementation; new params/response types; schemars derive.
12. MCP-level integration tests.
13. `cargo test && cargo clippy --all-targets -- -D warnings` clean.
14. Deploy to the pimsteward container via `make update` in dotfiles; verify via a live `search_email` call for the Apple/Archive bug that motivated this work.
