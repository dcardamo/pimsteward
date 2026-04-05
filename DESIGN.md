# pimsteward — Design

_A permission-aware MCP mediator for [forwardemail.net](https://forwardemail.net)
personal data (mail, calendar, contacts, sieve filters), with time-travel
backup built in._

This document explains **what pimsteward is, why it works the way it does,
and what it deliberately isn't**. For the deeper implementation plan and
phased build history, see [PLAN.md](PLAN.md). For operational details about
the forwardemail REST API as it exists today, see
[docs/api-findings.md](docs/api-findings.md).

## The problem

Giving an AI assistant access to your personal data is a one-way trust
decision unless you have receipts. Once you let a model read your calendar
and write new events, you've also let it delete events, rename contacts,
install sieve filters, and reorganise your mail — and you have no
general-purpose way to see what it did or undo it selectively.

Existing options don't fit:

- **Disk-level backups** (restic, borg, rsync.net) capture blobs of state
  on a schedule. They're great at "restore this whole disk to yesterday"
  but awful at "show me the 3 events the AI changed this morning."
- **Provider backups** (your email host's own backups) vary wildly in
  retention, access model, and granularity. Most don't expose a per-item
  history at all.
- **MCP servers that talk directly to your provider** give the AI read and
  write access with no intermediate layer. There's nowhere to attach
  attribution, nowhere to enforce per-resource permissions, and no undo.

## The solution

pimsteward is a small daemon that sits between an AI assistant (or any
MCP client) and a forwardemail alias. It does three things at once:

1. **Mediates.** The AI talks to pimsteward via MCP. pimsteward holds the
   forwardemail credentials, enforces a per-resource permission matrix,
   and executes the AI's requests on its behalf.
2. **Backs up.** Every change — from the AI, from your IMAP/CalDAV client,
   from forwardemail's own delivery — is captured into a local git
   repository as a time-series log. One file per resource, raw content
   stored verbatim.
3. **Restores.** Any resource can be rolled back to any prior commit,
   selectively. The AI can even drive its own cleanup, through a
   dry-run/confirm flow that prevents it from restoring something
   different from what it showed you.

The receipts layer and the permission layer are the same layer, and both
are built on git.

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│  AI assistant (any MCP client)                                 │
│      │  MCP over stdio: search_email, create_event, restore…   │
│      ▼                                                         │
│  ┌────────────────────────────────────────────────┐            │
│  │  pimsteward  (daemon, holds credentials)       │            │
│  │    ├─ MCP server       (read + write tools)    │            │
│  │    ├─ Permissions      (none/read/readwrite)   │            │
│  │    ├─ Pull loop        (forwardemail → git)    │            │
│  │    ├─ Write path       (git WAL → API → git)   │            │
│  │    ├─ Restore          (git @ T → API)         │            │
│  │    └─ Safety guardrail (test aliases only)     │            │
│  └────────────────────────────────────────────────┘            │
│      │ REST + basic auth        │ git via CLI                  │
│      ▼                          ▼                              │
│   forwardemail.net      time-series git repo                   │
│   (authoritative)       (local, offsite-mirrored as a file)    │
└────────────────────────────────────────────────────────────────┘
```

### The three loops

| Loop    | Trigger             | What it does                                          |
| ------- | ------------------- | ----------------------------------------------------- |
| Pull    | systemd timer       | poll forwardemail, diff against git tree, commit     |
| Write   | MCP tool call       | stage + apply + commit with caller attribution        |
| Restore | MCP tool or CLI     | read git tree @ T, compute diff, apply as new commit  |

MCP is intentionally NOT part of the daemon. The daemon runs the pull
loops. AI clients spawn `pimsteward mcp` as a child process with stdio
transport (matching the pattern forwardemail's own MCP server uses). This
keeps the daemon as a low-privilege pull-only service, and each MCP
client gets its own isolated process on demand.

### Storage layout

```
<repo_root>/
├── .git/
└── sources/forwardemail/<alias_slug>/
    ├── calendars/<calendar_id>/_calendar.json
    ├── calendars/<calendar_id>/events/<event_uid>.ics
    ├── contacts/default/<contact_uid>.vcf
    ├── contacts/default/<contact_uid>.meta.json
    ├── mail/<folder_path>/_folder.json
    ├── mail/<folder_path>/<message_id>.json
    ├── mail/<folder_path>/<message_id>.meta.json
    └── sieve/<script_name>.sieve
```

Key design points:

- **Raw content stored verbatim.** vCards, iCalendar events, and sieve
  scripts come from forwardemail as text strings (in `content` or `ical`
  fields depending on resource type) and land on disk exactly as
  received. No parsing, no re-serialisation, no canonicalisation.
- **Metadata sidecars** (`.meta.json`) are separate files for anything
  mutable that shouldn't pollute the diff of the raw content (etag,
  flags, modseq, etc.).
- **Folder paths used as directory names** for mail so `git blame
  INBOX/abc.json` is more useful than `fold-<mongoid>/abc.json`.
- **Calendar directories use the calendar id** (not name) since names can
  change and collide; the `_calendar.json` manifest inside carries the
  human-readable name.

### Why git as the store

Content-addressed storage. Free dedup. Diff/blame/log/time-travel. The
best tooling in the world. An AI already knows git. Branching and tagging
are free if we ever need them.

Specifically, pimsteward uses `git` via shell-out (not a library like
`gix` or `jj-lib`). The v1 writes are strictly linear and single-writer
— append a commit, update `HEAD`, move on. Using the `git` binary is
~20 lines of code, has no version-churn liability, and produces a repo
that any tool on earth can inspect. When concurrent writers or tree-level
merges become a real need, swapping in gix is a local change.

### Why no git remote by default

The plan originally called for pushing the backup repo to a self-hosted
git server. In practice, the backup volume (`/data/Backups/...` or
equivalent) is already mirrored offsite by whatever disk-level backup
tool the host runs. Adding a git remote for the backup repo is a second
copy, a second credential set, and a second attack surface with marginal
benefit. pimsteward writes to a local path only; offsite comes from the
host's existing backup story.

## Permission model

Deliberately coarse in v1: one setting per resource type, applied
globally.

```toml
[permissions]
email    = "read"        # AI can search/read but never modify
calendar = "read_write"  # full CRUD
contacts = "read_write"
sieve    = "read_write"
```

Each MCP tool is tagged with a resource and required access level. The
dispatcher runs the check at call time, and tools whose resource is
`none` return a typed permission error.

### Why not per-folder or per-calendar rules in v1

- Finer rules encourage confident mis-config. `email = "read"` is
  obviously correct; `email.folders.inbox = "readwrite", email.folders.trash = "none"`
  is a config-review hazard.
- The forwardemail API doesn't offer ACL hooks at the folder level, so
  enforcement would be purely client-side. That means pimsteward's
  permission file becomes the _only_ source of truth, which means
  mis-config becomes silent over-share.
- Users who need finer control can run two pimsteward instances with
  different allowlists.

v2 can add per-folder rules when the cost of mis-config is understood.

## The plan_token safety dance (restore)

Every restore is split into two MCP calls:

1. **Dry-run.** The AI calls `restore_<resource>_dry_run` with a
   resource identifier and a git SHA. pimsteward reads the historical
   state from git, compares to live, and returns a typed plan object
   plus a deterministic `plan_token` derived from the bytes of the plan.
2. **Apply.** The AI calls `restore_<resource>_apply` passing the plan
   and the token verbatim. pimsteward re-computes the token from the
   submitted plan and refuses if they don't match.

The binding means the AI cannot dry-run a small plan, show it to you,
get approval, and then apply a different larger plan under the same
mandate. Any byte-level modification to the plan changes the token. The
check is in pimsteward, not trusted to the client.

```text
  AI ──── dry_run(path, at_sha) ──▶ pimsteward
                                       │ read git @ sha, fetch live,
                                       │ compute plan + sha256(plan)
  AI ◀────── { plan, plan_token } ─────┤
          (human inspects plan)
  AI ────── apply(plan, token) ───────▶ pimsteward
                                       │ sha256(plan) == token ?
                                       │ if match: execute + git commit
                                       │ else: refuse
  AI ◀────────── ok / error ───────────┘
```

Bulk restore uses the same pattern but the plan is heterogeneous (a list
of per-resource sub-plans). Both the outer bulk `plan_token` and each
sub-plan's individual token are verified before any mutation runs.

## Attribution

Every write produces a git commit with:

- **`author.name`** = name of the caller (`ai`, `pimsteward-pull`,
  `manual`, or any string the MCP client supplies)
- **`author.email`** = `<caller>@pimsteward.local`
- **Commit message** = human-readable subject + a structured YAML block
  recording the tool name, resource, resource id, caller, free-text
  reason, and full arguments:

```
contacts: create Alice Smith

---
tool: create_contact
resource: contacts
resource_id: 69d1…
caller: ai
reason: "user asked me to add Alice to contacts"
args: {"emails":[{"type":"home","value":"alice@example.com"}],"full_name":"Alice Smith"}
---
```

`git log --author=ai -- sources/forwardemail/*/contacts/` then lists
every AI-made contact change with full context. No external audit log
is needed — git is the audit log.

## Safety guardrail

One rule, enforced at every destructive-test entry point:

**No test can run against a live forwardemail account unless the alias
contains `_test`.**

Implemented in `src/safety.rs::assert_test_alias`, which _panics_ (not
Result) on any of:

1. Alias doesn't contain the substring `_test` (case-insensitive).
2. Alias is on an explicit deny list of known production addresses.
3. Alias's localpart matches a known production owner on a known
   production domain (defense in depth for future list expansion).
4. String doesn't look like an email at all.

`assert_test_environment` wraps `assert_test_alias` and additionally
refuses repo paths under known production directories
(`/data/Backups/…`, `/var/lib/pimsteward`).

Panics, not `Result`, because a `Result` can be ignored with `let _ =
...`. The guard exists precisely to stop careless code. It must be
impossible to accidentally bypass.

**Test harness enforcement:** every e2e test file uses
`tests/common/mod.rs::E2eContext::from_env` to construct its client.
That function calls both guards at startup. There is no public
alternative path. E2e tests also require `PIMSTEWARD_RUN_E2E=1` in the
environment — without it, `from_env` panics immediately, so running
`cargo nextest run` (without the env var) never hits the network.

The boundary behaviour is itself tested: eight safety tests in
`tests/e2e_safety.rs` verify the guard fires for production aliases,
missing `@`, production paths, and passes for legitimate test aliases.

## Credential isolation

pimsteward's forwardemail credentials are encrypted for a dedicated
`pimsteward` target in the credentials store. No other machine and no
other service (AI runtimes, dev shells, ops tooling) has the age key to
decrypt them. The daemon runs in its own systemd-nspawn container with
read-only bind-mounted access to the decrypted secret files at
`/run/pimsteward-secrets/`. The container is the security boundary:
cryptographic isolation from everything else, filesystem isolation at
runtime.

In an ambient-authority world where an AI container might someday
enumerate every file the host user can read, this layering matters. The
AI container cannot decrypt pimsteward's credentials even if it pivots,
because those credentials were never encrypted with its key.

## Testing model

Three tiers, strict boundaries:

| Tier         | Location             | Mocks         | Network |
| ------------ | -------------------- | ------------- | ------- |
| unit         | `src/**/*.rs` `#[cfg(test)]` | allowed       | no      |
| integration  | `tests/integration_test.rs` | wiremock only | no      |
| e2e (safety) | `tests/e2e_safety.rs`       | none          | no      |
| e2e (live)   | `tests/e2e_*.rs`            | none          | yes     |

Live e2e tests are gated on both `PIMSTEWARD_RUN_E2E=1` AND the safety
guardrail. They each create unique per-process resources, verify the
full lifecycle against the real forwardemail API, and clean up after
themselves — even on partial failure.

```sh
# Quick check — unit + integration + safety boundary tests, no network
cargo nextest run

# Full rigor — everything, hits the real test API, cleans up after itself
PIMSTEWARD_RUN_E2E=1 cargo nextest run --run-ignored all
```

Current count: 56 tests. 46 network-free (run on every commit), 10 live
(run before releases).

## MCP tool surface

27 tools total, grouped by responsibility:

### Read (7)

`search_email`, `list_folders`, `list_calendars`, `list_events`,
`list_contacts`, `list_sieve`, `history`

### Write (12)

`create_contact`, `update_contact`, `delete_contact`,
`install_sieve_script`, `update_sieve_script`, `delete_sieve_script`,
`create_event`, `update_event`, `delete_event`,
`update_email_flags`, `move_email`, `delete_email`

### Restore (8)

`restore_contact_dry_run` / `_apply`, `restore_sieve_dry_run` / `_apply`,
`restore_calendar_event_dry_run` / `_apply`, `restore_mail_dry_run` /
`_apply`, `restore_path_dry_run` / `_apply` (bulk).

Every write tool is gated on `readwrite` for its resource. Every write
tool takes an optional `reason` string that flows into the git commit
body. Every restore tool is a two-call dance with plan_token binding.

## Non-goals

pimsteward is explicitly NOT:

- **A generic backup tool.** Use restic/borg for disk-level backup.
  pimsteward only backs up what it can read from the forwardemail API.
- **A PIM client.** Keep using your preferred IMAP/CalDAV/CardDAV app.
  pimsteward doesn't render calendars or let you type emails.
- **A multi-provider sync tool.** v1 is forwardemail-only. The native
  protocols still work against forwardemail, so a v2 could add them as a
  fallback, but supporting Gmail/Outlook/iCloud would be a different
  project.
- **A real-time push layer.** Forwardemail's webhook support is
  forwarding-based, not change-feed based, and their storage is
  zero-knowledge so server-side push isn't architecturally possible. The
  pull loop is the only mechanism and that's fine for cadences measured
  in minutes.
- **A rate-limit bypass.** All AI reads and writes still count against
  your alias's API quota. pimsteward just mediates them.

## Resolved since initial v1

Items that were deferred in earlier revisions of this document and have
since been built. Listed here so the delta is traceable.

| Was deferred                          | Resolved by |
| ------------------------------------- | ----------- |
| Per-folder / per-calendar permissions | **V2.1** — `EmailPermission::Scoped` + `CalendarPermission::Scoped` with per-folder and per-calendar-id overrides, back-compat with flat TOML |
| True `.eml` write-once store          | **V2.3** — Forwardemail's REST response already includes a `raw` field by default; pull loop extracts it into `<id>.eml` with a sidecar `meta.json` |
| Native mail source fallback           | **V2.2** — `MailSource` trait + `ImapMailSource` using `async-imap` + `tokio-rustls`, verified live against `imap.forwardemail.net:993` |
| Native CalDAV source                  | **V2.4** — `DavCalendarSource` using raw HTTP PROPFIND/REPORT via reqwest + quick-xml, verified live against `caldav.forwardemail.net` |
| Native CardDAV source                 | **V2.4** — `DavContactsSource` sibling implementation, verified live against `carddav.forwardemail.net` (different subdomain from CalDAV) |
| Automated re-APPEND on mail restore   | **V2.4** — `MailOperation::Append { target_folder, raw_bytes }` reads the `.eml` from git at `at_sha` and POSTs it via `Client::append_raw_message`; the `Unrestorable` variant is gone |
| Weekly `git gc --auto` timer          | **V2.4** — `daemon::spawn_gc_timer` runs `git gc --auto` on a 7-day tokio interval against the backup repo |
| `CONTRIBUTING.md`                     | **V2.4** — added with build/test/style instructions and the safety guardrail requirement for e2e tests |
| Attachment dedup `_attachments/<sha256>` | **V2.5** — `extract_attachments` parses `nodemailer.attachments[]`, writes content-addressed blobs to `_attachments/<sha256>`, sidecar `<id>.attachments.json` references them |
| Explicit adaptive rate-limit backoff  | **V2.5** — `backoff_for_remaining()` pure helper + `backoff_if_throttled()` in `send()`/`send_json()`. Tiered thresholds: <100→500ms, <50→2s, <10→10s, 0→30s |
| IMAP `CHANGEDSINCE` filtering         | **V2.5** — `MailSource::list_messages` returns `ListResult` with `all_ids` + `changed`. IMAP source issues `FETCH 1:* (...) (CHANGEDSINCE <m>)` when UIDVALIDITY matches stored value; `_folder.json` persists `modify_index`/`uid_validity` between pulls |
| IMAP `IDLE` for push notifications    | **V2.5** — `idle_loop()` runs a dedicated IMAP IDLE connection on INBOX, signals the mail puller via `tokio::sync::Notify`. Opt-in via `forwardemail.imap_idle = true`. Periodic ticker remains as safety net |
| Scoped email write permissions + `create_draft` tool | **V2.5** — removed resource-level baseline gate so per-folder overrides are authoritative; `create_draft` tool saves structured messages to Drafts folder |
| Richer contact restore (full vCard)   | **V2.5** — `Recreate` and `Update` now POST/PUT the raw historical vCard content; forwardemail parses it server-side preserving all fields |
| Calendar event If-Match via CalDAV etags | **V2.5** — `CalendarEvent.etag` populated from CalDAV getetag; `EventMeta` persists it; `update_calendar_event` accepts `if_match` parameter; MCP `update_event` tool exposes it |
| IMAP write path                        | **V2.5** — `MailWriter` trait with REST + IMAP impls; IMAP uses UID STORE/COPY+EXPUNGE for flags/moves/deletes; MCP server holds `Arc<dyn MailWriter>` |
| Canonical cross-source message id      | **V2.5** — `sha256(Message-ID header)[..16]` as filename stem; source-specific id preserved in `meta.json`; safe to switch `mail_source` between REST and IMAP without wiping |

## Deferred (and why)

Items that are still deliberately out of scope. Each is a reasonable
future improvement when the need is concrete.

### From the original list

| # | Deferred                              | Reason |
| - | ------------------------------------- | ------ |
| 12 | **Dedicated `get_*` MCP tools**       | `list_*` tools return full content for contacts/events/sieve; individual `get_*` would be redundant. `search_email` covers the mail case. |

### Design decisions (not planned)

Deliberate architectural choices, not gaps.

| # | Decision                              | Rationale |
| - | ------------------------------------- | --------- |
| 5 | **No retention / pruning of git history** | Disk is cheap, history is the product. The backup repo is append-only by design; `git gc --auto` handles object compaction. If a repo grows unwieldy, `git filter-repo` is the manual escape hatch. |
| 9 | **No webhook-driven push ingest**     | IMAP IDLE (`imap_idle = true`) provides sub-minute push notifications without exposing a public HTTPS endpoint. Webhooks would add attack surface, require partial-delivery handling, and duplicate what IDLE already does better. |
| 10 | **One alias per daemon instance**    | Clean isolation: each alias gets its own git repo, credentials, permission matrix, and systemd unit. No cross-alias data leakage. NixOS containers make per-alias instances cheap. Multiple aliases = multiple instances. |

## Open-source friendly

- MIT licensed.
- Neutral voice throughout. No references to any specific user's email
  addresses or domains. Configuration examples use `alias@example.com`.
- Documentation ([README.md](README.md), this file, and
  [PLAN.md](PLAN.md)) is the primary on-ramp for anyone reading the
  code cold.
- The test suite is runnable against any forwardemail test alias with a
  `_test` in its name — the safety guardrail is built in. Set
  `PIMSTEWARD_TEST_ALIAS_USER_FILE` and `PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE`
  to the credentials of a disposable alias, `PIMSTEWARD_RUN_E2E=1`, and
  run `cargo nextest run --run-ignored all`.

## Build status

Everything from the original plan is built and verified. What shipped:

- Safety guardrail with boundary tests
- Pull loops for all four resource types (mail, calendar, contacts, sieve)
- Forwardemail REST client with typed DTOs and rate-limit tracking
- Git-backed time-series store with attributed commits
- MCP server with 27 tools (read + write + restore + bulk)
- Daemon mode with per-resource tokio timers
- Systemd-nspawn container deployment (NixOS module)
- Credential isolation via a dedicated secrets target
- 56 tests passing (46 network-free + 10 live against a `_test` alias)

See [PLAN.md](PLAN.md) for the phased implementation history and the
specific design decisions that were made along the way.
