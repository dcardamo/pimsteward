# CalDAV Scheduling Agent (organizer-side iMIP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-extended-cc:subagent-driven-development (recommended) or superpowers-extended-cc:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make pimsteward send organizer-side iMIP scheduling messages (REQUEST for new/updated events, CANCEL for deletions) on Dan's behalf, since ForwardEmail's CalDAV server doesn't, with every send deduplicated and audited in git.

**Architecture:** A new `scheduling` module in the `pimsteward` crate hooks the existing calendar pull loop. After each pull commit, it diffs the commit against its parent to get changed `.ics` files, filters to events Dan organizes that are newer than an activation watermark, and for each sends an iMIP message built by a new `imip` module in the `pimsteward-ical` crate. A git-committed sent-ledger guarantees no double-sends across restarts.

**Tech Stack:** Rust 2024, tokio, the `pimsteward` crate (forwardemail REST client, git-backed `Repo` store, `CalendarSource`/`CalendarWriter` traits), the `pimsteward-ical` crate (dependency-light iCalendar helpers), `git` CLI via `std::process::Command`, ForwardEmail `POST /v1/emails` (`{from, raw}` MIME path).

**User decisions (already made):**
- Pure pimsteward; zero changes to rocky. "ok to do no changes to rocky, only changes in pimsteward."
- Fully automatic send, no hold window. "you can notify and send at the same time."
- Debug notify ON now: email dan@ on every send. "have pimsteward email me whenever it sends emails for a calendar event."
- No dry_run. "I think I'm ok to skip dry run."
- Never email for pre-existing events (watermark + past-DTSTART guard). "I don't want to do any emails for old/already created events."
- E2E acceptance gate using rocky@hld.ca as the sole invitee before going live. "can you do a real e2e test by creating ... and updating calendar events where rocky@hld.ca is an invitee. I want that to work well before we go live on my real calendar."
- Scope B: REQUEST (new + update) + CANCEL. Recurring series = one invite; RECURRENCE-ID = instance update.

---

## Shared types (defined in Task 1 and Task 3, referenced throughout)

```rust
// crates/pimsteward-ical/src/imip.rs
pub struct IcalAddress {
    pub email: String,
    pub cn: Option<String>,
}

// crates/pimsteward/src/scheduling/model.rs
pub enum ChangeKind { Added, Modified, Deleted }

pub struct EventChange {
    pub kind: ChangeKind,
    pub rel_path: String,          // "<cal_dir>/events/<key>.ics"
    pub uid: String,
    pub new_ics: Option<String>,   // None for Deleted
    pub old_ics: Option<String>,   // None for Added
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method { Request, Cancel }

pub struct Outbound {
    pub method: Method,
    pub uid: String,
    pub sequence: u32,
    pub recipients: Vec<String>,   // bare email addresses
    pub event_ics: String,         // source ics to derive the iMIP payload from
    pub summary: String,           // for subject + notify
}
```

## File structure

- `crates/pimsteward-ical/src/imip.rs` — NEW. Pure iMIP payload builder + organizer/attendee parsing. No network, no storage.
- `crates/pimsteward-ical/src/feed.rs` — MODIFY. Make `extract_components` and `scan_field` `pub(crate)` so `imip.rs` can reuse them.
- `crates/pimsteward-ical/src/lib.rs` — MODIFY. `pub mod imip;` + re-exports.
- `crates/pimsteward/src/scheduling/mod.rs` — NEW. Orchestrator + module root.
- `crates/pimsteward/src/scheduling/model.rs` — NEW. `EventChange`, `Method`, `Outbound`.
- `crates/pimsteward/src/scheduling/change_feed.rs` — NEW. Git-diff a commit into `Vec<EventChange>`.
- `crates/pimsteward/src/scheduling/watermark.rs` — NEW. Read/write activation watermark; gate.
- `crates/pimsteward/src/scheduling/ledger.rs` — NEW. `scheduling/sent.jsonl` dedup + append.
- `crates/pimsteward/src/scheduling/plan.rs` — NEW. Turn an `EventChange` into zero-or-more `Outbound` (organizer filter, recipient sets, significance differ).
- `crates/pimsteward/src/forwardemail/writes.rs` — MODIFY. Add `Client::send_imip(...)` raw-MIME sender.
- `crates/pimsteward/src/config.rs` — MODIFY. Add `[scheduling]` section.
- `crates/pimsteward/src/daemon.rs` — MODIFY. Run the scheduler after each calendar pull.
- `crates/pimsteward/src/lib.rs` — MODIFY. `pub mod scheduling;`.
- `crates/pimsteward/tests/e2e_scheduling.rs` — NEW. `#[ignore]`d real-ForwardEmail acceptance gate.

---

### Task 1: iMIP payload builder + address parsing (`pimsteward-ical`)

**Goal:** A pure function that turns a stored `.ics` into a valid `METHOD:REQUEST`/`METHOD:CANCEL` VCALENDAR payload with `mailto:` cal-addresses and `RSVP=TRUE`, plus helpers to read the organizer and attendee addresses.

**Files:**
- Create: `crates/pimsteward-ical/src/imip.rs`
- Modify: `crates/pimsteward-ical/src/feed.rs` (change `fn extract_components` → `pub(crate) fn extract_components`, `fn scan_field` → `pub(crate) fn scan_field`)
- Modify: `crates/pimsteward-ical/src/lib.rs` (add `pub mod imip;` and `pub use imip::{IcalAddress, build_imip, organizer, attendees};`)

**Acceptance Criteria:**
- [ ] `organizer(ics)` returns the organizer `IcalAddress` (email from `EMAIL=` param, else from a `mailto:` value), or `None`.
- [ ] `attendees(ics)` returns one `IcalAddress` per `ATTENDEE` line, email resolved the same way.
- [ ] `build_imip` emits a CRLF-folded VCALENDAR: `VERSION:2.0`, `PRODID`, `METHOD:<REQUEST|CANCEL>`, any source `VTIMEZONE` blocks, and the source VEVENT with `ORGANIZER` rewritten to `mailto:<organizer>`, each `ATTENDEE` rewritten to `mailto:<email>;RSVP=TRUE`, `SEQUENCE:<n>`, and a `DTSTAMP`.
- [ ] Golden tests pass for both methods.

**Verify:** `cargo test -p pimsteward-ical imip` → all pass.

**Steps:**

- [ ] **Step 1: Make helpers reusable in `feed.rs`**

Change the two private fns (currently at `crates/pimsteward-ical/src/feed.rs:27` and `:174`) to crate-visible:

```rust
pub(crate) fn extract_components(ical_text: &str, name: &str) -> Vec<String> {
```
```rust
pub(crate) fn scan_field(block: &str, name: &str) -> Option<String> {
```

- [ ] **Step 2: Write failing tests** in `crates/pimsteward-ical/src/imip.rs`

```rust
//! Pure iMIP (iTIP-over-email) payload construction. No network or storage —
//! given a stored `.ics`, produce a METHOD:REQUEST / METHOD:CANCEL VCALENDAR
//! suitable for a `text/calendar` MIME part.

use crate::feed::{extract_components, scan_field};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcalAddress {
    pub email: String,
    pub cn: Option<String>,
}

const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//forwardemail.net//caldav//EN\r\nBEGIN:VEVENT\r\nUID:evt-1\r\nSEQUENCE:0\r\nDTSTART;TZID=America/Toronto:20260701T190000\r\nDTEND;TZID=America/Toronto:20260701T200000\r\nSUMMARY:Dinner\r\nLOCATION:Craft\r\nORGANIZER;CN=Dan Cardamore;EMAIL=dan@hld.ca:/aMTc5opaque\r\nATTENDEE;CN=Heather;CUTYPE=INDIVIDUAL;EMAIL=heather@hld.ca;PARTSTAT=NEEDS-ACTION:/aZZZ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_organizer_email() {
        assert_eq!(organizer(SAMPLE).unwrap().email, "dan@hld.ca");
    }

    #[test]
    fn reads_attendee_emails() {
        let a = attendees(SAMPLE);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].email, "heather@hld.ca");
    }

    #[test]
    fn request_payload_normalizes_addresses_and_sets_sequence() {
        let out = build_imip(SAMPLE, "REQUEST", 2, "dan@hld.ca", &["heather@hld.ca".into()]);
        assert!(out.contains("METHOD:REQUEST\r\n"));
        assert!(out.contains("ORGANIZER:mailto:dan@hld.ca\r\n"));
        assert!(out.contains("ATTENDEE;RSVP=TRUE:mailto:heather@hld.ca\r\n"));
        assert!(out.contains("SEQUENCE:2\r\n"));
        assert!(out.contains("DTSTAMP:"));
        assert!(out.contains("DTSTART;TZID=America/Toronto:20260701T190000\r\n"));
        assert!(out.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(out.trim_end().ends_with("END:VCALENDAR"));
    }

    #[test]
    fn cancel_payload_uses_cancel_method() {
        let out = build_imip(SAMPLE, "CANCEL", 3, "dan@hld.ca", &["heather@hld.ca".into()]);
        assert!(out.contains("METHOD:CANCEL\r\n"));
        assert!(out.contains("SEQUENCE:3\r\n"));
    }
}
```

- [ ] **Step 3: Run tests, confirm they fail to compile** (functions undefined)

Run: `cargo test -p pimsteward-ical imip`
Expected: FAIL — `cannot find function organizer/attendees/build_imip`.

- [ ] **Step 4: Implement** in the same file (above the `#[cfg(test)]` block)

```rust
/// Parse the `EMAIL=` param of a raw property line, falling back to a
/// `mailto:` value after the colon. Returns lowercased address.
fn address_of(line: &str) -> Option<String> {
    // EMAIL= param (case-insensitive) wins; it's what forwardemail emits.
    for part in line.split([';', ':']) {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("EMAIL=").or_else(|| p.strip_prefix("email=")) {
            return Some(rest.trim().to_ascii_lowercase());
        }
    }
    // Fallback: a mailto: cal-address after the colon.
    let val = line.split_once(':').map(|(_, v)| v).unwrap_or("");
    val.to_ascii_lowercase()
        .strip_prefix("mailto:")
        .map(|s| s.trim().to_string())
}

fn cn_of(line: &str) -> Option<String> {
    for part in line.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("CN=").or_else(|| p.strip_prefix("cn=")) {
            let v = rest.split(':').next().unwrap_or(rest).trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn raw_lines(ics: &str, name: &str) -> Vec<String> {
    crate::ical::vevent_raw_lines_named(ics, name)
}

pub fn organizer(ics: &str) -> Option<IcalAddress> {
    let line = raw_lines(ics, "ORGANIZER").into_iter().next()?;
    let email = address_of(&line)?;
    if email.is_empty() {
        return None;
    }
    Some(IcalAddress { email, cn: cn_of(&line) })
}

pub fn attendees(ics: &str) -> Vec<IcalAddress> {
    raw_lines(ics, "ATTENDEE")
        .iter()
        .filter_map(|l| {
            let email = address_of(l)?;
            if email.is_empty() {
                None
            } else {
                Some(IcalAddress { email, cn: cn_of(l) })
            }
        })
        .collect()
}

/// Build a METHOD:REQUEST or METHOD:CANCEL VCALENDAR payload from a stored
/// `.ics`. `method` is "REQUEST" or "CANCEL". Organizer/attendees are
/// rewritten to `mailto:` form; SEQUENCE is forced to `sequence`; a fresh
/// DTSTAMP is added. VTIMEZONE blocks from the source are preserved.
pub fn build_imip(
    ics: &str,
    method: &str,
    sequence: u32,
    organizer_email: &str,
    attendee_emails: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//pimsteward//scheduling//EN\r\n");
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str(&format!("METHOD:{method}\r\n"));

    for tz in extract_components(ics, "VTIMEZONE") {
        out.push_str(&tz);
    }

    // Rebuild the VEVENT line-by-line, replacing scheduling-relevant props.
    let vevent = extract_components(ics, "VEVENT")
        .into_iter()
        .next()
        .unwrap_or_default();
    out.push_str("BEGIN:VEVENT\r\n");
    let mut wrote_dtstamp = false;
    let mut wrote_seq = false;
    for line in crate::ical::unfold(&vevent).lines() {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("BEGIN:VEVENT") || upper.starts_with("END:VEVENT") {
            continue;
        }
        if upper.starts_with("ORGANIZER") {
            out.push_str(&format!("ORGANIZER:mailto:{organizer_email}\r\n"));
            continue;
        }
        if upper.starts_with("ATTENDEE") {
            continue; // re-emitted below from attendee_emails
        }
        if upper.starts_with("SEQUENCE") {
            out.push_str(&format!("SEQUENCE:{sequence}\r\n"));
            wrote_seq = true;
            continue;
        }
        if upper.starts_with("DTSTAMP") {
            out.push_str(&format!("DTSTAMP:{}\r\n", dtstamp_now()));
            wrote_dtstamp = true;
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !wrote_seq {
        out.push_str(&format!("SEQUENCE:{sequence}\r\n"));
    }
    if !wrote_dtstamp {
        out.push_str(&format!("DTSTAMP:{}\r\n", dtstamp_now()));
    }
    for email in attendee_emails {
        out.push_str(&format!("ATTENDEE;RSVP=TRUE:mailto:{email}\r\n"));
    }
    out.push_str("END:VEVENT\r\n");
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn dtstamp_now() -> String {
    // UTC basic format YYYYMMDDTHHMMSSZ
    use chrono::Utc;
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}
```

Note: `chrono` is already a dependency of `pimsteward-ical` (used in `feed.rs`). `vevent_raw_lines_named` and `unfold` are existing `pub` fns in `ical.rs`.

- [ ] **Step 5: Run tests, confirm pass**

Run: `cargo test -p pimsteward-ical imip`
Expected: PASS (4 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/pimsteward-ical/src/imip.rs crates/pimsteward-ical/src/feed.rs crates/pimsteward-ical/src/lib.rs
git commit -m "feat(ical): iMIP REQUEST/CANCEL payload builder + address parsing"
```

---

### Task 2: Scheduling config section

**Goal:** A `[scheduling]` config block (`enabled`, `notify_on_send`) wired into `Config`, defaulting to disabled with notify on.

**Files:**
- Modify: `crates/pimsteward/src/config.rs`

**Acceptance Criteria:**
- [ ] `Config` has a `scheduling: SchedulingConfig` field with `#[serde(default)]`.
- [ ] `SchedulingConfig { enabled: bool (default false), notify_on_send: bool (default true) }`.
- [ ] A TOML with `[scheduling]\nenabled = true` parses with `notify_on_send == true`.

**Verify:** `cargo test -p pimsteward config::tests` → all pass (including new test).

**Steps:**

- [ ] **Step 1: Add the field to `Config`** (in the struct at `crates/pimsteward/src/config.rs:16`)

```rust
    #[serde(default)]
    pub scheduling: SchedulingConfig,
```

- [ ] **Step 2: Define `SchedulingConfig`** (near the other config structs)

```rust
/// Organizer-side calendar scheduling (iMIP). When `enabled`, the daemon
/// sends REQUEST/CANCEL messages for events the alias organizes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchedulingConfig {
    /// Master switch. Off by default — only the dan provider opts in.
    #[serde(default)]
    pub enabled: bool,
    /// Debug tripwire: email the alias a summary on every send.
    #[serde(default = "default_true")]
    pub notify_on_send: bool,
}

impl Default for SchedulingConfig {
    fn default() -> Self {
        Self { enabled: false, notify_on_send: true }
    }
}

fn default_true() -> bool {
    true
}
```

(If a `default_true` already exists in the file, reuse it instead of redefining.)

- [ ] **Step 3: Write a test** (in the existing `#[cfg(test)] mod tests` of `config.rs`)

```rust
#[test]
fn scheduling_section_parses() {
    let toml = r#"
repo_path = "/tmp/repo"

[scheduling]
enabled = true
"#;
    // mirror the existing config-parse test helper used in this module
    let cfg: Config = toml::from_str(toml).expect("parse");
    assert!(cfg.scheduling.enabled);
    assert!(cfg.scheduling.notify_on_send); // default
}
```

If the module's other tests load via `Config::load`/`Figment` rather than `toml::from_str`, match that pattern instead (look at the test at `config.rs:526`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p pimsteward config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/config.rs
git commit -m "feat(config): add [scheduling] section (enabled, notify_on_send)"
```

---

### Task 3: Scheduling model types + change feed (git diff)

**Goal:** Given the `Repo` and a pull's `commit_sha`, produce the list of added/modified/deleted event `.ics` changes by diffing the commit against its parent.

**Files:**
- Create: `crates/pimsteward/src/scheduling/mod.rs` (module root — `pub mod model; pub mod change_feed;` for now)
- Create: `crates/pimsteward/src/scheduling/model.rs`
- Create: `crates/pimsteward/src/scheduling/change_feed.rs`
- Modify: `crates/pimsteward/src/lib.rs` (add `pub mod scheduling;`)

**Acceptance Criteria:**
- [ ] `model.rs` defines `ChangeKind`, `EventChange`, `Method`, `Outbound` as in "Shared types".
- [ ] `change_feed(repo, commit_sha) -> Result<Vec<EventChange>, Error>` returns one `EventChange` per `*.ics` path in `git diff --name-status <parent> <commit>` (Added/Modified/Deleted), with `new_ics` read from the commit blob and `old_ics` from the parent blob.
- [ ] `.meta.json` and `_calendar.json` paths are ignored.
- [ ] A commit with no parent (root) yields all `.ics` as `Added`.

**Verify:** `cargo test -p pimsteward scheduling::change_feed` → all pass.

**Steps:**

- [ ] **Step 1: Create `model.rs`** with the exact "Shared types" definitions (ChangeKind, EventChange, Method, Outbound). Derive `Debug, Clone` where useful; `Method` also `Copy, PartialEq, Eq`.

- [ ] **Step 2: Write failing test** `crates/pimsteward/src/scheduling/change_feed.rs`

```rust
//! Turn a pull commit into a list of event-file changes by diffing it
//! against its parent with the `git` CLI (same backend as `Repo`).

use crate::scheduling::model::{ChangeKind, EventChange};
use crate::store::Repo;
use crate::error::Error;
use std::process::Command;

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git").current_dir(repo).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap().success();
        assert!(ok, "git {:?}", args);
    }

    #[test]
    fn added_and_modified_and_deleted_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let p = dir.path();

        // commit 1: add two events
        std::fs::create_dir_all(p.join("cal/events")).unwrap();
        std::fs::write(p.join("cal/events/a.ics"), "BEGIN:VEVENT\nUID:a\nEND:VEVENT\n").unwrap();
        std::fs::write(p.join("cal/events/b.ics"), "BEGIN:VEVENT\nUID:b\nEND:VEVENT\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "c1"]);

        // commit 2: modify a, delete b, add c
        std::fs::write(p.join("cal/events/a.ics"), "BEGIN:VEVENT\nUID:a\nSUMMARY:x\nEND:VEVENT\n").unwrap();
        std::fs::remove_file(p.join("cal/events/b.ics")).unwrap();
        std::fs::write(p.join("cal/events/c.ics"), "BEGIN:VEVENT\nUID:c\nEND:VEVENT\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "c2"]);

        let sha = String::from_utf8(
            Command::new("git").current_dir(p).args(["rev-parse", "HEAD"]).output().unwrap().stdout
        ).unwrap().trim().to_string();

        let mut changes = change_feed(&repo, &sha).unwrap();
        changes.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0].kind, ChangeKind::Modified)); // a.ics
        assert_eq!(changes[0].uid, "a");
        assert!(changes[0].old_ics.as_deref().unwrap().contains("UID:a"));
        assert!(matches!(changes[1].kind, ChangeKind::Deleted));  // b.ics
        assert!(matches!(changes[2].kind, ChangeKind::Added));    // c.ics
    }
}
```

Add `tempfile` to `[dev-dependencies]` of `crates/pimsteward/Cargo.toml` if not already present (`tempfile = "3"`).

- [ ] **Step 3: Run test, confirm fail**

Run: `cargo test -p pimsteward scheduling::change_feed`
Expected: FAIL — `change_feed` undefined.

- [ ] **Step 4: Implement** (above the test module)

```rust
fn run_git(repo: &Repo, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git")
        .current_dir(repo.root())
        .args(args)
        .output()
        .map_err(|e| Error::config(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(Error::config(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read a blob at `<rev>:<path>`; returns None if the path didn't exist there.
fn blob(repo: &Repo, rev: &str, path: &str) -> Option<String> {
    Command::new("git")
        .current_dir(repo.root())
        .args(["show", &format!("{rev}:{path}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn uid_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".ics"))
        .unwrap_or(path)
        .to_string()
}

pub fn change_feed(repo: &Repo, commit_sha: &str) -> Result<Vec<EventChange>, Error> {
    // Parent (empty tree if root commit).
    let parent = run_git(repo, &["rev-parse", &format!("{commit_sha}^")])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()); // empty tree

    let diff = run_git(
        repo,
        &["diff", "--name-status", "--no-renames", &parent, commit_sha],
    )?;

    let mut changes = Vec::new();
    for line in diff.lines() {
        let mut cols = line.split('\t');
        let status = cols.next().unwrap_or("");
        let path = cols.next().unwrap_or("").to_string();
        if !path.ends_with(".ics") {
            continue;
        }
        let kind = match status.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('M') => ChangeKind::Modified,
            Some('D') => ChangeKind::Deleted,
            _ => continue,
        };
        let new_ics = match kind {
            ChangeKind::Deleted => None,
            _ => blob(repo, commit_sha, &path),
        };
        let old_ics = match kind {
            ChangeKind::Added => None,
            _ => blob(repo, &parent, &path),
        };
        changes.push(EventChange {
            uid: uid_from_path(&path),
            rel_path: path,
            kind,
            new_ics,
            old_ics,
        });
    }
    Ok(changes)
}
```

- [ ] **Step 5: Run test, confirm pass**

Run: `cargo test -p pimsteward scheduling::change_feed`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/pimsteward/src/scheduling/ crates/pimsteward/src/lib.rs crates/pimsteward/Cargo.toml
git commit -m "feat(scheduling): model types + git-diff change feed"
```

---

### Task 4: Watermark store + gate

**Goal:** Persist an activation watermark (a commit SHA) in the repo and provide a gate that drops events at/below it and events whose start is in the past.

**Files:**
- Create: `crates/pimsteward/src/scheduling/watermark.rs`
- Modify: `crates/pimsteward/src/scheduling/mod.rs` (`pub mod watermark;`)

**Acceptance Criteria:**
- [ ] `read_watermark(repo) -> Option<String>` reads `scheduling/watermark` (trimmed), `None` if absent.
- [ ] `ensure_watermark(repo, head_sha)` writes the file (committing it) only if it doesn't already exist; never overwrites. This means: on first ever run, the current HEAD becomes the floor and all existing events are below it.
- [ ] `is_past_start(ics, now) -> bool` returns true when the event's last `DTSTART` is strictly before `now` (non-recurring); recurring events (`RRULE` present) are never considered past.
- [ ] `commit_is_after(repo, watermark, commit_sha) -> bool` true iff `commit_sha` is a descendant of `watermark` (i.e. strictly newer).

**Verify:** `cargo test -p pimsteward scheduling::watermark` → all pass.

**Steps:**

- [ ] **Step 1: Write failing tests** in `watermark.rs`

```rust
//! Activation watermark: the commit SHA at the moment scheduling was first
//! enabled. Only events introduced in commits strictly after it are eligible,
//! so pre-existing events never trigger invites.

use crate::store::Repo;
use crate::error::Error;
use chrono::{DateTime, Utc};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        repo.empty_commit("t", "t@t", "root").unwrap();
        let head = repo.empty_commit("t", "t@t", "head1").unwrap();
        ensure_watermark(&repo, &head).unwrap();
        assert_eq!(read_watermark(&repo).as_deref(), Some(head.as_str()));
        // second call must not overwrite
        let head2 = repo.empty_commit("t", "t@t", "head2").unwrap();
        ensure_watermark(&repo, &head2).unwrap();
        assert_eq!(read_watermark(&repo).as_deref(), Some(head.as_str()));
    }

    #[test]
    fn past_nonrecurring_is_filtered_recurring_is_not() {
        let now: DateTime<Utc> = "2026-06-11T00:00:00Z".parse().unwrap();
        let past = "BEGIN:VEVENT\nUID:p\nDTSTART:20200101T120000Z\nEND:VEVENT\n";
        let future = "BEGIN:VEVENT\nUID:f\nDTSTART:20990101T120000Z\nEND:VEVENT\n";
        let recurring = "BEGIN:VEVENT\nUID:r\nDTSTART:20200101T120000Z\nRRULE:FREQ=WEEKLY\nEND:VEVENT\n";
        assert!(is_past_start(past, now));
        assert!(!is_past_start(future, now));
        assert!(!is_past_start(recurring, now));
    }
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p pimsteward scheduling::watermark`
Expected: FAIL — undefined functions.

- [ ] **Step 3: Implement**

```rust
use std::process::Command;

const WATERMARK_PATH: &str = "scheduling/watermark";

pub fn read_watermark(repo: &Repo) -> Option<String> {
    repo.read_file(WATERMARK_PATH)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn ensure_watermark(repo: &Repo, head_sha: &str) -> Result<(), Error> {
    if read_watermark(repo).is_some() {
        return Ok(());
    }
    repo.write_file(WATERMARK_PATH, head_sha.trim().as_bytes())?;
    repo.commit_all(
        "pimsteward-scheduling",
        "scheduling@pimsteward.local",
        "scheduling: set activation watermark",
    )?;
    Ok(())
}

/// True iff `commit_sha` is strictly newer than `watermark` (a descendant).
pub fn commit_is_after(repo: &Repo, watermark: &str, commit_sha: &str) -> bool {
    if watermark == commit_sha {
        return false;
    }
    // `git merge-base --is-ancestor A B` exits 0 when A is an ancestor of B.
    Command::new("git")
        .current_dir(repo.root())
        .args(["merge-base", "--is-ancestor", watermark, commit_sha])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when the event's start is strictly before `now` and it does not
/// recur. Recurring events (RRULE present) are always eligible.
pub fn is_past_start(ics: &str, now: DateTime<Utc>) -> bool {
    use pimsteward_ical::ical::{vevent_field, vevent_field_all};
    if vevent_field(ics, "RRULE").is_some() {
        return false;
    }
    // Use the last DTSTART (handles the merged-history shape seen in real data).
    let Some(dt) = vevent_field_all(ics, "DTSTART").into_iter().last() else {
        return false; // no start → don't filter
    };
    match parse_ical_dt(&dt) {
        Some(start) => start < now,
        None => false,
    }
}

/// Parse a DTSTART value (basic UTC `...Z`, or local/floating basic form).
/// Floating/local values are treated as UTC for the past/future decision —
/// good enough for a coarse "is this in the past" guard.
fn parse_ical_dt(v: &str) -> Option<DateTime<Utc>> {
    use chrono::NaiveDateTime;
    let v = v.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S") {
        return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y%m%d") {
        return Some(DateTime::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0)?, Utc));
    }
    None
}
```

Note: `vevent_field` / `vevent_field_all` are existing `pub` fns in `pimsteward_ical::ical`. The crate is named `pimsteward-ical` → import path `pimsteward_ical`.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p pimsteward scheduling::watermark`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/scheduling/
git commit -m "feat(scheduling): activation watermark + past-start gate"
```

---

### Task 5: Sent ledger (dedup + audit)

**Goal:** An append-only `scheduling/sent.jsonl` recording each `(uid, sequence, recipient, method)` send so re-pulls and restarts never re-send, and CANCEL can find the last sequence.

**Files:**
- Create: `crates/pimsteward/src/scheduling/ledger.rs`
- Modify: `crates/pimsteward/src/scheduling/mod.rs` (`pub mod ledger;`)

**Acceptance Criteria:**
- [ ] `Ledger::load(repo)` reads all records from `scheduling/sent.jsonl` (empty if absent).
- [ ] `already_sent(uid, sequence, recipient, method)` returns true iff a matching record exists.
- [ ] `record(...)` appends a JSON line (`{uid, sequence, recipient, method, message_id, sent_at}`) to the file (in-memory + flushed to disk; the caller commits via `Repo`).
- [ ] `last_sequence(uid)` returns the max sequence recorded for a uid, or `None`.

**Verify:** `cargo test -p pimsteward scheduling::ledger` → all pass.

**Steps:**

- [ ] **Step 1: Write failing tests** in `ledger.rs`

```rust
//! Append-only sent-ledger for scheduling messages. One JSON line per
//! (uid, sequence, recipient, method) send. Restart-safe dedup + audit trail.

use crate::store::Repo;
use crate::error::Error;
use crate::scheduling::model::Method;
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_dedup_and_last_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let mut led = Ledger::load(&repo).unwrap();
        assert!(!led.already_sent("u1", 0, "a@x", Method::Request));
        led.record(&repo, "u1", 0, "a@x", Method::Request, "mid-1").unwrap();
        assert!(led.already_sent("u1", 0, "a@x", Method::Request));
        assert!(!led.already_sent("u1", 1, "a@x", Method::Request));
        led.record(&repo, "u1", 2, "a@x", Method::Cancel, "mid-2").unwrap();
        assert_eq!(led.last_sequence("u1"), Some(2));

        // reload from disk preserves records
        let led2 = Ledger::load(&repo).unwrap();
        assert!(led2.already_sent("u1", 0, "a@x", Method::Request));
        assert_eq!(led2.last_sequence("u1"), Some(2));
    }
}
```

- [ ] **Step 2: Run, confirm fail.** `cargo test -p pimsteward scheduling::ledger` → FAIL.

- [ ] **Step 3: Implement**

```rust
const LEDGER_PATH: &str = "scheduling/sent.jsonl";

fn method_str(m: Method) -> &'static str {
    match m {
        Method::Request => "REQUEST",
        Method::Cancel => "CANCEL",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentRecord {
    pub uid: String,
    pub sequence: u32,
    pub recipient: String,
    pub method: String,
    pub message_id: String,
    pub sent_at: String,
}

pub struct Ledger {
    records: Vec<SentRecord>,
}

impl Ledger {
    pub fn load(repo: &Repo) -> Result<Self, Error> {
        let records = match repo.read_file(LEDGER_PATH) {
            Ok(bytes) => String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<SentRecord>(l).ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        Ok(Self { records })
    }

    pub fn already_sent(&self, uid: &str, sequence: u32, recipient: &str, method: Method) -> bool {
        let m = method_str(method);
        self.records.iter().any(|r| {
            r.uid == uid && r.sequence == sequence && r.recipient == recipient && r.method == m
        })
    }

    pub fn last_sequence(&self, uid: &str) -> Option<u32> {
        self.records.iter().filter(|r| r.uid == uid).map(|r| r.sequence).max()
    }

    /// Append a record to memory and flush the whole ledger to disk. The
    /// caller is responsible for committing via `Repo::commit_all`.
    pub fn record(
        &mut self,
        repo: &Repo,
        uid: &str,
        sequence: u32,
        recipient: &str,
        method: Method,
        message_id: &str,
    ) -> Result<(), Error> {
        self.records.push(SentRecord {
            uid: uid.to_string(),
            sequence,
            recipient: recipient.to_string(),
            method: method_str(method).to_string(),
            message_id: message_id.to_string(),
            sent_at: chrono::Utc::now().to_rfc3339(),
        });
        let mut body = String::new();
        for r in &self.records {
            body.push_str(&serde_json::to_string(r).map_err(|e| Error::config(e.to_string()))?);
            body.push('\n');
        }
        repo.write_file(LEDGER_PATH, body.as_bytes())?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run, confirm pass.** `cargo test -p pimsteward scheduling::ledger` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/scheduling/
git commit -m "feat(scheduling): sent-ledger for dedup + audit"
```

---

### Task 6: Planner — change → outbound iMIP messages

**Goal:** Turn a single `EventChange` into zero or more `Outbound` messages, applying the organizer filter, recipient sets, and scheduling-significance differ.

**Files:**
- Create: `crates/pimsteward/src/scheduling/plan.rs`
- Modify: `crates/pimsteward/src/scheduling/mod.rs` (`pub mod plan;`)

**Acceptance Criteria:**
- [ ] `plan_change(change, organizer_self, now) -> Vec<Outbound>` where `organizer_self` is `dan@hld.ca`.
- [ ] Non-organizer events (organizer email != `organizer_self`) yield `[]`.
- [ ] `Added` with ≥1 non-self attendee → one `Request` to all non-self attendees.
- [ ] `Modified` with a scheduling-significant change → one `Request` to all current non-self attendees; bump sequence (handled in Task 8 via ledger).
- [ ] `Modified` that only removes attendees → one `Cancel` to each removed attendee, no Request.
- [ ] `Modified` with no significant change and no attendee delta → `[]`.
- [ ] `Deleted` → one `Cancel` to all (previous) non-self attendees.
- [ ] Self-attendee (`organizer_self`) is never a recipient.

**Verify:** `cargo test -p pimsteward scheduling::plan` → all pass.

**Steps:**

- [ ] **Step 1: Write failing tests** in `plan.rs`

```rust
//! Policy layer: decide what iMIP messages a single calendar change implies.

use crate::scheduling::model::{ChangeKind, EventChange, Method, Outbound};
use chrono::{DateTime, Utc};
use pimsteward_ical::imip;

fn ev(uid: &str, summary: &str, attendees: &[&str], extra: &str) -> String {
    let mut s = format!(
        "BEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:0\r\nDTSTART:20990101T120000Z\r\nSUMMARY:{summary}\r\nORGANIZER;EMAIL=dan@hld.ca:mailto:dan@hld.ca\r\n{extra}"
    );
    for a in attendees {
        s.push_str(&format!("ATTENDEE;EMAIL={a}:mailto:{a}\r\n"));
    }
    s.push_str("END:VEVENT\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> { "2026-06-11T00:00:00Z".parse().unwrap() }

    #[test]
    fn added_with_attendees_requests_all_non_self() {
        let c = EventChange {
            kind: ChangeKind::Added, rel_path: "c/events/x.ics".into(), uid: "x".into(),
            new_ics: Some(ev("x", "Lunch", &["heather@hld.ca", "dan@hld.ca"], "")), old_ics: None,
        };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Request);
        assert_eq!(out[0].recipients, vec!["heather@hld.ca".to_string()]);
    }

    #[test]
    fn non_organizer_event_is_ignored() {
        let mut ics = ev("y", "Theirs", &["heather@hld.ca"], "");
        ics = ics.replace("EMAIL=dan@hld.ca:mailto:dan@hld.ca", "EMAIL=sean@x.com:mailto:sean@x.com");
        let c = EventChange { kind: ChangeKind::Added, rel_path: "c/events/y.ics".into(), uid: "y".into(),
            new_ics: Some(ics), old_ics: None };
        assert!(plan_change(&c, "dan@hld.ca", now()).is_empty());
    }

    #[test]
    fn time_change_is_significant() {
        let old = ev("z", "Mtg", &["heather@hld.ca"], "DTSTART:20990101T120000Z\r\n");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "DTSTART:20990101T130000Z\r\n");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Request);
    }

    #[test]
    fn alarm_only_change_is_not_significant() {
        let old = ev("z", "Mtg", &["heather@hld.ca"], "");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "BEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\n");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        assert!(plan_change(&c, "dan@hld.ca", now()).is_empty());
    }

    #[test]
    fn removed_attendee_gets_cancel_only() {
        let old = ev("z", "Mtg", &["heather@hld.ca", "kid@hld.ca"], "");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Cancel);
        assert_eq!(out[0].recipients, vec!["kid@hld.ca".to_string()]);
    }

    #[test]
    fn deleted_cancels_all() {
        let old = ev("z", "Mtg", &["heather@hld.ca", "kid@hld.ca"], "");
        let c = EventChange { kind: ChangeKind::Deleted, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: None, old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Cancel);
        let mut r = out[0].recipients.clone(); r.sort();
        assert_eq!(r, vec!["heather@hld.ca".to_string(), "kid@hld.ca".to_string()]);
    }
}
```

- [ ] **Step 2: Run, confirm fail.** `cargo test -p pimsteward scheduling::plan` → FAIL.

- [ ] **Step 3: Implement**

```rust
const SIGNIFICANT: &[&str] = &[
    "DTSTART", "DTEND", "DURATION", "RRULE", "RDATE", "EXDATE",
    "RECURRENCE-ID", "SUMMARY", "LOCATION", "DESCRIPTION",
];

fn non_self_attendees(ics: &str, organizer_self: &str) -> Vec<String> {
    imip::attendees(ics)
        .into_iter()
        .map(|a| a.email)
        .filter(|e| e != organizer_self)
        .collect()
}

fn is_significant_change(old: &str, new: &str) -> bool {
    use pimsteward_ical::ical::vevent_field_all;
    SIGNIFICANT.iter().any(|name| {
        vevent_field_all(old, name) != vevent_field_all(new, name)
    })
}

fn organized_by_self(ics: &str, organizer_self: &str) -> bool {
    imip::organizer(ics).map(|o| o.email == organizer_self).unwrap_or(false)
}

pub fn plan_change(
    change: &EventChange,
    organizer_self: &str,
    now: DateTime<Utc>,
) -> Vec<Outbound> {
    use crate::scheduling::watermark::is_past_start;

    match change.kind {
        ChangeKind::Added => {
            let Some(ics) = change.new_ics.as_deref() else { return vec![] };
            if !organized_by_self(ics, organizer_self) || is_past_start(ics, now) {
                return vec![];
            }
            let to = non_self_attendees(ics, organizer_self);
            if to.is_empty() {
                return vec![];
            }
            vec![Outbound {
                method: Method::Request,
                uid: change.uid.clone(),
                sequence: 0, // finalized in orchestrator from ledger/event
                recipients: to,
                event_ics: ics.to_string(),
                summary: summary_of(ics),
            }]
        }
        ChangeKind::Modified => {
            let (Some(new), Some(old)) = (change.new_ics.as_deref(), change.old_ics.as_deref())
            else { return vec![] };
            if !organized_by_self(new, organizer_self) {
                return vec![];
            }
            let new_to = non_self_attendees(new, organizer_self);
            let old_to = non_self_attendees(old, organizer_self);
            let removed: Vec<String> =
                old_to.iter().filter(|e| !new_to.contains(e)).cloned().collect();

            let mut out = Vec::new();
            // Significant detail change OR new attendee → REQUEST to current set.
            let added_attendee = new_to.iter().any(|e| !old_to.contains(e));
            if !is_past_start(new, now)
                && !new_to.is_empty()
                && (is_significant_change(old, new) || added_attendee)
            {
                out.push(Outbound {
                    method: Method::Request,
                    uid: change.uid.clone(),
                    sequence: 0,
                    recipients: new_to.clone(),
                    event_ics: new.to_string(),
                    summary: summary_of(new),
                });
            }
            // Removed attendees → CANCEL to just them.
            if !removed.is_empty() {
                out.push(Outbound {
                    method: Method::Cancel,
                    uid: change.uid.clone(),
                    sequence: 0,
                    recipients: removed,
                    event_ics: new.to_string(),
                    summary: summary_of(new),
                });
            }
            out
        }
        ChangeKind::Deleted => {
            let Some(ics) = change.old_ics.as_deref() else { return vec![] };
            if !organized_by_self(ics, organizer_self) {
                return vec![];
            }
            let to = non_self_attendees(ics, organizer_self);
            if to.is_empty() {
                return vec![];
            }
            vec![Outbound {
                method: Method::Cancel,
                uid: change.uid.clone(),
                sequence: 0,
                recipients: to,
                event_ics: ics.to_string(),
                summary: summary_of(ics),
            }]
        }
    }
}

fn summary_of(ics: &str) -> String {
    pimsteward_ical::ical::vevent_field(ics, "SUMMARY").unwrap_or_else(|| "(no title)".into())
}
```

- [ ] **Step 4: Run, confirm pass.** `cargo test -p pimsteward scheduling::plan` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/scheduling/
git commit -m "feat(scheduling): planner (organizer filter, significance, recipients)"
```

---

### Task 7: iMIP MIME sender on the forwardemail Client

**Goal:** A `Client::send_imip(...)` that builds a `multipart/alternative` RFC822 message (text/plain + text/calendar with the method) and posts it via the existing `{from, raw}` path, returning the new message id.

**Files:**
- Modify: `crates/pimsteward/src/forwardemail/writes.rs`

**Acceptance Criteria:**
- [ ] A pure builder `build_imip_mime(from, to, subject, text_body, calendar_payload, method) -> String` produces an RFC822 message with `Content-Type: multipart/alternative; boundary=...`, a `text/plain` part, and a `text/calendar; method=<METHOD>; charset=utf-8` part containing `calendar_payload`.
- [ ] Unit test asserts the structure (both parts present, method in the calendar content-type, boundary closes).
- [ ] `send_imip(&self, to, subject, text_body, calendar_payload, method) -> Result<serde_json::Value, Error>` posts `{from: alias, raw: <built message>}` to `/v1/emails` (same as `send_raw_threaded`).

**Verify:** `cargo test -p pimsteward forwardemail::writes` → all pass (build test; the network `send_imip` is covered by Task 10's e2e).

**Steps:**

- [ ] **Step 1: Write failing test** (append to the existing `#[cfg(test)] mod tests` in `writes.rs`, or add one)

```rust
#[test]
fn imip_mime_has_both_parts_and_method() {
    let cal = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n";
    let raw = build_imip_mime(
        "dan@hld.ca", "heather@hld.ca", "Invitation: Lunch",
        "You're invited to Lunch", cal, "REQUEST",
    );
    assert!(raw.contains("From: dan@hld.ca\r\n"));
    assert!(raw.contains("To: heather@hld.ca\r\n"));
    assert!(raw.contains("Subject: Invitation: Lunch\r\n"));
    assert!(raw.contains("Content-Type: multipart/alternative;"));
    assert!(raw.contains("Content-Type: text/plain; charset=utf-8"));
    assert!(raw.contains("Content-Type: text/calendar; method=REQUEST; charset=utf-8"));
    assert!(raw.contains("BEGIN:VCALENDAR"));
    // boundary opens (--b) and closes (--b--)
    let b = raw.split("boundary=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    assert!(raw.contains(&format!("--{b}\r\n")));
    assert!(raw.contains(&format!("--{b}--")));
}
```

- [ ] **Step 2: Run, confirm fail.** `cargo test -p pimsteward forwardemail::writes` → FAIL (`build_imip_mime` undefined).

- [ ] **Step 3: Implement** in `writes.rs`

```rust
/// Build a multipart/alternative iMIP message: a human-readable text/plain
/// part plus a text/calendar part carrying the iTIP method. Deterministic
/// boundary derived from the calendar payload so tests are stable and we add
/// no new RNG dependency.
pub(crate) fn build_imip_mime(
    from: &str,
    to: &str,
    subject: &str,
    text_body: &str,
    calendar_payload: &str,
    method: &str,
) -> String {
    let boundary = format!("pimsteward-{:016x}", fnv1a(calendar_payload.as_bytes()));
    let mut m = String::new();
    m.push_str(&format!("From: {from}\r\n"));
    m.push_str(&format!("To: {to}\r\n"));
    m.push_str(&format!("Subject: {subject}\r\n"));
    m.push_str("MIME-Version: 1.0\r\n");
    m.push_str(&format!(
        "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n"
    ));

    m.push_str(&format!("--{boundary}\r\n"));
    m.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    m.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
    m.push_str(text_body);
    m.push_str("\r\n\r\n");

    m.push_str(&format!("--{boundary}\r\n"));
    m.push_str(&format!(
        "Content-Type: text/calendar; method={method}; charset=utf-8\r\n"
    ));
    m.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
    m.push_str(calendar_payload);
    m.push_str("\r\n");

    m.push_str(&format!("--{boundary}--\r\n"));
    m
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Client {
    /// Send an iMIP scheduling message (REQUEST/CANCEL) to a single recipient,
    /// From: the authenticated alias. Returns the `/v1/emails` JSON response.
    pub async fn send_imip(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        calendar_payload: &str,
        method: &str,
    ) -> Result<serde_json::Value, Error> {
        let raw = build_imip_mime(
            self.alias_user(),
            to,
            subject,
            text_body,
            calendar_payload,
            method,
        );
        let body = serde_json::json!({ "from": self.alias_user(), "raw": raw });
        self.post_json("/v1/emails", &body).await
    }
}
```

(`post_json`, `alias_user`, and `Error` are already in scope in `writes.rs`. If `serde_json::json` isn't already imported, the existing file uses `json!` — reuse that import.)

- [ ] **Step 4: Run, confirm pass.** `cargo test -p pimsteward forwardemail::writes` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/forwardemail/writes.rs
git commit -m "feat(forwardemail): send_imip — multipart/alternative iMIP sender"
```

---

### Task 8: Orchestrator — run scheduling for one pull commit

**Goal:** Wire change-feed → watermark gate → planner → sequence assignment → ledger dedup → `send_imip` → notify, into one `run_scheduling(repo, commit_sha, client, alias, notify_on_send, now)` entry point.

**Files:**
- Modify: `crates/pimsteward/src/scheduling/mod.rs`

**Acceptance Criteria:**
- [ ] `run_scheduling(...)` is a no-op (returns `Ok(0)`) when the commit is not after the watermark, or the watermark is unset (it sets the watermark to the commit's parent-or-self on first run, so the triggering commit is NOT retroactively processed... see Step 3 note).
- [ ] For each `Outbound`, sequence = `max(event SEQUENCE, last ledger sequence + 1 for CANCEL)`; REQUEST uses the event's SEQUENCE (default 0). Each (uid, seq, recipient, method) is sent once (ledger-guarded).
- [ ] After sending, it records the ledger and commits `scheduling/sent.jsonl`.
- [ ] When `notify_on_send`, it sends one summary email to `alias` per outbound message via `send_imip`'s sibling plain path (use `Client::send_email` with a `NewMessage`).
- [ ] Returns the count of iMIP messages sent.
- [ ] A unit test with a stub sender verifies dedup (second identical run sends nothing) and recipient/method correctness.

**Verify:** `cargo test -p pimsteward scheduling::orchestrator` → all pass.

**Steps:**

- [ ] **Step 1: Define a `Sender` trait** (so tests don't hit the network) in `mod.rs`

```rust
pub mod model;
pub mod change_feed;
pub mod watermark;
pub mod ledger;
pub mod plan;

use crate::store::Repo;
use crate::error::Error;
use crate::scheduling::model::{Method, Outbound};
use chrono::{DateTime, Utc};

/// Abstraction over the act of sending one iMIP message + one notify mail,
/// so the orchestrator is unit-testable without network access.
#[async_trait::async_trait]
pub trait Sender: Send + Sync {
    /// Send the iMIP message to `to`; return a message-id string.
    async fn send_imip(
        &self, to: &str, subject: &str, text_body: &str, payload: &str, method: &str,
    ) -> Result<String, Error>;
    /// Send a plaintext notify mail to the alias owner.
    async fn notify(&self, subject: &str, body: &str) -> Result<(), Error>;
}
```

(`async-trait` is already a dependency — the `CalendarWriter` trait at `source/traits.rs:146` uses `#[async_trait]`. Confirm and reuse the same import style.)

- [ ] **Step 2: Write failing test** with a stub sender

```rust
#[cfg(test)]
mod orchestrator_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubSender {
        sent: Mutex<Vec<(String, String)>>, // (to, method)
        notes: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Sender for StubSender {
        async fn send_imip(&self, to: &str, _s: &str, _t: &str, _p: &str, method: &str)
            -> Result<String, Error> {
            self.sent.lock().unwrap().push((to.into(), method.into()));
            Ok(format!("mid-{}", self.sent.lock().unwrap().len()))
        }
        async fn notify(&self, subject: &str, _b: &str) -> Result<(), Error> {
            self.notes.lock().unwrap().push(subject.into());
            Ok(())
        }
    }

    fn git(p: &std::path::Path, args: &[&str]) {
        std::process::Command::new("git").current_dir(p).args(args)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@t")
            .env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@t")
            .status().unwrap();
    }

    #[tokio::test]
    async fn sends_once_then_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let p = dir.path();
        repo.empty_commit("t","t@t","root").unwrap();
        // watermark = current HEAD (root); the NEXT commit is eligible.
        let head = repo.empty_commit("t","t@t","wm").unwrap();
        watermark::ensure_watermark(&repo, &head).unwrap();

        // new commit adds an organizer event with an attendee
        std::fs::create_dir_all(p.join("cal/events")).unwrap();
        let ics = "BEGIN:VEVENT\r\nUID:e1\r\nSEQUENCE:0\r\nDTSTART:20990101T120000Z\r\nSUMMARY:Lunch\r\nORGANIZER;EMAIL=dan@hld.ca:mailto:dan@hld.ca\r\nATTENDEE;EMAIL=heather@hld.ca:mailto:heather@hld.ca\r\nEND:VEVENT\r\n";
        std::fs::write(p.join("cal/events/e1.ics"), ics).unwrap();
        git(p, &["add","-A"]); git(p, &["commit","-m","add e1"]);
        let sha = String::from_utf8(std::process::Command::new("git").current_dir(p)
            .args(["rev-parse","HEAD"]).output().unwrap().stdout).unwrap().trim().to_string();

        let stub = StubSender::default();
        let now: DateTime<Utc> = "2026-06-11T00:00:00Z".parse().unwrap();
        let n = run_scheduling(&repo, &sha, &stub, "dan@hld.ca", true, now).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(stub.sent.lock().unwrap().clone(), vec![("heather@hld.ca".into(),"REQUEST".into())]);
        assert_eq!(stub.notes.lock().unwrap().len(), 1);

        // re-run same commit → ledger dedups, nothing sent
        let n2 = run_scheduling(&repo, &sha, &stub, "dan@hld.ca", true, now).await.unwrap();
        assert_eq!(n2, 0);
        assert_eq!(stub.sent.lock().unwrap().len(), 1);
    }
}
```

- [ ] **Step 3: Implement `run_scheduling`**

```rust
/// Process one pull commit. Returns the number of iMIP messages sent.
/// Pre-watermark commits and pre-existing events are never processed.
pub async fn run_scheduling(
    repo: &Repo,
    commit_sha: &str,
    sender: &dyn Sender,
    organizer_self: &str,
    notify_on_send: bool,
    now: DateTime<Utc>,
) -> Result<usize, Error> {
    let Some(wm) = watermark::read_watermark(repo) else {
        // No watermark yet → this is activation; set it and process nothing.
        watermark::ensure_watermark(repo, commit_sha)?;
        return Ok(0);
    };
    if !watermark::commit_is_after(repo, &wm, commit_sha) {
        return Ok(0);
    }

    let changes = change_feed::change_feed(repo, commit_sha)?;
    let mut ledger = ledger::Ledger::load(repo)?;
    let mut sent_count = 0usize;
    let mut dirty = false;

    for change in &changes {
        for mut outbound in plan::plan_change(change, organizer_self, now) {
            outbound.sequence = resolve_sequence(&outbound, &ledger);
            for recipient in &outbound.recipients {
                if ledger.already_sent(&outbound.uid, outbound.sequence, recipient, outbound.method) {
                    continue;
                }
                let payload = build_payload(&outbound, organizer_self);
                let (subject, text) = render(&outbound);
                let method = method_word(outbound.method);
                let mid = sender
                    .send_imip(recipient, &subject, &text, &payload, method)
                    .await?;
                ledger.record(repo, &outbound.uid, outbound.sequence, recipient, outbound.method, &mid)?;
                dirty = true;
                sent_count += 1;
                if notify_on_send {
                    let nsub = format!("[scheduling] {} → {recipient}: {}", method, outbound.summary);
                    let nbody = format!(
                        "Sent {method} for \"{}\" (uid {}, seq {}) to {recipient}.",
                        outbound.summary, outbound.uid, outbound.sequence
                    );
                    sender.notify(&nsub, &nbody).await?;
                }
            }
        }
    }

    if dirty {
        repo.commit_all(
            "pimsteward-scheduling",
            "scheduling@pimsteward.local",
            &format!("scheduling: sent {sent_count} iMIP message(s) for {commit_sha}"),
        )?;
    }
    Ok(sent_count)
}

fn resolve_sequence(outbound: &Outbound, ledger: &ledger::Ledger) -> u32 {
    let event_seq = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "SEQUENCE")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    match outbound.method {
        // CANCEL must out-rank the last thing the recipient saw.
        Method::Cancel => ledger
            .last_sequence(&outbound.uid)
            .map(|s| s + 1)
            .unwrap_or(event_seq + 1)
            .max(event_seq),
        Method::Request => event_seq,
    }
}

fn build_payload(outbound: &Outbound, organizer_self: &str) -> String {
    pimsteward_ical::imip::build_imip(
        &outbound.event_ics,
        method_word(outbound.method),
        outbound.sequence,
        organizer_self,
        &outbound.recipients,
    )
}

fn method_word(m: Method) -> &'static str {
    match m { Method::Request => "REQUEST", Method::Cancel => "CANCEL" }
}

fn render(outbound: &Outbound) -> (String, String) {
    let verb = match outbound.method { Method::Request => "Invitation", Method::Cancel => "Cancelled" };
    let subject = format!("{verb}: {}", outbound.summary);
    let when = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "DTSTART").unwrap_or_default();
    let loc = pimsteward_ical::ical::vevent_field(&outbound.event_ics, "LOCATION").unwrap_or_default();
    let body = format!("{verb}: {}\nWhen: {when}\nWhere: {loc}\nOrganizer: dan@hld.ca\n", outbound.summary);
    (subject, body)
}
```

Note on the CANCEL payload: `build_imip` re-emits `ATTENDEE` lines from `outbound.recipients`, so a CANCEL to a removed attendee correctly addresses only them.

- [ ] **Step 4: Run, confirm pass.** `cargo test -p pimsteward scheduling::` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/scheduling/
git commit -m "feat(scheduling): orchestrator (sequence, ledger dedup, notify)"
```

---

### Task 9: Daemon wiring — run scheduler after each calendar pull

**Goal:** When `[scheduling].enabled` and the alias has `send` capability, run `run_scheduling` after each calendar pull that produced a commit, using a real `Sender` backed by the forwardemail `Client`.

**Files:**
- Modify: `crates/pimsteward/src/daemon.rs`

**Acceptance Criteria:**
- [ ] A `ClientSender` implements `scheduling::Sender` using `Client::send_imip` and `Client::send_email` (notify → `NewMessage { to: vec![alias], ... }`).
- [ ] `spawn_calendar_puller` accepts an `Option<Arc<SchedulingCtx>>` (client + alias + notify flag); after a successful pull with a non-empty `commit_sha`, it calls `run_scheduling`.
- [ ] Scheduling is wired only when `cfg.scheduling.enabled`, the provider is forwardemail (has a `Client`), and `cfg.permissions.check_write(Resource::Mail)` (or the project's send-capability check) is Ok. Otherwise the puller runs exactly as before.
- [ ] `cargo build -p pimsteward` succeeds; existing daemon tests still pass.

**Verify:** `cargo test -p pimsteward` → all pass; `cargo build -p pimsteward` clean.

**Steps:**

- [ ] **Step 1: Add `ClientSender`** near the other daemon helpers in `daemon.rs`

```rust
use crate::scheduling::{self, Sender};
use crate::forwardemail::{Client, writes::NewMessage};

struct ClientSender {
    client: Client,
    alias: String,
}

#[async_trait::async_trait]
impl Sender for ClientSender {
    async fn send_imip(
        &self, to: &str, subject: &str, text_body: &str, payload: &str, method: &str,
    ) -> Result<String, crate::error::Error> {
        let v = self.client.send_imip(to, subject, text_body, payload, method).await?;
        Ok(v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string())
    }
    async fn notify(&self, subject: &str, body: &str) -> Result<(), crate::error::Error> {
        let msg = NewMessage {
            folder: String::new(),
            to: vec![self.alias.clone()],
            cc: vec![], bcc: vec![],
            subject: subject.to_string(),
            text: Some(body.to_string()),
            html: None,
            in_reply_to: None,
            references: vec![],
        };
        self.client.send_email(&msg).await?;
        Ok(())
    }
}
```

(Confirm `NewMessage` is re-exported from `forwardemail`; the exact path is `crate::forwardemail::writes::NewMessage` per Task 7's file. Adjust the `use` to match the actual module visibility — make the struct `pub` and re-export if needed.)

- [ ] **Step 2: Add an optional scheduling context to the puller.** Change `spawn_calendar_puller` (`daemon.rs`) to take one extra parameter:

```rust
fn spawn_calendar_puller(
    period: Duration,
    source: Arc<dyn CalendarSource>,
    repo: Arc<Repo>,
    alias: String,
    scheduling_ctx: Option<Arc<(ClientSender, bool)>>, // (sender, notify_on_send)
) -> tokio::task::JoinHandle<()> {
```

Inside the loop, after the existing `match result { Ok(s) => ..., Err(e) => ... }`, add:

```rust
                if let Ok(ref s) = result {
                    if let (Some(ctx), Some(sha)) = (&scheduling_ctx, s.commit_sha.clone()) {
                        let now = chrono::Utc::now();
                        let (sender, notify) = (&ctx.0, ctx.1);
                        match scheduling::run_scheduling(
                            &repo, &sha, sender, &alias, notify, now,
                        ).await {
                            Ok(0) => {}
                            Ok(n) => tracing::info!(sent = n, "scheduling sent iMIP messages"),
                            Err(e) => tracing::error!(error = %e, "scheduling failed"),
                        }
                    }
                }
```

Note: `PullSummary.commit_sha` is `Option<String>` (confirmed in `pull/calendar.rs`). `&alias` here is the organizer self-address — for the dan daemon this is `dan@hld.ca`.

- [ ] **Step 3: Build the context at the call site** (the calendar-pull spawn block around `daemon.rs:383`)

```rust
        if let Some(calendar_source) = provider.build_calendar_source()? {
            let scheduling_ctx = if cfg.scheduling.enabled {
                match fe_provider.as_ref() {
                    Some(fe) => Some(Arc::new((
                        ClientSender { client: fe.client().clone(), alias: alias.clone() },
                        cfg.scheduling.notify_on_send,
                    ))),
                    None => {
                        tracing::warn!("scheduling.enabled but provider has no send client; disabled");
                        None
                    }
                }
            } else {
                None
            };
            handles.push(spawn_calendar_puller(
                Duration::from_secs(cfg.pull.calendar_interval_seconds),
                calendar_source,
                repo.clone(),
                alias.clone(),
                scheduling_ctx,
            ));
        }
```

(`Client` must be `Clone` — it wraps a `reqwest::Client` + alias creds; confirm and derive/clone accordingly. `fe.client()` returns `&Client` per `provider/forwardemail.rs:122`.)

- [ ] **Step 4: Build + test**

Run: `cargo build -p pimsteward && cargo test -p pimsteward`
Expected: clean build, all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/src/daemon.rs crates/pimsteward/src/forwardemail/writes.rs
git commit -m "feat(daemon): run scheduling after each calendar pull (gated)"
```

---

### Task 10: E2E acceptance gate — rocky@ invitee (real ForwardEmail)

**Goal:** USER-ORDERED GATE. An automated test that drives the real `dan@hld.ca` calendar with `rocky@hld.ca` as the sole invitee and asserts the iMIP REQUEST / REQUEST-update / CANCEL messages actually arrive in rocky@'s inbox, then cleans up.

> **USER-ORDERED GATE — NON-SKIPPABLE.** This task was requested by the user in the current conversation. It MUST NOT be closed by walking around it, by declaring it "verified inline", or by substituting a cheaper check. Close only after every item in `acceptanceCriteria` has been re-validated independently, with output captured.

**Files:**
- Create: `crates/pimsteward/tests/e2e_scheduling.rs`

**Acceptance Criteria:**
- [ ] `#[ignore]`d test `e2e_rocky_invite_lifecycle` runs only via `cargo test -p pimsteward --test e2e_scheduling -- --ignored`.
- [ ] It reads dan + rocky alias credentials from env (`PIMSTEWARD_DAN_USER/PASS`, `PIMSTEWARD_ROCKY_USER/PASS`) and skips with a clear message if unset.
- [ ] Creates an event (organizer dan@, attendee rocky@, future DTSTART) via the forwardemail REST `Client`; runs a pull into a temp repo with watermark set to before the create; runs `run_scheduling`; asserts a `METHOD:REQUEST` for the event UID lands in rocky@'s INBOX (poll up to ~60s).
- [ ] Updates the event start time; re-pulls; re-runs scheduling; asserts a second `METHOD:REQUEST` (higher SEQUENCE) arrives.
- [ ] Deletes the event; re-pulls; re-runs; asserts a `METHOD:CANCEL` arrives.
- [ ] Deletes the created event and the received rocky@ test messages on teardown (best-effort).

**Verify:** `cargo test -p pimsteward --test e2e_scheduling -- --ignored --nocapture` → `e2e_rocky_invite_lifecycle ... ok`, with captured log lines showing each method received.

**Steps:**

- [ ] **Step 1: Write the e2e test.** It uses the crate's public API: build a forwardemail `Client` for each alias, the `CalendarSource`/`CalendarWriter` from the provider, `pull::calendar::pull_calendar`, `scheduling::run_scheduling`, and `Client::list_messages`/REST search to read rocky@.

```rust
//! Real-ForwardEmail end-to-end acceptance gate for organizer-side iMIP.
//! Uses rocky@hld.ca (a mailbox Dan owns) as the sole invitee, so no real
//! contact is ever emailed. Ignored by default — requires network + creds.

use chrono::Utc;
use std::time::Duration;

fn env(k: &str) -> Option<String> { std::env::var(k).ok() }

fn creds() -> Option<(String, String, String, String)> {
    Some((
        env("PIMSTEWARD_DAN_USER")?, env("PIMSTEWARD_DAN_PASS")?,
        env("PIMSTEWARD_ROCKY_USER")?, env("PIMSTEWARD_ROCKY_PASS")?,
    ))
}

#[tokio::test]
#[ignore = "real ForwardEmail e2e; run explicitly with --ignored and creds set"]
async fn e2e_rocky_invite_lifecycle() {
    let Some((dan_user, dan_pass, rocky_user, rocky_pass)) = creds() else {
        eprintln!("SKIP: set PIMSTEWARD_DAN_USER/PASS and PIMSTEWARD_ROCKY_USER/PASS");
        return;
    };

    // 1. Build dan client + calendar source/writer; rocky client for inbox reads.
    //    (Construct via the same provider helpers the daemon uses; see
    //    crates/pimsteward/src/provider/forwardemail.rs for the builder.)
    let dan = pimsteward::forwardemail::Client::new_basic(&dan_user, &dan_pass);
    let rocky = pimsteward::forwardemail::Client::new_basic(&rocky_user, &rocky_pass);

    let uid = format!("pimsteward-e2e-{}", Utc::now().format("%Y%m%dT%H%M%SZ"));
    let cal_id = first_calendar_id(&dan).await;

    // helper closures defined below: create_event, update_event, delete_event,
    // sync_and_schedule (pull into temp repo + run_scheduling), wait_for_method.

    // 2. CREATE → expect REQUEST
    create_event(&dan, &cal_id, &uid, "20990901T180000Z").await;
    let repo = sync_and_schedule(&dan, &dan_user).await;
    assert!(wait_for_method(&rocky, &uid, "REQUEST", 60).await,
        "no METHOD:REQUEST received for {uid}");

    // 3. UPDATE time → expect another REQUEST
    update_event(&dan, &cal_id, &uid, "20990901T190000Z").await;
    let _ = sync_and_schedule_existing(&repo, &dan, &dan_user).await;
    assert!(wait_for_method(&rocky, &uid, "REQUEST", 60).await,
        "no updated METHOD:REQUEST received for {uid}");

    // 4. DELETE → expect CANCEL
    delete_event(&dan, &cal_id, &uid).await;
    let _ = sync_and_schedule_existing(&repo, &dan, &dan_user).await;
    assert!(wait_for_method(&rocky, &uid, "CANCEL", 60).await,
        "no METHOD:CANCEL received for {uid}");

    // 5. Teardown (best-effort): delete event + received rocky test messages.
    let _ = delete_event(&dan, &cal_id, &uid).await;
    cleanup_rocky(&rocky, &uid).await;
}
```

- [ ] **Step 2: Implement the helper functions** in the same test file. These wrap existing crate APIs:
  - `first_calendar_id(&dan)` → `dan.list_calendars()` → pick the calendar whose name is `Dan` (the one in real data, id `4341D594-...`); fall back to the first.
  - `create_event/update_event/delete_event` → build an `.ics` string (organizer `dan@hld.ca`, attendee `rocky@hld.ca`, the given UID/DTSTART) and call `dan.create_calendar_event` / `update_calendar_event` / `delete_calendar_event` (the REST methods confirmed at `forwardemail/calendar.rs:121-173`).
  - `sync_and_schedule(&dan, &dan_user)` → make a `tempfile::tempdir` `Repo`; run `pull::calendar::pull_calendar` to populate + commit; capture HEAD; set the watermark to the commit *before* the create by doing an initial empty pull first OR by writing the watermark to the parent SHA so the event commit is "after"; then run a second pull that introduces the event and call `scheduling::run_scheduling` with the resulting `commit_sha`, a real `ClientSender`-equivalent, `notify_on_send=false`, `Utc::now()`. Return the repo for reuse.
  - `wait_for_method(&rocky, uid, method, secs)` → poll `rocky.list_messages("INBOX")` (or REST search) every 3s up to `secs`; fetch each candidate message body; return true when a message contains both the `uid` and `METHOD:<method>`.
  - `cleanup_rocky(&rocky, uid)` → delete rocky@ INBOX messages whose body contains `uid`.

> Implementation note for the engineer: prefer reusing the daemon's real wiring rather than re-deriving it. If constructing `Client`/source/writer directly is awkward, factor the daemon's "build sender + run one scheduling pass for a repo" into a small `pub` helper in `scheduling` or `daemon` and call it from the test, so the e2e exercises the *same* code path the daemon runs. Do NOT fork a parallel implementation just for the test.

- [ ] **Step 3: Find the credentials.** The daemon reads them from `alias_user_file`/`alias_password_file` (`/run/pimsteward-secrets/pimsteward-{dan,rocky}-alias-{user,password}`). For the test, export them:

```bash
export PIMSTEWARD_DAN_USER="$(sudo cat /proc/$(pgrep -f pimsteward-dan-config | head -1)/root/run/pimsteward-secrets/pimsteward-dan-alias-user)"
# ...same for -password and the rocky alias (pimsteward-rocky-config / pimsteward-rocky-alias-*)
```

(Run on saturn where the containers live; `export PATH=/run/wrappers/bin:$PATH` for sudo. The exact secret filenames are under each container's `/run/pimsteward-secrets/`.)

- [ ] **Step 4: Run the gate**

Run: `cargo test -p pimsteward --test e2e_scheduling -- --ignored --nocapture`
Expected: `test e2e_rocky_invite_lifecycle ... ok`, with captured lines confirming REQUEST, updated REQUEST, and CANCEL were each received by rocky@.

- [ ] **Step 5: Commit**

```bash
git add crates/pimsteward/tests/e2e_scheduling.rs
git commit -m "test(scheduling): e2e acceptance gate via rocky@ invitee"
```

---

## Deployment (after Task 10 passes)

Not a coded task — operational rollout, done once the gate is green:

1. In the dan provider Nix config, add `[scheduling]\nenabled = true` (and confirm `notify_on_send` defaults true). The config is rendered to `pimsteward-dan-config.toml` via the dotfiles Nix module.
2. `make update` on the host, which rebuilds and restarts the pimsteward containers.
3. On first start with scheduling enabled, the daemon's first calendar pull sets the watermark and sends nothing — confirm a `scheduling: set activation watermark` commit appears in `/var/lib/pimsteward-dan` and no iMIP is sent for historical events.
4. Create a real test event in Apple Calendar with rocky@ as invitee; confirm the notify email arrives and rocky@ receives the invite.
5. Once satisfied, set `notify_on_send = false` if desired.

---

## Self-Review

- **Spec coverage:** Problem/diagnosis → Tasks 1,6 (organizer filter, address parsing). Trigger via pull commit → Tasks 3,9. Watermark + past-DTSTART → Task 4. Organizer/recipient resolution → Task 6. Lifecycle REQUEST/CANCEL → Tasks 6,8. iMIP MIME + cal-address normalization → Tasks 1,7. Idempotency ledger → Tasks 5,8. Config + notify → Tasks 2,8,9. Permissions gating → Task 9. Unit tests → Tasks 1–8. E2E gate → Task 10. Deployment → final section. All spec sections covered.
- **Placeholder scan:** No TBD/TODO; every code step shows code. Task 10's helper bodies are described at a finer grain rather than fully written because they wrap already-confirmed REST methods and intentionally reuse daemon wiring — the engineer must not fork a parallel path. This is a deliberate instruction, not a placeholder.
- **Type consistency:** `Method`/`Outbound`/`EventChange`/`IcalAddress` used consistently; `method_word`/`method_str` both map the same enum to the same strings ("REQUEST"/"CANCEL"); `build_imip(ics, method, sequence, organizer_email, attendee_emails)` signature matches all call sites; `run_scheduling(repo, commit_sha, sender, organizer_self, notify_on_send, now)` matches the daemon call and the test.
