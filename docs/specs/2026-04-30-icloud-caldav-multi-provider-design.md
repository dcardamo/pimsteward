# Multi-provider support: iCloud CalDAV alongside forwardemail

**Status:** design approved, pre-implementation
**Date:** 2026-04-30
**Scope:** add iCloud CalDAV (calendars only) as a second provider; keep forwardemail untouched as the primary; reframe pimsteward as provider-aware rather than forwardemail-only.

## Background

pimsteward today is a single-provider tool: every daemon connects to forwardemail and serves mail, calendar, contacts, and sieve out of one process backed by one git repo. The README, the configuration schema, and the source layout all assume forwardemail is the only backend.

Users — starting with the project owner — increasingly hold data on more than one provider. Specifically: iCloud calendars, accessed via CalDAV. The goal is to back those up and let an AI assistant interact with them through the same MCP mediation/audit/restore story pimsteward already provides for forwardemail data.

The architectural decision made during brainstorming is to support iCloud as a **separate daemon** (separate process, separate git repo, separate bearer token, separate MCP server entry on AI clients), not as additional sources merged into the existing forwardemail daemon. The motivation is credential isolation, blast-radius isolation, backup-isolation, and a simpler mental model than a monolithic "all your stuff" daemon.

The architectural surface this *does* require is a provider abstraction inside the existing pimsteward binary: one binary, multiple provider modes, selected by config.

## Goals

1. Run a second pimsteward daemon configured for iCloud CalDAV against an Apple ID, backing up calendars to its own git repo.
2. Expose CRUD + restore MCP tools for those calendars on a separate HTTP MCP endpoint.
3. Do this within the existing pimsteward binary — same release, same Nix derivation, same test harness.
4. Forwardemail daemon behaviour, on-wire shape, storage layout, and operational characteristics are unchanged.
5. Reframe README + project description to make multi-provider explicit. Forwardemail remains the primary, fully-featured target; iCloud is documented as a partial-functionality option (calendars only).
6. e2e tests exercise the iCloud path locally against a dedicated `pimsteward_test` calendar in the project owner's iCloud account, behind an opt-in env var. No CI access to iCloud.
7. No secrets committed to the repo at any point.

## Non-goals (this design)

- iCloud CardDAV (contacts). Not used by the project owner; would add an untested code path.
- iCloud IMAP (`@icloud.com` mail). Not used by the project owner.
- JMAP / Fastmail / Gmail / generic-email-provider support. Out of scope until there's a real user with a testable account.
- Cross-provider unification (one daemon serving forwardemail + iCloud as a merged calendar view). Explicitly rejected during brainstorming in favour of separate daemons.
- A standalone `pimsteward-icloud` binary. Same binary, different config.
- Migration tooling for existing forwardemail-mode configs. The schema change is additive — existing configs continue to parse.

## Decisions locked during brainstorming

| Decision                              | Choice                                                                                       |
| ------------------------------------- | -------------------------------------------------------------------------------------------- |
| Multi-provider topology               | One daemon = one provider. Separate process, port, repo, bearer token, MCP entry per provider. |
| Binary structure                      | Same `pimsteward` binary; provider selected by which `[provider.*]` section is in config.   |
| Capability matrix per provider        | forwardemail = mail + calendar + contacts + sieve + email_send. iCloud = calendar only.      |
| Permission keys validated per-provider| iCloud config rejects `email`, `contacts`, `sieve`, `email_send` at load time (hard error).  |
| Storage layout for iCloud daemon      | Same `calendars/<cal_id>/` shape as forwardemail. No mail/contacts/sieve subtrees.           |
| iCloud CalDAV discovery               | Full RFC 6764 well-known + principal-URL + calendar-home-set walk. No hardcoded URLs.        |
| iCloud auth                           | App-specific password. Username + password stored in dotvault, mounted as files.             |
| e2e test target                       | A real `pimsteward_test`-named calendar in the project owner's iCloud account.                |
| e2e test guard                        | Calendar display name must contain `_test`; calendar URL is verified at test setup.          |
| e2e opt-in mechanism                  | `PIMSTEWARD_RUN_E2E_ICLOUD=1` plus `PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE` / `..._PASSWORD_FILE`. |
| CI for iCloud                         | None. iCloud e2e is local-opt-in only.                                                        |
| README                                | Rewritten to lead with provider-agnostic framing. forwardemail is "primary, full feature set"; iCloud is "example of partial-functionality provider". |
| Stable identity for iCloud calendars  | iCalendar `UID` for events, calendar URL (or its trailing UUID) for the calendar itself. CalDAV gives stable IDs natively — no synthetic ID story. |

## Architecture

### Provider abstraction

A provider is a config-selected bundle of:

1. A capability set: which resources (mail, calendar, contacts, sieve, send) are supported.
2. Concrete `Source` and `Writer` trait implementations for those resources.
3. A pull-task list (one task per supported resource).
4. An MCP tool registration list (the union of read/write/restore tools for the supported resources, filtered by the permission policy).
5. A credentials shape and how to load it from disk.

The existing forwardemail code (`src/forwardemail/*`) becomes the implementation of the `forwardemail` provider. No code there needs to move; the daemon just learns to ask "which provider is configured?" before wiring things up.

A new module `src/icloud/` holds the iCloud provider:

```
src/icloud/
├── mod.rs           # provider-level wiring: capabilities, trait impls, mcp tool list
├── caldav.rs        # iCloud-flavoured CalDAV client: discovery, basic-auth, quirks
└── discovery.rs     # RFC 6764 well-known + principal/calendar-home-set walk
```

`src/source/caldav.rs` is generic CalDAV transport. iCloud-specific flavour — the well-known URL, the User-Agent header iCloud demands, the strict If-Match etag handling — lives in `src/icloud/caldav.rs` as a thin wrapper that delegates to the generic transport.

### Same binary, config-selected provider

`Config` gains a `provider` enum. Exactly one provider section must be present:

```rust
enum Provider {
    Forwardemail(ForwardemailConfig),
    IcloudCaldav(IcloudCaldavConfig),
}
```

Daemon startup:

1. Parse config; require exactly one `[provider.*]` section.
2. Build the provider's `Source`/`Writer` trait objects from credentials + URLs.
3. Build the pull-task list from the provider's capabilities ∩ the permission policy.
4. Build the MCP tool list from the provider's capabilities ∩ the permission policy.
5. Spawn the HTTP server and the pull tasks.

The existing `daemon.rs` is the right place for the dispatch. The provider sections register themselves via a small registry pattern (or an enum match — registry is overkill for two providers).

### Configuration schema

#### Forwardemail (existing, restated for clarity)

```toml
log_level = "info"

[provider.forwardemail]
api_base            = "https://api.forwardemail.net"
alias_user_file     = "/run/pimsteward-secrets/forwardemail-alias-user"
alias_password_file = "/run/pimsteward-secrets/forwardemail-alias-password"

mail_source     = "rest"   # or "imap"
imap_idle       = false
calendar_source = "rest"   # or "caldav"
contacts_source = "rest"   # or "carddav"

managesieve_host = "imap.forwardemail.net"
managesieve_port = 4190

[storage]
repo_path = "/var/lib/pimsteward"

[pull]
mail_interval_seconds     = 300
calendar_interval_seconds = 300
contacts_interval_seconds = 900
sieve_interval_seconds    = 3600

[permissions]
email      = "read"
calendar   = "read_write"
contacts   = "read_write"
sieve      = "read"
email_send = "denied"
```

The current top-level `[forwardemail]`/`[storage]`/`[pull]`/`[permissions]` schema is preserved for backwards-compat: if a top-level `[forwardemail]` section is present and there is no `[provider.*]` section, parsing treats it as `[provider.forwardemail]`. This is the only migration concession; new configs should use the namespaced form.

#### iCloud CalDAV (new)

```toml
log_level = "info"

[provider.icloud_caldav]
discovery_url = "https://caldav.icloud.com/"
username_file = "/run/pimsteward-secrets/icloud-username"
password_file = "/run/pimsteward-secrets/icloud-app-password"
user_agent    = "pimsteward/0.x (iCloud CalDAV)"   # iCloud rejects empty UA

[storage]
repo_path = "/var/lib/pimsteward-icloud"

[pull]
calendar_interval_seconds = 300
# mail/contacts/sieve intervals not allowed: error at config-load if present

[permissions]
calendar = "read_write"     # or "read", or scoped per-calendar-id
# email/contacts/sieve/email_send not allowed: error at config-load if present
```

Validation rules at config load:

- Exactly one `[provider.*]` block.
- `[pull]` keys are restricted to the provider's capabilities. Setting `mail_interval_seconds` under iCloud CalDAV is a hard error with a clear message.
- `[permissions]` keys are restricted similarly.
- All `*_file` paths must exist and be readable at startup. Daemon fails loud if any are missing.

### Capability matrix

| Resource  | forwardemail | icloud_caldav |
| --------- | ------------ | ------------- |
| mail      | yes (REST + IMAP) | no       |
| calendar  | yes (REST + CalDAV) | yes (CalDAV) |
| contacts  | yes (REST + CardDAV) | no    |
| sieve     | yes (REST + ManageSieve) | no |
| send      | yes (REST → SMTP)        | no |

This matrix lives in a new `src/provider/` module — a small abstraction crate-internal to pimsteward, holding the `Provider` trait, the capability set type, and the registry that maps a config block to a concrete provider impl. It drives:

- Permission key validation.
- MCP tool registration.
- Pull task spawning.
- `get_permissions` MCP tool output.

### iCloud CalDAV adapter

#### Discovery (RFC 6764)

1. PROPFIND on `https://caldav.icloud.com/.well-known/caldav` → follow 3xx redirect to user's principal URL (typically `https://pNN-caldav.icloud.com/<principal-id>/`).
2. PROPFIND on principal URL with `<calendar-home-set/>` → returns the calendar home collection URL.
3. PROPFIND `Depth: 1` on calendar home → returns child calendar collections, each with `displayname`, `getctag`, `resourcetype`, `current-user-privilege-set`.
4. Filter to collections with `resourcetype` containing `<C:calendar/>`.

Discovered calendar URLs and ctags are cached in `_calendar.json` (existing manifest shape). On reconnect, ctags are checked first to skip unchanged calendars.

#### Read path

For each known calendar:

- If `getctag` matches the cached value, skip.
- Otherwise REPORT `calendar-query` for all events; diff against the local tree by iCalendar UID; commit changes.

This is the same logic the existing CalDAV source uses against forwardemail. The only iCloud-specific piece is the discovery walk and the User-Agent header.

#### Write path

- Create event: PUT to `<calendar-url>/<uid>.ics` with `If-None-Match: *`. Re-fetch via REPORT; commit.
- Update event: PUT with `If-Match: <etag>`. On 412 (etag conflict), the writer returns a structured error; the MCP layer surfaces it to the AI as a precondition-failed response so the AI can re-read and retry deliberately.
- Delete event: DELETE with `If-Match: <etag>`. Same conflict semantics.

iCloud-specific quirks to handle defensively:

- Empty User-Agent → 403. Always send a UA.
- iCloud occasionally returns `200 OK` for PUTs that should be `201 Created`. Treat both as success.
- Calendar-home-set principal URL changes by Apple ID region — must be discovered, not hardcoded.

### Storage & deployment

| Aspect              | forwardemail (existing)              | iCloud (new)                          |
| ------------------- | ------------------------------------ | ------------------------------------- |
| Container           | `pimsteward-dan`, `pimsteward-rocky` | `pimsteward-icloud-dan` (suggested)   |
| Repo path           | `/var/lib/pimsteward`                | `/var/lib/pimsteward-icloud`          |
| Port                | 8100, 8101                           | 8102                                  |
| Bearer token file   | `pimsteward-mcp-bearer-token`        | `pimsteward-icloud-mcp-bearer-token`  |
| Credential files    | forwardemail-alias-{user,password}   | icloud-{username,app-password}        |
| `.mcp.json` entry on rocky | `pimsteward-rocky`             | adds `pimsteward-icloud-rocky` (if rocky should see iCloud calendars) |

Each daemon is its own systemd unit on the host. Backups, restore tooling, and the existing rockycc verification script all operate per-container — adding a second container does not require collapsing repos or sharing state.

### Permission model — provider-aware

The existing flat / scoped permission shapes are unchanged. What's new is config-load validation:

- An iCloud config with `email = "read"` fails at startup with:
  `error: provider 'icloud_caldav' does not support resource 'email'. Remove this permission key.`
- `get_permissions` MCP tool returns only the keys the provider supports. iCloud daemon's response is a single-key map: `{"calendar": "read_write"}`.
- Scoped per-calendar-id permissions work for iCloud as for forwardemail; the calendar IDs are iCloud's calendar UUIDs.

### MCP tool registration

Tools register iff:

1. The provider supports the underlying resource (capability matrix).
2. The permission policy for that resource is not `none`.

For the iCloud daemon, the registered tools are:

- Calendar read: `list_calendars`, `list_events`, `get_event` (if exists)
- Calendar write (only if calendar permission ≥ `read_write`): `create_event`, `update_event`, `delete_event`
- Restore: `restore_calendar_event_dry_run`, `restore_calendar_event_apply`, `restore_path_dry_run`, `restore_path_apply`
- Cross-cutting: `history`, `get_permissions`

Mail, contacts, sieve, email_send tools are not registered. The AI literally cannot see them.

### Audit / commit shape

Identical to today. The git author identity is the same (`pimsteward-pull` for pull commits, `ai` or `PIMSTEWARD_CALLER` for write commits). The audit trailer's `resource:` field for an iCloud-originated calendar event is still `calendar`. There is no provider tag in the audit trailer — the daemon's identity (which repo it's writing to) carries that information implicitly.

This keeps audit consumers unchanged. If a future need emerges for distinguishing forwardemail-vs-iCloud commits within a single audit pipeline, a `provider:` field can be added later additively.

## Testing

### Unit tests

- New: `src/icloud/discovery.rs` exercised against a CalDAV PROPFIND mock that returns iCloud-shaped XML responses (well-known redirect, principal URL, calendar-home-set, calendar collection).
- New: `src/icloud/caldav.rs` exercised for User-Agent enforcement, etag handling, and the 200-vs-201 PUT response normalisation.
- Existing `src/source/caldav.rs` unit tests continue to cover the generic CalDAV transport.

### e2e tests

Gated by env vars; opt-in only:

```sh
export PIMSTEWARD_RUN_E2E_ICLOUD=1
export PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE=/path/to/icloud-username
export PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE=/path/to/icloud-app-password

cargo nextest run --run-ignored all -- icloud
```

Safety guards (defense in depth, in `src/safety.rs`):

1. The target calendar's `displayname` (returned by PROPFIND) must contain the substring `_test`. Tests panic immediately if the display name does not match — same pattern as the forwardemail `_test` alias guard, mirrored for calendars.
2. The target calendar's URL must be in a known explicit allowlist of test calendar URLs (configurable; documented in `CONTRIBUTING.md`). This catches the case where a calendar is renamed to `_test`-something on a production iCloud account.
3. The repo path used by the test must be a `tempfile::tempdir()` — not `/var/lib/pimsteward*`.

The recommended setup, documented in `CONTRIBUTING.md`:

- Create an iCloud calendar named `pimsteward_test` (already done by project owner).
- Generate an app-specific password at appleid.apple.com.
- Store the username (Apple ID email) and app password in dotvault.
- Point the test env vars at the dotvault paths.

### Secrets handling

- iCloud Apple ID and app-specific password are managed via dotvault, the same way forwardemail credentials are.
- They are deployed to the production iCloud container via systemd `LoadCredential` (or whatever existing pattern the forwardemail container uses — to be matched, not invented).
- They are deployed to the developer machine by the developer, outside the repo.
- `.gitignore` is reviewed; if any new file pattern could carry secrets (e.g. local test fixtures), it's added to `.gitignore` defensively.
- No secret values appear in any committed file: not in tests, not in fixtures, not in spec, not in README examples. Examples use placeholder paths only.

## README rewrite

The current README is a forwardemail evangelism document. It will be reframed to make multi-provider support explicit while preserving the forwardemail-as-primary framing.

### Changes

1. **Lead paragraph:** "pimsteward is a permission-aware MCP mediator + git backup for personal data — primarily for forwardemail.net, with optional standards-based providers (e.g. iCloud CalDAV) for partial functionality."
2. **New section near the top: "Supported providers."** A small matrix:

   | Provider | Mail | Calendar | Contacts | Sieve | Send |
   | -------- | ---- | -------- | -------- | ----- | ---- |
   | forwardemail.net | ✅ | ✅ | ✅ | ✅ | ✅ |
   | iCloud (CalDAV)  | —  | ✅ | —  | —  | —  |

   With a note: "Each provider runs as its own pimsteward daemon — separate process, separate git repo, separate MCP endpoint. Add as many as you have accounts."
3. **Existing "Why forwardemail" section:** kept, but renamed "Why forwardemail (the primary target)" and slightly narrowed to make clear it's about *that* provider, not about pimsteward as a whole.
4. **Existing "Non-goals":** the line `Not a multi-provider sync tool. v1 is forwardemail-only by design` is replaced with: `Not a generic any-provider client. We add providers we use; iCloud CalDAV is here because the project owner has iCloud calendars. Fastmail/JMAP, Gmail, generic IMAP-only providers are out unless someone with that backend brings testable code.`
5. **New section near the architecture diagram: "Running multiple providers."** Short — points at the example iCloud config, explains the "one daemon per provider" model, links to the iCloud-specific gotchas (app-specific password, calendar-only).
6. **Banner subtitle (`assets/banner.svg`):** "permission-aware MCP mediator for forwardemail.net" → "permission-aware MCP mediator for your personal data" (or similar). SVG edit, not code.
7. **CLAUDE.md (project file):** the deploy-verification section is forwardemail-specific. Updated to make clear those steps apply to the forwardemail daemon; iCloud daemon gets its own (parallel) verification steps documented in `CONTRIBUTING.md`.

The README rewrite ships as part of this work, not as a follow-up — Dan called it out explicitly during brainstorming.

## Open questions / risks

1. **iCloud rate limits.** Apple does not publish a CalDAV rate limit. If a 5-minute pull interval is too aggressive for a large calendar set, the interval becomes per-deployment-tunable (already is, via `[pull]`). Worst case: pre-populate `_calendar.json` with conservative ctag-skip behaviour to minimise PROPFIND volume.
2. **Apple ID 2FA + app-specific password rotation.** App-specific passwords don't expire automatically but can be revoked from appleid.apple.com. The daemon's failure mode on rotation is a 401 on every CalDAV request; surfaced as a clear "iCloud auth failed — rotate app-specific password" log line. Not silent.
3. **iCloud principal URL drift.** Apple has historically moved users between `pNN-caldav.icloud.com` shards. Discovery handles this via the well-known redirect, but the cache (calendar URLs) becomes invalid if the shard changes. Detection: 301 → re-run discovery → invalidate cache. Implementation: re-run discovery on any non-2xx response from a cached calendar URL.
4. **Calendar permission shape.** iCloud calendars have read-only system calendars (Birthdays, Holidays). The daemon reads them fine but writes return 403. This must be a typed error so the MCP layer can hand the AI a useful message rather than a generic "write failed."
5. **Restore safety.** Restore-to-iCloud uses the existing dry-run + plan_token mechanism. Risk: an AI-driven restore against a calendar the user actually wants intact. The existing safety net (dry-run, token, audit) applies unchanged.

## Out of scope (future work, not this design)

- iCloud CardDAV (contacts).
- iCloud IMAP (`@icloud.com` mail).
- JMAP/Fastmail.
- Gmail API.
- Generic CalDAV against arbitrary servers (the iCloud adapter is iCloud-flavoured; a `[provider.generic_caldav]` would need its own design).
- Cross-provider restore (restoring an event from one provider onto another).
- Cross-provider search.
- A "pimsteward-cli" that talks to multiple daemons.

## Verification (post-implementation)

After deploying the iCloud daemon:

1. iCloud daemon starts, discovery succeeds, `_calendar.json` written for each iCloud calendar.
2. `list_calendars` MCP tool returns the iCloud calendars.
3. `list_events` returns expected events from `pimsteward_test`.
4. `create_event` against `pimsteward_test` creates and commits.
5. `update_event` updates and commits.
6. `delete_event` deletes and commits.
7. `restore_calendar_event_dry_run` + `_apply` round-trips against a deleted event.
8. e2e suite green.
9. forwardemail daemon untouched and verified via existing rockycc verification script.
