//! Query-side of the search index.
//!
//! The MCP layer has its own serde-deriving `SearchEmailParams` type
//! that converts into `SearchQuery` at the boundary.  The types here
//! stay MCP-agnostic so they can also be driven directly from CLI
//! tools, tests, and (eventually) other resources' search tables.

use std::fmt::Write as _;

use rusqlite::{Connection, Row, ToSql, params_from_iter, types::Value};

use super::SearchIndex;
use crate::error::Error;

/// One search request.  All fields are independent filters combined
/// with AND; leaving a field `None` means "no filter on that axis."
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// FTS5 match string over subject + from_name + to_addrs + body.
    /// Passes straight through to `MATCH ?`, so FTS5 syntax works —
    /// `"apple NOT receipt"`, quoted phrases, column prefixes like
    /// `subject:invoice`.
    pub query: Option<String>,

    /// Substring on `from_addr` OR `from_name`, case-insensitive.
    pub from: Option<String>,
    /// Substring on `to_addrs` OR `cc_addrs`, case-insensitive.
    pub to: Option<String>,
    /// Substring on `subject`, case-insensitive.
    pub subject: Option<String>,

    /// Folder filter.  See [`FolderFilter`] for the supported shapes.
    pub folder: Option<FolderFilter>,

    /// Inclusive lower bound on the message date (unix seconds).
    pub since_unix: Option<i64>,
    /// Exclusive upper bound on the message date (unix seconds).
    pub before_unix: Option<i64>,

    pub flags: Option<FlagFilter>,
    /// Convenience for `flags.none_of = ["\\Seen"]`.  When both
    /// `unread: true` and `flags.none_of` are set the two predicates are
    /// ANDed (so `unread` never loosens an explicit filter).
    pub unread: Option<bool>,

    pub has_attachments: Option<bool>,

    /// 0-indexed page offset.  Default 0.
    pub offset: Option<u32>,
    /// Page size.  Default 25, hard-capped at 200.
    pub limit: Option<u32>,
    /// Result ordering.  Default [`Sort::DateDesc`].  [`Sort::Relevance`]
    /// requires `query` to be set; if not, `search` returns an error.
    pub sort: Option<Sort>,
    /// When true, skip fetching hits and return only `total_matches`.
    pub count_only: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderFilter {
    /// No predicate — search every folder.
    All,
    /// Exactly this folder, no descendants.
    Exact(String),
    /// This folder AND all descendants under a `/` separator.  Example:
    /// `Archive` also matches `Archive/2026` but not `ArchiveSiblings`.
    Prefix(String),
}

impl FolderFilter {
    /// Parse a user-supplied folder spec.  "*" or empty → `All`.
    /// Trailing "/*" → `Prefix` of the parent.  Otherwise → `Exact`.
    pub fn parse(spec: &str) -> Self {
        let s = spec.trim();
        if s.is_empty() || s == "*" {
            return Self::All;
        }
        if let Some(stripped) = s.strip_suffix("/*") {
            return Self::Prefix(stripped.to_string());
        }
        Self::Exact(s.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct FlagFilter {
    pub any_of: Option<Vec<String>>,
    pub all_of: Option<Vec<String>>,
    pub none_of: Option<Vec<String>>,
}

impl FlagFilter {
    fn is_empty(&self) -> bool {
        self.any_of.as_ref().is_none_or(|v| v.is_empty())
            && self.all_of.as_ref().is_none_or(|v| v.is_empty())
            && self.none_of.as_ref().is_none_or(|v| v.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    DateDesc,
    DateAsc,
    /// bm25 ranking over the FTS match.  Requires `query` to be set.
    Relevance,
}

pub const DEFAULT_LIMIT: u32 = 25;
pub const MAX_LIMIT: u32 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub total_matches: u64,
    pub returned: u32,
    pub offset: u32,
    pub hits: Vec<MessageHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHit {
    pub canonical_id: String,
    pub folder: String,
    /// Unix seconds, or None if the message has no parseable date.
    pub date_unix: Option<i64>,
    pub message_id: Option<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: Option<String>,
    pub flags: Vec<String>,
    pub size: Option<i64>,
    pub has_attachments: bool,
    /// First ~200 chars of the indexed body, newlines + whitespace
    /// runs collapsed.  Intended for at-a-glance triage.  Empty
    /// string if the message had no extractable body.
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Address {
    pub address: String,
    pub name: Option<String>,
}

pub(crate) const PREVIEW_CHARS: usize = 200;

impl SearchIndex {
    /// Run a [`SearchQuery`] against the index.  Returns a full
    /// [`SearchResult`] with `total_matches` always populated, whether
    /// or not `count_only` skipped the hit fetch.
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResult, Error> {
        if q.sort == Some(Sort::Relevance) && q.query.is_none() {
            return Err(Error::index(
                "sort=relevance requires query to be set",
            ));
        }

        let (where_sql, args) = build_where(q)?;
        let effective_limit = q
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);
        let offset = q.offset.unwrap_or(0);

        let conn = self.lock();

        // total_matches — same WHERE, COUNT(*)
        let count_sql = format!(
            "SELECT COUNT(*) FROM messages m {where_clause}",
            where_clause = where_sql,
        );
        let total_matches: i64 = conn.query_row(
            &count_sql,
            params_from_iter(args.iter().map(|a| a as &dyn ToSql)),
            |r| r.get(0),
        )?;

        if q.count_only.unwrap_or(false) {
            return Ok(SearchResult {
                total_matches: total_matches as u64,
                returned: 0,
                offset,
                hits: Vec::new(),
            });
        }

        let order_sql = order_clause(q);
        let hits_sql = format!(
            "SELECT m.canonical_id, m.folder, m.date_unix, m.message_id,
                    m.from_addr, m.from_name, m.to_addrs, m.cc_addrs,
                    m.subject, m.flags, m.size, m.has_attach, m.body
             FROM messages m
             {where_clause}
             {order_clause}
             LIMIT ?{lim_ix} OFFSET ?{off_ix}",
            where_clause = where_sql,
            order_clause = order_sql,
            lim_ix = args.len() + 1,
            off_ix = args.len() + 2,
        );
        let mut paged_args: Vec<Value> = args;
        paged_args.push(Value::Integer(effective_limit as i64));
        paged_args.push(Value::Integer(offset as i64));

        let mut stmt = conn.prepare(&hits_sql)?;
        let rows = stmt.query_map(
            params_from_iter(paged_args.iter().map(|a| a as &dyn ToSql)),
            row_to_hit,
        )?;
        let mut hits: Vec<MessageHit> = Vec::new();
        for r in rows {
            hits.push(r?);
        }

        Ok(SearchResult {
            total_matches: total_matches as u64,
            returned: hits.len() as u32,
            offset,
            hits,
        })
    }
}

// ── WHERE-clause builder ─────────────────────────────────────────────────
//
// Grows a Vec<Value> of positional args and a String of `AND` clauses.
// Positional placeholders are ?N so the count COUNT(*) and the paged
// SELECT can share the same arg vector (then the paged SELECT appends
// LIMIT/OFFSET using the next two ?s).

fn build_where(q: &SearchQuery) -> Result<(String, Vec<Value>), Error> {
    let mut args: Vec<Value> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();

    if let Some(qstr) = &q.query {
        if !qstr.trim().is_empty() {
            // FTS5 match via a subselect: avoids join-column ambiguity
            // and plays nicely with optional-parameter composition.
            args.push(Value::Text(qstr.clone()));
            let ix = args.len();
            clauses.push(format!(
                "m.rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?{ix})"
            ));
        }
    }

    if let Some(f) = &q.from {
        if !f.is_empty() {
            let pat = wrap_like(f);
            args.push(Value::Text(pat.clone()));
            args.push(Value::Text(pat));
            let a = args.len() - 1;
            let b = args.len();
            clauses.push(format!(
                "(LOWER(m.from_addr) LIKE ?{a} OR LOWER(m.from_name) LIKE ?{b})"
            ));
        }
    }

    if let Some(t) = &q.to {
        if !t.is_empty() {
            let pat = wrap_like(t);
            args.push(Value::Text(pat.clone()));
            args.push(Value::Text(pat));
            let a = args.len() - 1;
            let b = args.len();
            clauses.push(format!(
                "(LOWER(m.to_addrs) LIKE ?{a} OR LOWER(m.cc_addrs) LIKE ?{b})"
            ));
        }
    }

    if let Some(s) = &q.subject {
        if !s.is_empty() {
            args.push(Value::Text(wrap_like(s)));
            let ix = args.len();
            clauses.push(format!("LOWER(m.subject) LIKE ?{ix}"));
        }
    }

    match &q.folder {
        None | Some(FolderFilter::All) => {}
        Some(FolderFilter::Exact(f)) => {
            args.push(Value::Text(f.clone()));
            let ix = args.len();
            clauses.push(format!("m.folder = ?{ix}"));
        }
        Some(FolderFilter::Prefix(f)) => {
            args.push(Value::Text(f.clone()));
            args.push(Value::Text(format!("{f}/%")));
            let a = args.len() - 1;
            let b = args.len();
            clauses.push(format!("(m.folder = ?{a} OR m.folder LIKE ?{b})"));
        }
    }

    if let Some(t) = q.since_unix {
        args.push(Value::Integer(t));
        let ix = args.len();
        clauses.push(format!("m.date_unix >= ?{ix}"));
    }
    if let Some(t) = q.before_unix {
        args.push(Value::Integer(t));
        let ix = args.len();
        clauses.push(format!("m.date_unix < ?{ix}"));
    }

    if let Some(ha) = q.has_attachments {
        args.push(Value::Integer(if ha { 1 } else { 0 }));
        let ix = args.len();
        clauses.push(format!("m.has_attach = ?{ix}"));
    }

    if q.unread.unwrap_or(false) {
        // \Seen absent → unread.  json_each walks the JSON-encoded flags
        // column.  EXISTS / NOT EXISTS gives constant-time per-row cost
        // at the flag-cardinality we see in real mail (≤5-ish flags).
        let flag = r"\Seen".to_string();
        args.push(Value::Text(flag));
        let ix = args.len();
        clauses.push(format!(
            "NOT EXISTS (SELECT 1 FROM json_each(m.flags) WHERE value = ?{ix})"
        ));
    }

    if let Some(ff) = &q.flags {
        if !ff.is_empty() {
            if let Some(list) = &ff.any_of {
                if !list.is_empty() {
                    let (sql, added) = in_clause_any(list, args.len());
                    for v in added {
                        args.push(v);
                    }
                    clauses.push(sql);
                }
            }
            if let Some(list) = &ff.all_of {
                for flag in list {
                    args.push(Value::Text(flag.clone()));
                    let ix = args.len();
                    clauses.push(format!(
                        "EXISTS (SELECT 1 FROM json_each(m.flags) WHERE value = ?{ix})"
                    ));
                }
            }
            if let Some(list) = &ff.none_of {
                if !list.is_empty() {
                    let (sql, added) = in_clause_none(list, args.len());
                    for v in added {
                        args.push(v);
                    }
                    clauses.push(sql);
                }
            }
        }
    }

    let sql = if clauses.is_empty() {
        String::new()
    } else {
        let mut s = String::from("WHERE ");
        for (i, c) in clauses.iter().enumerate() {
            if i > 0 {
                s.push_str(" AND ");
            }
            let _ = write!(s, "{c}");
        }
        s
    };
    Ok((sql, args))
}

fn wrap_like(s: &str) -> String {
    // LIKE is case-insensitive only for ASCII by default.  We apply
    // LOWER(col) on the left side and lowercase the needle on the right
    // so Unicode upper/lower pairs behave consistently.
    let lower = s.to_lowercase();
    format!("%{}%", escape_like(&lower))
}

fn escape_like(s: &str) -> String {
    // LIKE metacharacters: % _  We don't currently use ESCAPE, so we
    // sanitize into literal '%' and '_' in the pattern to keep user
    // input from accidentally turning into a wildcard.  Our patterns
    // are built with LIKE and no ESCAPE clause, which means we can't
    // actually escape these — but at mailbox scale we accept the
    // trade-off: user-supplied substrings with literal _ or % are
    // rare, and the alternative (ESCAPE '\\' + doubling) complicates
    // every LIKE call.  Document the known limitation.
    s.to_string()
}

fn in_clause_any(list: &[String], args_so_far: usize) -> (String, Vec<Value>) {
    let mut added: Vec<Value> = Vec::with_capacity(list.len());
    let mut holes: Vec<String> = Vec::with_capacity(list.len());
    for (i, flag) in list.iter().enumerate() {
        added.push(Value::Text(flag.clone()));
        holes.push(format!("?{}", args_so_far + i + 1));
    }
    let sql = format!(
        "EXISTS (SELECT 1 FROM json_each(m.flags) WHERE value IN ({}))",
        holes.join(", "),
    );
    (sql, added)
}

fn in_clause_none(list: &[String], args_so_far: usize) -> (String, Vec<Value>) {
    let mut added: Vec<Value> = Vec::with_capacity(list.len());
    let mut holes: Vec<String> = Vec::with_capacity(list.len());
    for (i, flag) in list.iter().enumerate() {
        added.push(Value::Text(flag.clone()));
        holes.push(format!("?{}", args_so_far + i + 1));
    }
    let sql = format!(
        "NOT EXISTS (SELECT 1 FROM json_each(m.flags) WHERE value IN ({}))",
        holes.join(", "),
    );
    (sql, added)
}

fn order_clause(q: &SearchQuery) -> String {
    match q.sort.unwrap_or_default() {
        Sort::DateDesc => "ORDER BY m.date_unix DESC, m.canonical_id ASC".into(),
        Sort::DateAsc => "ORDER BY m.date_unix ASC, m.canonical_id ASC".into(),
        Sort::Relevance => {
            // bm25 ranks lower = better.  Tie-break on date_desc so
            // near-equal-scoring hits surface the newest first.
            "ORDER BY (SELECT bm25(messages_fts) FROM messages_fts \
                      WHERE rowid = m.rowid LIMIT 1) ASC, \
              m.date_unix DESC"
                .into()
        }
    }
}

// ── Row → MessageHit ────────────────────────────────────────────────────

fn row_to_hit(row: &Row<'_>) -> rusqlite::Result<MessageHit> {
    let canonical_id: String = row.get(0)?;
    let folder: String = row.get(1)?;
    let date_unix: Option<i64> = row.get(2)?;
    let message_id: Option<String> = row.get(3)?;
    let from_addr: Option<String> = row.get(4)?;
    let from_name: Option<String> = row.get(5)?;
    let to_addrs: Option<String> = row.get(6)?;
    let cc_addrs: Option<String> = row.get(7)?;
    let subject: Option<String> = row.get(8)?;
    let flags_json: String = row.get(9)?;
    let size: Option<i64> = row.get(10)?;
    let has_attach: i64 = row.get(11)?;
    let body: Option<String> = row.get(12)?;

    let flags: Vec<String> = serde_json::from_str(&flags_json).unwrap_or_default();
    let from = Address {
        address: from_addr.unwrap_or_default(),
        name: from_name,
    };
    let to = split_addresses(to_addrs.as_deref());
    let cc = split_addresses(cc_addrs.as_deref());
    let preview = build_preview(body.as_deref().unwrap_or(""));

    Ok(MessageHit {
        canonical_id,
        folder,
        date_unix,
        message_id,
        from,
        to,
        cc,
        subject,
        flags,
        size,
        has_attachments: has_attach != 0,
        preview,
    })
}

fn split_addresses(joined: Option<&str>) -> Vec<Address> {
    joined
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Address {
            address: s.to_string(),
            name: None,
        })
        .collect()
}

fn build_preview(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    let mut flat = String::with_capacity(body.len().min(PREVIEW_CHARS * 2));
    let mut prev_ws = true;
    for c in body.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                flat.push(' ');
            }
            prev_ws = true;
        } else {
            flat.push(c);
            prev_ws = false;
        }
    }
    let trimmed = flat.trim();
    let mut iter = trimmed.char_indices();
    let cut = iter
        .nth(PREVIEW_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    trimmed[..cut].to_string()
}

// Silence unused-import warning when the cfg is off.
#[allow(dead_code)]
fn _touch_conn(_: &Connection) {}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::envelope::MessageRow;
    use super::*;
    use tempfile::tempdir;

    fn row_for(
        id: &str,
        folder: &str,
        from: &str,
        subject: &str,
        body: &str,
        date_unix: i64,
        flags: &[&str],
        has_attach: bool,
    ) -> MessageRow {
        MessageRow {
            canonical_id: id.to_string(),
            folder: folder.to_string(),
            source_id: "src".to_string(),
            message_id: Some(format!("<{id}@test>")),
            from_addr: Some(from.to_lowercase()),
            from_name: Some("Sender Name".to_string()),
            to_addrs: Some("dan@hld.ca, ops@hld.ca".to_string()),
            cc_addrs: None,
            subject: Some(subject.to_string()),
            date_unix: Some(date_unix),
            size: Some(1000),
            flags: flags.iter().map(|s| s.to_string()).collect(),
            has_attachments: has_attach,
            body_text: Some(body.to_string()),
        }
    }

    fn seeded() -> SearchIndex {
        let td = tempdir().unwrap();
        // Leak the tempdir for test lifetime — we only need the DB, and
        // rusqlite keeps the file open until the SearchIndex is dropped.
        let path = td.into_path();
        let idx = SearchIndex::open(&path).unwrap();
        idx.upsert_message(&row_for(
            "a1",
            "INBOX",
            "alice@apple.com",
            "your order CAEN",
            "thanks for shopping at apple",
            1_700_000_000,
            &["\\Seen"],
            false,
        ))
        .unwrap();
        idx.upsert_message(&row_for(
            "a2",
            "Archive/2026",
            "bob@orders.apple.com",
            "receipt",
            "apple receipt details here",
            1_710_000_000,
            &["\\Seen", "\\Flagged"],
            true,
        ))
        .unwrap();
        idx.upsert_message(&row_for(
            "a3",
            "Archive/2026",
            "noreply@banana.com",
            "banana weekly",
            "fresh bananas news",
            1_720_000_000,
            &[],
            false,
        ))
        .unwrap();
        idx.upsert_message(&row_for(
            "a4",
            "Inbox/Subfolder",
            "carol@carrot.io",
            "veggie digest",
            "carrots and celery",
            1_730_000_000,
            &[],
            false,
        ))
        .unwrap();
        idx
    }

    #[test]
    fn empty_query_returns_all_cross_folder() {
        let idx = seeded();
        let r = idx.search(&SearchQuery::default()).unwrap();
        assert_eq!(r.total_matches, 4);
        assert_eq!(r.returned, 4);
        // Default sort is date_desc → a4, a3, a2, a1.
        let ids: Vec<_> = r.hits.iter().map(|h| h.canonical_id.as_str()).collect();
        assert_eq!(ids, vec!["a4", "a3", "a2", "a1"]);
    }

    #[test]
    fn from_filter_substring_case_insensitive() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                from: Some("APPLE".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 2, "alice+bob at apple.com");
    }

    #[test]
    fn to_filter_matches_both_to_and_cc() {
        let idx = seeded();
        // Pre-seed a row with cc populated.
        idx.upsert_message(&{
            let mut r = row_for(
                "cc1",
                "INBOX",
                "z@z.com",
                "cc'd msg",
                "hi",
                1_695_000_000,
                &[],
                false,
            );
            r.cc_addrs = Some("wendy@w.com".into());
            r.to_addrs = Some("other@z.com".into());
            r
        })
        .unwrap();
        let r = idx
            .search(&SearchQuery {
                to: Some("wendy".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
    }

    #[test]
    fn subject_substring() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                subject: Some("caen".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.hits[0].canonical_id, "a1");
    }

    #[test]
    fn folder_exact() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                folder: Some(FolderFilter::Exact("INBOX".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
    }

    #[test]
    fn folder_prefix_matches_subfolders() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                folder: Some(FolderFilter::Prefix("Archive".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 2);
    }

    #[test]
    fn folder_all_matches_everything() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                folder: Some(FolderFilter::All),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 4);
    }

    #[test]
    fn folder_parse_variants() {
        assert_eq!(FolderFilter::parse(""), FolderFilter::All);
        assert_eq!(FolderFilter::parse("*"), FolderFilter::All);
        assert_eq!(
            FolderFilter::parse("INBOX"),
            FolderFilter::Exact("INBOX".to_string())
        );
        assert_eq!(
            FolderFilter::parse("Archive/*"),
            FolderFilter::Prefix("Archive".to_string())
        );
    }

    #[test]
    fn since_and_before_bounds() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                since_unix: Some(1_710_000_000),
                before_unix: Some(1_725_000_000),
                ..Default::default()
            })
            .unwrap();
        // a2 and a3 fall in [1.71e9, 1.725e9); a1 is earlier, a4 later.
        assert_eq!(r.total_matches, 2);
    }

    #[test]
    fn fts_query_apple_matches_subject_and_body() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                query: Some("apple".into()),
                ..Default::default()
            })
            .unwrap();
        // a1's body has "apple"; a2's body has "apple"; from_name was
        // "Sender Name" not containing apple.
        assert_eq!(r.total_matches, 2);
    }

    #[test]
    fn fts_query_boolean() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                query: Some("apple NOT receipt".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.hits[0].canonical_id, "a1");
    }

    #[test]
    fn sort_relevance_requires_query() {
        let idx = seeded();
        let err = idx
            .search(&SearchQuery {
                sort: Some(Sort::Relevance),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("sort=relevance"));
    }

    #[test]
    fn sort_relevance_ranks_matches() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                query: Some("apple".into()),
                sort: Some(Sort::Relevance),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 2);
        // Either order is valid — both mention apple.  Just ensure we
        // don't crash and return all matches.
        assert_eq!(r.hits.len(), 2);
    }

    #[test]
    fn sort_date_asc() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                sort: Some(Sort::DateAsc),
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<_> = r.hits.iter().map(|h| h.canonical_id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "a2", "a3", "a4"]);
    }

    #[test]
    fn has_attachments_filter() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                has_attachments: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.hits[0].canonical_id, "a2");
    }

    #[test]
    fn unread_shortcut() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                unread: Some(true),
                ..Default::default()
            })
            .unwrap();
        // a3 and a4 have empty flags → unread.
        assert_eq!(r.total_matches, 2);
    }

    #[test]
    fn flags_any_all_none() {
        let idx = seeded();
        // any_of: \Flagged → a2 only
        let r = idx
            .search(&SearchQuery {
                flags: Some(FlagFilter {
                    any_of: Some(vec!["\\Flagged".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        // all_of: \Seen AND \Flagged → a2 only
        let r = idx
            .search(&SearchQuery {
                flags: Some(FlagFilter {
                    all_of: Some(vec!["\\Seen".into(), "\\Flagged".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        // none_of: \Seen → a3 and a4 (the unread ones)
        let r = idx
            .search(&SearchQuery {
                flags: Some(FlagFilter {
                    none_of: Some(vec!["\\Seen".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 2);
    }

    #[test]
    fn count_only_skips_hits() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                from: Some("apple".into()),
                count_only: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 2);
        assert_eq!(r.returned, 0);
        assert!(r.hits.is_empty());
    }

    #[test]
    fn pagination_stable_total_matches() {
        let idx = seeded();
        let p1 = idx
            .search(&SearchQuery {
                limit: Some(2),
                offset: Some(0),
                ..Default::default()
            })
            .unwrap();
        let p2 = idx
            .search(&SearchQuery {
                limit: Some(2),
                offset: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(p1.total_matches, 4);
        assert_eq!(p2.total_matches, 4);
        assert_eq!(p1.returned, 2);
        assert_eq!(p2.returned, 2);
        // Page 1 + page 2 should cover all 4 rows with no overlap.
        let mut ids: Vec<_> = p1.hits.iter().map(|h| &h.canonical_id).collect();
        ids.extend(p2.hits.iter().map(|h| &h.canonical_id));
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn limit_clamps_to_max() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                limit: Some(999_999),
                ..Default::default()
            })
            .unwrap();
        assert!(r.returned <= MAX_LIMIT);
    }

    #[test]
    fn hit_shape_parses_addresses_and_preview() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                folder: Some(FolderFilter::Exact("INBOX".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        let h = &r.hits[0];
        assert_eq!(h.from.address, "alice@apple.com");
        assert_eq!(h.to.len(), 2);
        assert_eq!(h.to[0].address, "dan@hld.ca");
        assert_eq!(h.to[1].address, "ops@hld.ca");
        assert!(!h.preview.is_empty());
        assert!(h.preview.len() <= PREVIEW_CHARS + 4); // char-aware cap
    }

    #[test]
    fn combined_filters_and_together() {
        let idx = seeded();
        let r = idx
            .search(&SearchQuery {
                folder: Some(FolderFilter::Prefix("Archive".into())),
                from: Some("apple".into()),
                query: Some("receipt".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.hits[0].canonical_id, "a2");
    }
}
