# pimsteward — Implementation Plan

*A permission-aware MCP mediator for personal data on forwardemail.net (mail,
calendar, contacts, sieve), with time-travel backup built in.*

## What this is

pimsteward sits between an AI assistant and a [forwardemail.net](https://forwardemail.net)
mailbox. It does three things at once:

1. **Mediates.** Exposes a small, typed set of MCP tools that the AI uses to
   read and write your PIM data. The AI never sees your credentials, never
   calls forwardemail directly, and is subject to a per-resource permission
   policy (`none` | `read` | `readwrite` × `{email, calendar, contacts}`).
2. **Backs up.** Every change — whether the AI made it, you made it via an
   IMAP/CalDAV client, or forwardemail itself made it — is captured into a
   local git repository as a time-series log. Each resource is one file;
   commits are atomic batches.
3. **Restores.** At any point you can ask pimsteward to rewind a file, a
   directory, or a date range back to a prior state. The AI can drive the
   restore too, but only through a dry-run-first tool that requires explicit
   confirmation to apply.

### Scope (v1) — forwardemail.net only

The source layer is tightly coupled to forwardemail's REST API. This is
deliberate: forwardemail's native CalDAV/CardDAV/IMAP endpoints still work and
are a plausible v2 fallback, but building a generic PIM mediator is a
different, larger project. **pimsteward v1 is a forwardemail tool.** The
README will say this in the first sentence so nobody on GitHub is confused.

### What this is NOT

- Not a generic backup tool (restic/borg do that better at the volume level).
- Not a PIM client (use your usual IMAP/CalDAV app).
- Not a search index — forwardemail's own search is excellent; pimsteward
  passes queries through rather than re-indexing.
- Not a rate-limit bypass — all AI reads/writes still hit forwardemail's API
  with your credentials, they're just mediated.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│  AI assistant (ai-assistant-container, Claude Desktop, any MCP client)         │
│      │  MCP: search_email, list_events, create_event, …         │
│      ▼                                                          │
│  ┌──────────────────────────────────────────────────┐           │
│  │  pimsteward  (daemon, own nspawn container       │           │
│  │   on the host, owns forwardemail credentials)      │           │
│  │    ├─ MCP server  — high-level, safe tools       │           │
│  │    ├─ Permissions — per-resource none/read/rw    │           │
│  │    ├─ Pull loop   — forwardemail → diff → git    │           │
│  │    ├─ Write path  — git WAL → forwardemail → git │           │
│  │    └─ Restore     — git @ T → forwardemail       │           │
│  └──────────────────────────────────────────────────┘           │
│      │ REST (alias-auth)         │ git (gix)                    │
│      ▼                           ▼                              │
│   forwardemail.net      /data/Backups/<host>/pimsteward/        │
│   (authoritative)       <alias_slug>/   (offsite-mirrored by the host's disk backup)  │
└─────────────────────────────────────────────────────────────────┘
```

### Four loops, one data store

| Loop    | Trigger             | Action                                                       |
| ------- | ------------------- | ------------------------------------------------------------ |
| Pull    | systemd timer       | poll forwardemail, diff against git tree, commit new state   |
| Write   | MCP tool call       | stage intended change, apply via API, commit with attribution |
| Restore | MCP tool or CLI     | read git tree @ T, compute diff vs live, apply as new commit |
| GC      | weekly systemd timer| `git gc --auto` so offsite backup stays compact               |

### Storage layout

```
/data/Backups/<host>/pimsteward/<alias_slug>/
├── .git/
├── sources/forwardemail/<alias_slug>/
│   ├── calendars/<cal_id>/_meta.json
│   ├── calendars/<cal_id>/events/<uid>.ics
│   ├── contacts/<book_id>/_meta.json
│   ├── contacts/<book_id>/<uid>.vcf
│   ├── mail/<folder_id>/_meta.json
│   ├── mail/<folder_id>/<msg_id>/raw.eml         # immutable body + headers
│   ├── mail/<folder_id>/<msg_id>/meta.json       # flags, folder — mutable
│   ├── mail/_attachments/<sha256>                # dedup
│   └── sieve/<script_name>.sieve
├── _sync/
│   └── state.json          # poll cursors, last successful run per resource
└── audit/
    └── mutations.jsonl     # append-only human-readable log of AI writes
```

### Why git (and gix specifically)

Git gives us content-addressed storage, diff/blame, time-travel, branching,
and the best tooling in the world — for free. gix (gitoxide) is chosen over
git2 (libgit2 bindings) because it's pure Rust, and over jj-lib because
pimsteward's VCS needs are linear and boring (append-only writes, single
writer). jj-lib's killer features (tree merge, operation log undo) map to a
different problem space. See `specuna/docs/research/jj-lib-vcs-layer.md` for
the analysis of when jj-lib does pay off — not this.

### Why not push to gitsrv

`/data/Backups/<host>/` is already rclone→offsite-mirrored by the host's disk backup, so the git
repo gets offsite backup for free. A gitsrv remote would add credential
handling inside the daemon, a push loop, and create a second copy reachable
by any machine that has the operator's gitsrv SSH key. Not worth the complexity.

---

## Permission model

v1 is deliberately coarse: one setting per resource type, applied globally.
Per-folder and per-calendar rules are explicitly a v2 question.

```toml
# /etc/pimsteward/config.toml

[forwardemail]
api_base = "https://api.forwardemail.net"
# Credentials come from the secrets dir, not the config file.
alias_user_file     = "/run/pimsteward-secrets/forwardemail-alias-user"
alias_password_file = "/run/pimsteward-secrets/forwardemail-alias-password"

[storage]
repo_path = "/data/Backups/<host>/pimsteward/<alias_slug>"

[permissions]
# Each: "none" | "read" | "readwrite"
email    = "read"        # AI can search/read but never modify
calendar = "readwrite"   # full CRUD
contacts = "readwrite"
sieve    = "readwrite"   # sieve rules are grouped with their resource

[pull]
# Per-resource poll cadences
calendar_interval_seconds = 300
contacts_interval_seconds = 900
email_interval_seconds    = 300

[mcp]
# Where the MCP server listens. Defaults to stdio for CLI use; pimsteward also
# supports a loopback/loopback HTTP mode so ai-assistant-container can reach it from its
# container.
listen = "unix:/run/pimsteward/mcp.sock"
# OR: listen = "http://127.0.0.1:8765"
```

The permission check happens in the tool dispatcher before any forwardemail
call. A tool is either pure-read (subject to `read`+), mutating (requires
`readwrite`), or listing (pure-read). Tools that don't match the configured
resource (e.g. `search_email` when `email = "none"`) are not exposed in the
MCP tool list at all — the AI simply can't see them.

### Why no per-folder/per-calendar rules in v1

- Simpler mental model for users.
- The underlying forwardemail API doesn't give us clean ACL hooks per folder
  anyway — we'd have to enforce client-side, which means the permission
  config becomes the only source of truth, which means mis-config becomes
  a silent over-share. Coarse + obvious is safer for v1.
- Users who need finer control can run two pimsteward instances with
  different allowlisted resources.

---

## MCP tool surface

Rendered dynamically based on the permission config. Tools are grouped by
resource; the tool dispatcher gates each one.

### Read tools (require `read` or `readwrite`)

| Tool                     | Args                                      | Returns                              |
| ------------------------ | ----------------------------------------- | ------------------------------------ |
| `search_email`           | `q?, folder?, since?, before?, flags?`    | list of message summaries            |
| `get_email`              | `id`                                      | headers + body (+ attachment refs)   |
| `list_folders`           | —                                         | folders with counts                  |
| `list_calendars`         | —                                         | calendars with metadata              |
| `list_events`            | `calendar, from, to`                      | events (reads git cache, fast)       |
| `get_event`              | `id`                                      | single event                         |
| `list_contacts`          | `q?`                                      | contacts (reads git cache)           |
| `get_contact`            | `id`                                      | single contact                       |
| `list_sieve`             | —                                         | active+inactive sieve scripts        |
| `history`                | `path, since?, until?`                    | git log filtered to a path           |
| `diff`                   | `path, from_sha, to_sha`                  | structured diff                      |

### Write tools (require `readwrite`)

| Tool                     | Args                                      | Attribution              |
| ------------------------ | ----------------------------------------- | ------------------------ |
| `update_email_flags`     | `id, add?, remove?`                       | author = AI name         |
| `move_email`             | `id, to_folder`                           | author = AI name         |
| `create_event`           | `calendar, event`                         | author = AI name         |
| `update_event`           | `id, patch`                               | author = AI name         |
| `delete_event`           | `id`                                      | author = AI name         |
| `create_contact`         | `book, contact`                           | author = AI name         |
| `update_contact`         | `id, patch`                               | author = AI name         |
| `delete_contact`         | `id`                                      | author = AI name         |
| `install_sieve_script`   | `name, body`                              | author = AI name         |
| `activate_sieve_script`  | `name`                                    | author = AI name         |
| `delete_sieve_script`    | `name`                                    | author = AI name         |
| `send_email`             | `from, to, subject, body, …`              | outbound — not in git    |

### Restore tools (always available, gated by a confirmation dance)

| Tool      | Args                                                 | Behaviour                                      |
| --------- | ---------------------------------------------------- | ---------------------------------------------- |
| `restore` | `path, at_time?, at_sha?, confirm=false`             | default dry-run: returns structured plan       |
| `restore` | `path, at_time?, at_sha?, confirm=true, plan_token`  | executes the plan returned by the dry-run call |

The `plan_token` binds the confirm call to the exact plan from the dry-run
call — so the AI can't dry-run a small change and then apply a bigger one
under the same "confirmed" mandate. This is the safety net against a rogue
AI orchestrating its own destruction.

### Attribution scheme

Every write creates a git commit with:

- `author.name` = name of the caller (from MCP session identity, or `manual`
  for CLI, or `pull` for the polling loop).
- `author.email` = `<caller>@pimsteward.local`
- Commit message: structured YAML header + human summary:
  ```
  cal: update_event "Dentist appointment"

  ---
  tool: update_event
  resource: calendars/1234/events/abc-def-ghi.ics
  caller: ai-assistant-container
  session_id: 2026-04-04T23:45:00Z-a7f3
  reason: "User asked to move appointment from 10am to 2pm"
  api_response_etag: "W/\"a3b1c\""
  ---
  ```

`git log --author=ai-assistant-container -- calendars/` then reveals every event the AI has
ever touched, with full context.

---

## Delta sync strategy

Forwardemail's REST API has a strong search capability for messages but
**no explicit `updated_since` / modseq / sync-token mechanism** for calendars,
contacts, or calendar-events. The authoritative list of endpoints and their
observed behaviours must come from the smoke test (task #5) — this plan
assumes the worst case and will be refined.

### Messages

- Use `GET /v1/messages?folder=X&since=<last_poll>` for arrivals (cheap).
- Separate pass: `GET /v1/messages?folder=X&fields=id,flags,folder,modtime`
  per folder to detect flag/move/delete. Diff against previous manifest.
- `.eml` bodies are fetched once and never re-fetched (assumed immutable —
  smoke-test confirms).
- Attachments extracted to `_attachments/<sha256>` on first fetch, referenced
  from the message's meta.

### Calendars & calendar-events

- `GET /v1/calendars` — usually a handful, list-and-diff is free.
- `GET /v1/calendar-events?calendar=X` — potentially thousands.
  Canonicalise each event to strip churn fields (DTSTAMP, LAST-MODIFIED,
  PRODID, SEQUENCE) before hashing — see smoke test for which fields actually
  drift in practice. Store the original bytes under `events/<uid>.ics`;
  maintain a sidecar `_hashes.json` with canonicalised hashes for fast diff.

### Contacts

- Same pattern as calendar-events. Smaller scale, fewer churn fields.

### Sieve

- `GET /v1/sieve-scripts` lists, each script body is short. Just diff it.

---

## Credential isolation

pimsteward is a new dotvault target, mirroring the ai-assistant-container refactor that was
just landed as task #2 (commit 5f9f83a in dotfiles).

```
/var/lib/pimsteward-secrets/
├── .config/age/key.txt                      # pimsteward's age private key
└── .config/secrets/
    ├── forwardemail-prod-alias-user
    ├── forwardemail-prod-alias-password
    └── env
```

Manifest additions:

```nix
# secrets/manifest.nix
machines.pimsteward = "age1…";

secrets.forwardemail-prod-alias-user = {
  source = ".config/secrets/forwardemail-prod-alias-user";
  target = ".config/secrets/forwardemail-prod-alias-user";
  mode = "0600";
  authorities = ["<host>"];
  targets = ["pimsteward"];  # ONLY pimsteward. Not <host>, not ai-assistant-container.
};
# forwardemail-prod-alias-password — same
```

Targets = `["pimsteward"]` means:

- ai-assistant-container cannot decrypt these even though it has its own dotvault key.
- the host's regular user cannot decrypt these with `dotvault import` because that host
  is not a recipient. (the operator can still reach them by sudo'ing into
  `/var/lib/pimsteward-secrets/` if ever needed — file-level permission, not
  cryptographic, since <host> is the authority that originally encrypted
  them.)

The nspawn container bind-mounts `/var/lib/pimsteward-secrets/.config/secrets`
read-only at `/run/pimsteward-secrets` inside the container. The pimsteward
process reads credentials from there at startup.

---

## Container + deployment

Mirrors the ai-assistant-container pattern. New file: `nixos/<host>/pimsteward.nix`.

```nix
containers.pimsteward = {
  autoStart = true;
  privateNetwork = true;
  hostAddress = "10.0.102.1";
  localAddress = "10.0.102.2";

  bindMounts = {
    "/run/pimsteward-secrets" = {
      hostPath = "/var/lib/pimsteward-secrets/.config/secrets";
      isReadOnly = true;
    };
    "/var/lib/pimsteward" = {
      hostPath = "/data/Backups/<host>/pimsteward/<alias_slug>";
      isReadOnly = false;
    };
  };

  config = { … };  # systemd unit for pimsteward.service
};
```

Any MCP client on the local network reaches pimsteward via
`http://10.0.102.2:8765` or a loopback socket, depending on final choice.

---

## Testing strategy

Three tiers, strict boundaries (matches dotfiles CLAUDE.md):

| Tier        | Location              | Mocks     | What it covers                                               |
| ----------- | --------------------- | --------- | ------------------------------------------------------------ |
| unit        | `src/**/*.rs`         | allowed   | canonicalisation, config loading, permission gate, git path mapping |
| integration | `tests/*.rs`          | wiremock  | full pull loop against a wiremock-scripted forwardemail, real git repo |
| e2e         | `tests/e2e/*.rs`      | **zero**  | against the real `test_alias@example.com` test alias, gated by `E2E=1` |

All three must pass before pimsteward ships.

### Integration test shape

For each resource type (mail, calendar, contacts, sieve), an integration test
that:

1. Spins up a `wiremock::MockServer`.
2. Scripts forwardemail responses for a list → create → update → delete
   sequence.
3. Points pimsteward at the mock.
4. Runs the pull loop, write loop, restore loop.
5. Asserts the git repo state via `gix` at each checkpoint, using `insta`
   snapshots of the commit graph and file contents.

### E2e test shape

Using the real test alias:

1. Seed the test mailbox with a known calendar/contact/message.
2. Run pimsteward's pull loop once.
3. Verify git state matches.
4. Make a mutation via the write loop. Verify it lands in forwardemail.
5. Verify it lands in git with the correct attribution.
6. Clean up.

E2e tests are destructive (they create and delete real items) so they run
against the isolated test alias, never against production_alias@example.com.

---

## Implementation phases

This is one shipment (per the operator's instruction), but internally I'll sequence
the work so each phase is independently compilable + testable. Rough order:

### Phase A — Foundations

1. Clone `dotfiles/templates/rust-service` to `~/git/pimsteward`, rename.
2. Define `Config` + `Permission` types and their TOML/env loading.
3. Set up `tracing` with a structured JSON layer for systemd-journald.
4. Write the `Error` enum.
5. Unit tests for config, permission matrix.

### Phase B — Forwardemail client

1. `forwardemail::client::Client` — reqwest wrapper with alias auth,
   retries, rate-limit handling, and typed methods matching the subset of
   endpoints we need.
2. DTO types for `Message`, `Folder`, `Calendar`, `CalendarEvent`, `Contact`,
   `SieveScript`, each with `serde`.
3. Integration tests with wiremock covering the happy and error paths.

### Phase C — Git store

1. `store::Repo` — gix wrapper. Open/init a repo, read a tree at a path,
   write a file into a new tree, commit with an author and structured
   message, walk history for a path.
2. Canonicalisation helpers for iCal and vCard (strip churn fields before
   hashing; keep original bytes verbatim).
3. Unit tests with a `tempfile` repo.

### Phase D — Pull loop

1. `pull::calendars` — list calendars, list events per calendar, write
   each event as a file, commit the batch. Track cursors in `_sync/state.json`.
2. `pull::contacts` — analogous.
3. `pull::mail` — list folders, incremental pull for new messages by date,
   full pass for mutable metadata.
4. `pull::sieve` — small, trivial.
5. Each pull step is a pure function `(client, repo, state) -> new_state`.
6. Integration test per resource with wiremock.

### Phase E — Write loop + attribution

1. `write::Mutation` enum — one variant per MCP write tool.
2. `write::apply` — stage a pending commit on a dedicated branch, execute
   the forwardemail call, either fast-forward onto main (on success) or
   record the failure and surface the error.
3. Structured commit message format.
4. Integration tests.

### Phase F — Restore

1. `restore::plan` — given a path + target-time/sha, walk git to find the
   desired state, fetch current state from forwardemail, compute the
   diff as a list of concrete API calls.
2. `restore::apply` — execute a plan, with `plan_token` binding.
3. Unit tests for diff computation; integration tests for full dry-run →
   confirm flow.

### Phase G — MCP server

1. `rmcp`-based server exposing the tool list dynamically from the
   permission config.
2. Each tool is a thin adapter over `pull`/`write`/`restore` functions.
3. Integration tests driving the MCP server via stdio with a recorded
   AI conversation.

### Phase H — Deployment

1. `nix/module.nix` — copy template, customise for pimsteward.
2. `nixos/<host>/pimsteward.nix` — container wiring.
3. Add `pimsteward` target + the two forwardemail secrets to
   `secrets/manifest.nix`.
4. `bin/update.sh` — add third dotvault import for `pimsteward` target
   (mirrors ai-assistant-container).

### Phase I — Open source polish

1. `LICENSE` — MIT.
2. `README.md` — first line: "pimsteward is a PIM steward for
   forwardemail.net." Then: what it does, why you'd want it, how to
   install (nix flake), how to configure (annotated TOML), how it backs
   up, how to restore, explicit non-goals.
3. Neutral "AI assistant" phrasing throughout — no "rocky", no "claude".
4. `CONTRIBUTING.md` — minimal, just the test tiers.
5. GitHub mirror setup (push main to the self-hosted git server AND
   `github:<user>/pimsteward`).

### Phase J — E2e test pass + ship

1. Run the full e2e suite against `test_alias@example.com`.
2. Fix whatever breaks.
3. Deploy to <host> via `make update`.
4. Observe for 24h. Commit any follow-ups.
5. Close task #6.

---

## Known open questions (resolve during smoke test — task #5)

1. **ETags / If-Match for writes** — does forwardemail honor optimistic
   concurrency on `updateContact`, `updateCalendarEvent`, `updateMessage`?
   The blog post mentions ETag validation for contacts but the reference
   docs say "Coming soon." If not honored, the write path has to tolerate
   last-writer-wins and the restore tool's safety guarantees weaken.
2. **iCal/vCard round-trip stability** — do sequential GETs of the same
   event return byte-identical payloads, or do DTSTAMP/LAST-MODIFIED/SEQUENCE
   drift on every fetch? Affects canonicalisation strategy (strip which
   fields before hashing).
3. **Message body mutability** — does `updateMessage` let you rewrite
   headers/body or only flags/folder? If immutable, the mail store plan
   (`raw.eml` write-once, `meta.json` mutable) is correct.
4. **Rate limits** — forwardemail's FAQ says "generous limits" without
   specifying. Observe real throttling behaviour during the smoke test and
   bake appropriate backoff into the client. Rate-limit headers to parse.
5. **Native protocol fallback** — confirm CalDAV/CardDAV/IMAP endpoints
   still work on forwardemail's infra for when REST polling becomes
   expensive (v2 concern, but verify the escape hatch exists).

---

## Risks and mitigations

| Risk                                                         | Mitigation                                                                        |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------- |
| Forwardemail API changes break pimsteward                    | Integration test suite against wiremock catches behaviour changes; e2e catches protocol changes; pin the smoke-test findings in `docs/api-findings.md` for future comparison |
| AI assistant manipulates restore tool to undo safeguards     | `plan_token` binding between dry-run and confirm; every restore is itself a git commit with `author=<ai>`, so it's visible in history |
| Git repo corruption                                          | `git gc --auto` weekly, integrity check on every pull loop start, offsite backup via `/data/Backups` |
| Credential leak via MCP error messages                       | `Error` enum never wraps credential values; `tracing` uses `#[tracing::instrument(skip(password))]` |
| iCal/vCard canonicalisation strips a semantically important field | Store original bytes; canonicalisation only affects *hash-for-diff*, restoration writes the original back |
| The host dies with uncommitted state                           | Pull loop commits atomically (nothing is in a "half-applied" state); write loop commits before returning success to the MCP client |

---

## Success criteria

pimsteward v1 is "done" when:

- [ ] Pull loop runs on a 5-minute systemd timer and captures all resource
      changes for `production_alias@example.com` into git.
- [ ] At least one AI client (ai-assistant-container) is wired up and can successfully
      list/read/write calendar events and contacts via the MCP tools.
- [ ] Permission config enforces `email = "read"` — writes to email return
      an error through the MCP tool, not a silent success.
- [ ] A restore dry-run returns a structured plan; confirm-with-token
      executes it; both are visible in git history.
- [ ] All three test tiers pass in CI.
- [ ] GitHub mirror is published with README, LICENSE, and neutral phrasing.
- [ ] No pimsteward credential is reachable from inside the ai-assistant-container
      container. (Verified by `cat /config/secrets/forwardemail-*`
      returning ENOENT.)
