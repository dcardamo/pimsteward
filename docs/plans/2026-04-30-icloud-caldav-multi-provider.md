# iCloud CalDAV multi-provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-extended-cc:subagent-driven-development (recommended) or superpowers-extended-cc:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add iCloud CalDAV as a second provider (calendar-only), running as its own pimsteward daemon, while leaving the existing forwardemail daemon untouched. Reframe pimsteward as provider-aware in code and documentation.

**Architecture:** A new `Provider` abstraction inside the same `pimsteward` binary lets the daemon dispatch source/writer construction, MCP tool registration, pull-task spawning, and permission validation through a provider-supplied capability set. Forwardemail is wrapped as the first `Provider` impl with no behaviour change. iCloud is added as a second, calendar-only impl using RFC 6764 CalDAV discovery against `caldav.icloud.com`, basic-auth with an Apple-ID app-specific password.

**Tech Stack:** Rust 2024, tokio, axum, rmcp (MCP HTTP), gix (git), figment (config), serde, async-trait, reqwest (existing CalDAV transport), `quick-xml` (CalDAV PROPFIND parsing — already in Cargo.lock from existing CalDAV code; verify before adding).

**User Verification:** NO — the spec defines implementer-side verification (cargo test, e2e suite, deploy verification script) but does not require human-in-the-loop sign-off. The owner reviews the spec and final result through normal commit/PR flow.

**Spec:** `docs/specs/2026-04-30-icloud-caldav-multi-provider-design.md`

---

## File Structure

### New files

- `src/provider/mod.rs` — `Provider` trait, `Capabilities` struct, error types for capability/permission mismatch, and a small registry helper.
- `src/provider/forwardemail.rs` — `ForwardemailProvider` impl wrapping the existing `src/forwardemail/` code paths.
- `src/provider/icloud_caldav.rs` — `IcloudCaldavProvider` impl wiring `src/icloud/` into the provider trait.
- `src/icloud/mod.rs` — module root for the iCloud-specific code.
- `src/icloud/discovery.rs` — RFC 6764 well-known → principal → calendar-home-set walk, returning a list of calendar collection URLs with metadata (display name, ctag, color, supported components).
- `src/icloud/caldav.rs` — `IcloudCalendarSource` + `IcloudCalendarWriter`. Uses `src/source/caldav.rs` as transport for raw CalDAV operations; layers iCloud-specific behaviour (User-Agent header, etag handling, 200-vs-201 PUT response normalisation).
- `tests/icloud_e2e.rs` — opt-in e2e suite gated by `PIMSTEWARD_RUN_E2E_ICLOUD=1`. Runs against the project owner's real `pimsteward_test` calendar.
- `tests/icloud_caldav_test.rs` — unit-level integration tests using a CalDAV PROPFIND mock (httpmock or similar — match the pattern existing tests use for the forwardemail HTTP mocks).
- `examples/config-icloud-caldav.toml` — example configuration for the iCloud daemon. No secrets; placeholder paths only.
- `nix/pimsteward-icloud.nix` — only if a Nix module already exists for the forwardemail daemon and a parallel module is the natural shape; otherwise the deployment is documented in `CONTRIBUTING.md` and the actual Nix wiring is the operator's job.

### Modified files

- `src/lib.rs` — declare `pub mod provider;` and `pub mod icloud;` (gated under `cfg(feature = ...)` is **not** required — keep it always-on; iCloud code is small and unconditional compilation simplifies tests).
- `src/config.rs` — add `Provider` enum, namespaced `[provider.*]` parsing, backwards-compat handling for top-level `[forwardemail]`, validation hooks, and a new `IcloudCaldavConfig` struct.
- `src/daemon.rs` — replace per-resource source construction with `provider.build_*()` calls; drive pull-task spawning and MCP tool registration off `provider.capabilities()`.
- `src/mcp/server.rs` — accept a `&dyn Provider` (or its capability set) so tool registration can filter by capability. Tools for resources the provider doesn't support must not register at all (return `None` from `tool_list()` and reject calls in dispatch with a clear error).
- `src/permission.rs` — add a method `validate_against_capabilities(&self, caps: &Capabilities) -> Result<(), Error>` that errors when a permission key references a resource the provider doesn't support.
- `src/safety.rs` — add `assert_icloud_test_calendar(...)` analogous to the existing `_test` alias guard. Same panic-immediately semantics. Same explicit deny mechanism for the repo path.
- `README.md` — multi-provider rewrite (lead, supported-providers matrix, "Why forwardemail" narrowed, non-goals updated, "Running multiple providers" section added).
- `CONTRIBUTING.md` — iCloud setup walkthrough (Apple ID, app-specific password, `_test` calendar creation, env vars).
- `CLAUDE.md` — note that the deploy-verification protocol applies per-daemon; document the iCloud daemon's verification command parallel to the forwardemail one.
- `assets/banner.svg` — update subtitle from "permission-aware MCP mediator for forwardemail.net" to a provider-agnostic phrasing.
- `.gitignore` — add `*.icloud-app-password`, `*.icloud-username`, `**/icloud-test-creds*` defensive patterns.
- `Cargo.toml` — add `quick-xml` only if not already a dependency; check first.

---

## Phase 1 — Provider abstraction and forwardemail refactor (no behaviour change)

### Task 1: Add `Provider` trait, `Capabilities`, and the registry skeleton

**Goal:** Establish the abstraction surface without yet using it. Pure addition: no existing code is modified.

**Files:**
- Create: `src/provider/mod.rs`
- Modify: `src/lib.rs` (add `pub mod provider;`)
- Test: `src/provider/mod.rs` inline `#[cfg(test)] mod tests`

**Acceptance Criteria:**
- [ ] `Provider` trait defined with the required methods (see steps).
- [ ] `Capabilities` struct enumerates the five resources with `bool` flags.
- [ ] `Resource::iter()` helper returns all enum variants in a fixed order.
- [ ] `Capabilities::supports(Resource)` returns the flag for that resource.
- [ ] Unit test: `Capabilities::default()` has all flags `false`.
- [ ] Unit test: `Capabilities::all_calendar()` is `Capabilities { calendar: true, ..false }`.
- [ ] `cargo build` succeeds.
- [ ] `cargo test --lib provider::` passes.

**Verify:** `cargo test --lib provider::` → all tests pass.

**Steps:**

- [ ] **Step 1: Create `src/provider/mod.rs`**

```rust
//! Provider abstraction. A "provider" is the bundle of (capabilities,
//! sources, writers, MCP tool list, credentials) that pimsteward uses to
//! talk to one specific upstream service. Exactly one provider is active
//! per daemon — selected at startup by which `[provider.*]` section is
//! present in config.
//!
//! Phase 1 of the multi-provider work is pure addition: this trait and
//! `Capabilities` exist but nothing yet calls through them. Phase 2 wires
//! `daemon.rs` and the MCP server to dispatch through `&dyn Provider`.

use crate::config::Config;
use crate::error::Error;
use crate::source::{CalendarSource, ContactsSource, MailSource, MailWriter};
use std::sync::Arc;

/// Resource enum used everywhere in pimsteward. Mirrors the existing
/// `permission::Resource` set; the two will be unified in a follow-up
/// cleanup. Order is stable (used in iter()).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Mail,
    Calendar,
    Contacts,
    Sieve,
    EmailSend,
}

impl Resource {
    pub fn all() -> &'static [Resource] {
        &[
            Resource::Mail,
            Resource::Calendar,
            Resource::Contacts,
            Resource::Sieve,
            Resource::EmailSend,
        ]
    }
}

/// What a provider can do. Used to drive permission validation, MCP tool
/// registration, and pull-task spawning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub mail: bool,
    pub calendar: bool,
    pub contacts: bool,
    pub sieve: bool,
    pub email_send: bool,
}

impl Capabilities {
    pub fn supports(&self, r: Resource) -> bool {
        match r {
            Resource::Mail => self.mail,
            Resource::Calendar => self.calendar,
            Resource::Contacts => self.contacts,
            Resource::Sieve => self.sieve,
            Resource::EmailSend => self.email_send,
        }
    }

    /// Convenience for calendar-only providers (iCloud).
    pub fn calendar_only() -> Self {
        Self {
            calendar: true,
            ..Self::default()
        }
    }

    /// Convenience for the full forwardemail capability set.
    pub fn forwardemail_full() -> Self {
        Self {
            mail: true,
            calendar: true,
            contacts: true,
            sieve: true,
            email_send: true,
        }
    }
}

/// A `Provider` knows how to construct the resource-specific
/// `Source`/`Writer` trait objects, declares its capability set, and
/// supplies a stable name used in logs and the audit trailer.
///
/// Methods returning sources/writers for unsupported resources MUST
/// return `None` rather than panicking — the daemon checks
/// `capabilities()` before calling them, but the `Option` makes the
/// invariant typecheck-able.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Stable identifier (`"forwardemail"`, `"icloud_caldav"`). Used in
    /// log lines, never in user-facing strings.
    fn name(&self) -> &'static str;

    /// Capability set declared by this provider's config.
    fn capabilities(&self) -> Capabilities;

    /// The "alias-like" identity for this provider — used in pull-loop
    /// log lines and in the git author field. For forwardemail this is
    /// the alias localpart-domain (`rocky-hld.ca`); for iCloud it's the
    /// Apple ID with `@` replaced by `-`.
    fn alias(&self) -> &str;

    async fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error>;
    async fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error>;
    async fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error>;
    async fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error>;
}

/// Build the configured provider from a `Config`. This is the
/// single dispatch point that decides "forwardemail or iCloud or…".
pub fn build(_cfg: &Config) -> Result<Arc<dyn Provider>, Error> {
    // Phase 1 placeholder — real dispatch added in Task 2.
    Err(Error::config(
        "provider::build is not yet wired up — use the legacy code paths in daemon.rs",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_all_false() {
        let c = Capabilities::default();
        for r in Resource::all() {
            assert!(!c.supports(*r), "{:?} should be unsupported by default", r);
        }
    }

    #[test]
    fn calendar_only_supports_only_calendar() {
        let c = Capabilities::calendar_only();
        assert!(c.supports(Resource::Calendar));
        assert!(!c.supports(Resource::Mail));
        assert!(!c.supports(Resource::Contacts));
        assert!(!c.supports(Resource::Sieve));
        assert!(!c.supports(Resource::EmailSend));
    }

    #[test]
    fn forwardemail_full_supports_all() {
        let c = Capabilities::forwardemail_full();
        for r in Resource::all() {
            assert!(c.supports(*r), "{:?} should be supported", r);
        }
    }
}
```

- [ ] **Step 2: Wire the module into the crate**

Edit `src/lib.rs` and add `pub mod provider;` next to the other module declarations. (Read the file first to find the right location among the existing `pub mod` lines.)

- [ ] **Step 3: Verify it builds and tests pass**

Run: `cargo build && cargo test --lib provider::`
Expected: PASS — three `provider::tests::*` tests pass; no warnings introduced.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/provider/mod.rs
git commit -m "provider: add Provider trait and Capabilities (no wiring yet)"
```

---

### Task 2: Implement `ForwardemailProvider` wrapping the existing forwardemail code paths

**Goal:** The current daemon dispatches through inline match-on-config-enum logic. Replace those construction sites with a `ForwardemailProvider` that returns the same source/writer trait objects. Behaviour-preserving refactor — every existing test must still pass with no edit.

**Files:**
- Create: `src/provider/forwardemail.rs`
- Modify: `src/provider/mod.rs` (export the new struct, update `build()`)
- Modify: `src/daemon.rs` (use the provider for source construction; remove inline match arms)
- Modify: `src/mcp/server.rs` (factory closure uses provider; tool list reads capabilities)
- Test: existing test suite must pass unchanged.

**Acceptance Criteria:**
- [ ] `ForwardemailProvider` implements `Provider`, with `capabilities()` returning `Capabilities::forwardemail_full()`.
- [ ] `daemon::run` constructs the provider via `provider::build(&cfg)` and uses it for every source/writer it currently builds inline.
- [ ] `mcp::PimstewardServer` factory closure builds via the provider too.
- [ ] No tests modified. `cargo test` passes with the same number of tests as before this task.
- [ ] No new clippy warnings.

**Verify:** `cargo test && cargo clippy -- -D warnings` → all green.

**Steps:**

- [ ] **Step 1: Create `src/provider/forwardemail.rs`**

The struct holds the parsed `ForwardemailConfig`, the loaded credentials, and lazily-constructed `Client`. Each `build_*` method matches on the existing `*_source` enum and returns the same `Arc<dyn Trait>` the daemon currently constructs inline.

```rust
//! Forwardemail provider. Wraps the existing `src/forwardemail/` and
//! `src/source/` code paths into a `Provider` impl. No new behaviour —
//! every method here mirrors what `daemon.rs` was doing inline before
//! the provider abstraction was introduced.

use crate::config::{
    CalendarSourceKind, Config, ContactsSourceKind, ForwardemailConfig, MailSourceKind,
};
use crate::error::Error;
use crate::forwardemail::Client;
use crate::provider::{Capabilities, Provider};
use crate::source::{
    imap::ImapConfig, CalendarSource, ContactsSource, DavCalendarSource,
    DavContactsSource, ImapMailSource, MailSource, MailWriter, RestCalendarSource,
    RestContactsSource, RestMailSource,
};
use std::sync::Arc;

pub struct ForwardemailProvider {
    cfg: ForwardemailConfig,
    user: String,
    password: String,
    alias: String,
    client: Client,
}

impl ForwardemailProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        let cfg = top_cfg.forwardemail.clone();
        let (user, password) = top_cfg.load_credentials()?;
        let alias = user.replace('@', "-");
        let client = Client::new(cfg.api_base.clone(), user.clone(), password.clone())?;
        Ok(Self {
            cfg,
            user,
            password,
            alias,
            client,
        })
    }

    fn imap_config(&self) -> ImapConfig {
        ImapConfig {
            host: self.cfg.imap_host.clone(),
            port: self.cfg.imap_port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for ForwardemailProvider {
    fn name(&self) -> &'static str {
        "forwardemail"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::forwardemail_full()
    }

    fn alias(&self) -> &str {
        &self.alias
    }

    async fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error> {
        Ok(Some(match self.cfg.mail_source {
            MailSourceKind::Rest => Arc::new(RestMailSource::new(self.client.clone())),
            MailSourceKind::Imap => Arc::new(ImapMailSource::new(self.imap_config())),
        }))
    }

    async fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        // Existing daemon writes always go through REST — keep that.
        Ok(Some(Arc::new(RestMailSource::new(self.client.clone()))))
    }

    async fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error> {
        Ok(Some(match self.cfg.calendar_source {
            CalendarSourceKind::Rest => {
                Arc::new(RestCalendarSource::new(self.client.clone()))
            }
            CalendarSourceKind::Caldav => Arc::new(DavCalendarSource::new(
                self.cfg.caldav_base_url.clone(),
                self.user.clone(),
                self.password.clone(),
            )?),
        }))
    }

    async fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error> {
        Ok(Some(match self.cfg.contacts_source {
            ContactsSourceKind::Rest => {
                Arc::new(RestContactsSource::new(self.client.clone()))
            }
            ContactsSourceKind::Carddav => Arc::new(DavContactsSource::new(
                self.cfg.carddav_base_url.clone(),
                self.user.clone(),
                self.password.clone(),
            )?),
        }))
    }
}
```

- [ ] **Step 2: Update `provider::build`**

In `src/provider/mod.rs`, replace the placeholder `build` body:

```rust
pub fn build(cfg: &Config) -> Result<Arc<dyn Provider>, Error> {
    // Phase 1: only forwardemail. Phase 2 (Task 6) adds iCloud dispatch.
    Ok(Arc::new(forwardemail::ForwardemailProvider::new(cfg)?))
}

pub mod forwardemail;
```

- [ ] **Step 3: Refactor `daemon::run` to use the provider**

In `src/daemon.rs`:
1. After parsing `cfg` and opening `repo`, construct `let provider = crate::provider::build(&cfg)?;` (an `Arc<dyn Provider>`).
2. Replace the inline match arms for `contacts_source`, `calendar_source`, and `mail_source` with `provider.build_*().await?` calls. The construction logic stays the same in behaviour, just wrapped.
3. The mail puller branch's `(u, p)` reload via `cfg.load_credentials()` becomes the provider's responsibility — `ForwardemailProvider` already holds them.
4. The `build_mail_source` helper at the top of `daemon.rs` (lines 59–81) becomes redundant for the daemon path — keep it for the MCP factory closure call site or move that call to the provider too, whichever is cleaner.
5. The `IMAP IDLE` branch needs the same `(u, p)` — expose a helper `ForwardemailProvider::imap_config()` (already drafted above) and use it.

Concrete shape inside `daemon::run`:

```rust
let provider = crate::provider::build(&cfg)?;
// ...later, where cfg.permissions.check_read(Resource::Calendar).is_ok() {
if cfg.permissions.check_read(Resource::Calendar).is_ok() && provider.capabilities().calendar {
    let Some(calendar_source) = provider.build_calendar_source().await? else { unreachable!() };
    handles.push(spawn_calendar_puller(...));
}
```

The same shape for mail and contacts. For sieve, since the existing daemon path uses `client.clone()` directly via the closure, keep that — sieve is forwardemail-specific and the iCloud provider returns no sieve. Add a capability gate: spawn the sieve puller only if `provider.capabilities().sieve`.

- [ ] **Step 4: Refactor `spawn_mcp_http_listener`'s factory closure**

The factory currently builds `Client`, mail source, repo, managesieve config, and search index inline. Move the source/writer construction behind `provider.build_*()` calls. The `Client`, managesieve config, and search index stay as today — they're forwardemail-specific. Add a follow-up TODO comment marking that `managesieve` and `Client` will move into the provider in a later iteration; the immediate goal is replacing source/writer construction.

The factory closure has to be `Send + Sync + 'static`, so `provider` needs to be cloned in once at startup:

```rust
let provider_for_factory = provider.clone();
```

Inside the closure, call `provider.build_*().await` synchronously — but the factory is sync (`-> Result<PimstewardServer, std::io::Error>`). Block on a tokio runtime handle:

```rust
let rt = tokio::runtime::Handle::current();
let mail_source = rt.block_on(provider_for_factory.build_mail_source())
    .map_err(std::io::Error::other)?
    .ok_or_else(|| std::io::Error::other("mail source not supported by provider"))?;
```

(Verify `Handle::current()` is callable from inside the factory; if not, capture the Handle outside and clone it in. The factory runs on a Tokio worker thread.)

- [ ] **Step 5: Run the existing test suite**

Run: `cargo test`
Expected: PASS — same test count as before this task. If anything fails, the refactor is wrong; do not weaken assertions.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/provider/ src/lib.rs src/daemon.rs src/mcp/server.rs
git commit -m "provider: wrap forwardemail behind Provider trait (no behavior change)"
```

---

### Task 3: Provider-aware config schema (namespaced sections + backwards-compat)

**Goal:** Introduce the `[provider.*]` namespaced config form. Keep the existing top-level `[forwardemail]` form working as backwards-compat (it parses as `[provider.forwardemail]`). Permission-key validation against the active provider's capabilities lands here too.

**Files:**
- Modify: `src/config.rs`
- Modify: `src/permission.rs` (add `validate_against_capabilities`)
- Test: new tests in both files.

**Acceptance Criteria:**
- [ ] Parsing a config with `[forwardemail]` (current shape) still works and is treated as `provider.forwardemail`.
- [ ] Parsing a config with `[provider.forwardemail]` works.
- [ ] Parsing a config with `[provider.icloud_caldav]` works (definition added in Task 6, but the field exists now).
- [ ] Parsing a config with **both** top-level `[forwardemail]` and a `[provider.*]` section is a hard error at load time.
- [ ] Parsing a config with **zero** `[provider.*]` sections **and** no top-level `[forwardemail]` is a hard error.
- [ ] `permissions.validate_against_capabilities(caps)` returns `Err` with a clear message if a permission key references an unsupported resource.
- [ ] Unit tests cover: backwards-compat parses, namespaced parses, both-set errors, neither-set errors, permission validation rejection.

**Verify:** `cargo test --lib config:: permission::` → all tests pass.

**Steps:**

- [ ] **Step 1: Add `IcloudCaldavConfig` and the provider enum to `src/config.rs`**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IcloudCaldavConfig {
    /// CalDAV discovery root. Default: `https://caldav.icloud.com/`.
    /// Discovery walks `.well-known/caldav`, then PROPFINDs for the
    /// principal URL and `calendar-home-set`.
    #[serde(default = "default_icloud_discovery_url")]
    pub discovery_url: String,

    /// File containing the Apple ID email (CalDAV basic-auth username).
    pub username_file: Option<PathBuf>,

    /// File containing an Apple-ID app-specific password (CalDAV basic-auth
    /// password). Generated at appleid.apple.com; rotate on suspected compromise.
    pub password_file: Option<PathBuf>,

    /// HTTP User-Agent for CalDAV requests. iCloud rejects empty UAs with 403.
    #[serde(default = "default_icloud_user_agent")]
    pub user_agent: String,
}

fn default_icloud_discovery_url() -> String {
    "https://caldav.icloud.com/".into()
}

fn default_icloud_user_agent() -> String {
    "pimsteward (iCloud CalDAV)".into()
}

/// Namespaced provider config holder. Exactly one variant must be set;
/// validation enforces that at load time. Top-level `[forwardemail]`
/// stays parseable (Phase 1 backwards-compat) — it migrates into
/// `provider.forwardemail` during validation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfigs {
    pub forwardemail: Option<ForwardemailConfig>,
    pub icloud_caldav: Option<IcloudCaldavConfig>,
}
```

Add to `Config`:

```rust
#[serde(default)]
pub provider: ProviderConfigs,
```

- [ ] **Step 2: Add `Config::active_provider_kind()`**

A method on `Config` that returns an enum naming the single active provider (or returns an `Error` if zero or multiple are configured). Backwards-compat: if `provider.forwardemail` is unset but the top-level `forwardemail` field has any non-default credentials configured, treat it as `provider.forwardemail`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Forwardemail,
    IcloudCaldav,
}

impl Config {
    /// Resolve the single active provider declared by this config.
    /// Errors loud on misconfiguration so we never silently start a
    /// daemon with the wrong provider.
    pub fn active_provider_kind(&self) -> Result<ProviderKind, Error> {
        let has_namespaced_fe = self.provider.forwardemail.is_some();
        let has_namespaced_ic = self.provider.icloud_caldav.is_some();
        let has_legacy_fe = self.forwardemail.alias_user_file.is_some()
            || self.forwardemail.alias_password_file.is_some();

        match (
            has_namespaced_fe || has_legacy_fe,
            has_namespaced_ic,
        ) {
            (true, true) => Err(Error::config(
                "config has both forwardemail and icloud_caldav providers; \
                 exactly one [provider.*] section per daemon is required",
            )),
            (true, false) => Ok(ProviderKind::Forwardemail),
            (false, true) => Ok(ProviderKind::IcloudCaldav),
            (false, false) => Err(Error::config(
                "no provider configured: set [provider.forwardemail] or \
                 [provider.icloud_caldav]",
            )),
        }
    }

    /// Resolve the effective `ForwardemailConfig`, merging any legacy
    /// top-level [forwardemail] block with [provider.forwardemail]
    /// (namespaced wins). Used after `active_provider_kind()` returns
    /// `ProviderKind::Forwardemail`.
    pub fn effective_forwardemail(&self) -> ForwardemailConfig {
        self.provider
            .forwardemail
            .clone()
            .unwrap_or_else(|| self.forwardemail.clone())
    }
}
```

- [ ] **Step 3: Update `ForwardemailProvider::new` to use `effective_forwardemail()`**

```rust
let cfg = top_cfg.effective_forwardemail();
```

- [ ] **Step 4: Add `Permissions::validate_against_capabilities`**

In `src/permission.rs`:

```rust
use crate::provider::{Capabilities, Resource as ProviderResource};

impl Permissions {
    /// Reject permission keys for resources the provider doesn't support.
    /// Called at startup after the provider is built.
    pub fn validate_against_capabilities(
        &self,
        caps: &Capabilities,
    ) -> Result<(), crate::error::Error> {
        // Each call below returns `Some(reason)` if a permission grants
        // (or denies non-default) something the provider can't do.
        // For a `none`/`denied` permission against an unsupported resource,
        // we *also* error — silently accepting a no-op key is a footgun.
        let bad: Vec<_> = [
            (self.email_is_set(), ProviderResource::Mail, "email"),
            (self.calendar_is_set(), ProviderResource::Calendar, "calendar"),
            (self.contacts_is_set(), ProviderResource::Contacts, "contacts"),
            (self.sieve_is_set(), ProviderResource::Sieve, "sieve"),
            (
                self.email_send_is_set(),
                ProviderResource::EmailSend,
                "email_send",
            ),
        ]
        .into_iter()
        .filter(|(is_set, res, _)| *is_set && !caps.supports(*res))
        .collect();

        if bad.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = bad.iter().map(|(_, _, n)| *n).collect();
        Err(crate::error::Error::config(format!(
            "provider does not support permission key(s): {} — remove them",
            names.join(", ")
        )))
    }
}
```

`*_is_set` helpers return `true` if the user explicitly configured a value. If the existing `Permissions` struct uses defaults and there's no way to detect "explicitly set vs default," add `#[serde(default, skip_serializing_if = "Option::is_none")]` and wrap each field in `Option<_>` — but this changes the public type. **Smaller alternative**: detect non-default values: a permission whose `default_access()` is anything other than `Access::None` (or whose scoped overrides are non-empty) counts as set. Use whichever option is the smaller diff against current `Permissions` shape — read `src/permission.rs` first to decide.

- [ ] **Step 5: Wire validation into daemon startup**

In `daemon::run`, immediately after `let provider = crate::provider::build(&cfg)?;`:

```rust
cfg.permissions.validate_against_capabilities(&provider.capabilities())?;
```

Daemon refuses to start if validation fails.

- [ ] **Step 6: Add config-parsing tests**

New tests in `src/config.rs`:

```rust
#[test]
fn provider_kind_forwardemail_legacy_top_level() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    std::fs::write(&p, r#"
[forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"
"#).unwrap();
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.active_provider_kind().unwrap(), ProviderKind::Forwardemail);
}

#[test]
fn provider_kind_forwardemail_namespaced() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    std::fs::write(&p, r#"
[provider.forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"
"#).unwrap();
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.active_provider_kind().unwrap(), ProviderKind::Forwardemail);
}

#[test]
fn provider_kind_icloud_namespaced() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    std::fs::write(&p, r#"
[provider.icloud_caldav]
username_file = "/tmp/u"
password_file = "/tmp/p"
"#).unwrap();
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.active_provider_kind().unwrap(), ProviderKind::IcloudCaldav);
}

#[test]
fn provider_kind_both_set_errors() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    std::fs::write(&p, r#"
[forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"

[provider.icloud_caldav]
username_file = "/tmp/u"
password_file = "/tmp/p"
"#).unwrap();
    let cfg = Config::load(&p).unwrap();
    let err = cfg.active_provider_kind().unwrap_err();
    assert!(err.to_string().contains("both"), "{}", err);
}

#[test]
fn provider_kind_none_set_errors() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("c.toml");
    std::fs::write(&p, r#"
log_level = "info"
"#).unwrap();
    let cfg = Config::load(&p).unwrap();
    let err = cfg.active_provider_kind().unwrap_err();
    assert!(err.to_string().contains("no provider"), "{}", err);
}
```

New test in `src/permission.rs`:

```rust
#[test]
fn validate_rejects_unsupported_keys() {
    use crate::provider::Capabilities;
    let caps = Capabilities::calendar_only();
    let perms = Permissions { /* with email = "read", calendar = "read_write" */ };
    let err = perms.validate_against_capabilities(&caps).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("email"), "{}", msg);
    assert!(!msg.contains("calendar"), "{}", msg);
}
```

(Construct the Permissions value with whatever public constructor `Permissions` exposes; if it has no convenient builder, add a tiny `#[cfg(test)]` helper.)

- [ ] **Step 7: Verify**

Run: `cargo test --lib config:: permission::`
Expected: PASS — six new tests + the one new permission test all pass; existing tests unchanged.

- [ ] **Step 8: Commit**

```bash
git add src/config.rs src/permission.rs src/provider/forwardemail.rs src/daemon.rs
git commit -m "config: provider-namespaced sections + permission-vs-capability validation"
```

---

## Phase 2 — iCloud CalDAV adapter

### Task 4: RFC 6764 CalDAV discovery walk

**Goal:** Given an Apple ID + app-specific password, walk RFC 6764 from `https://caldav.icloud.com/` to a list of `(calendar_url, displayname, ctag, color, supported_components)` tuples.

**Files:**
- Create: `src/icloud/mod.rs` (module root, just `pub mod discovery; pub mod caldav;` and a `pub struct DiscoveredCalendar { ... }`).
- Create: `src/icloud/discovery.rs`
- Modify: `src/lib.rs` (`pub mod icloud;`)
- Test: `tests/icloud_caldav_test.rs` — discovery against canned PROPFIND XML responses.

**Acceptance Criteria:**
- [ ] `discover(client, base_url, user, password) -> Result<Vec<DiscoveredCalendar>, Error>` returns the calendar list.
- [ ] Handles the `.well-known/caldav` redirect to the user's principal shard.
- [ ] PROPFINDs for `current-user-principal` to find the user's principal URL.
- [ ] PROPFINDs for `calendar-home-set` on the principal URL.
- [ ] PROPFIND `Depth: 1` on calendar-home-set, filtering to collections whose `resourcetype` contains `<C:calendar/>`.
- [ ] Returns `displayname`, calendar URL, `getctag`, `calendar-color`, `supported-calendar-component-set` for each calendar.
- [ ] iCloud-specific quirks handled: User-Agent header always set; redirect chain followed.
- [ ] Unit tests: at least four — well-known redirect, principal discovery, calendar-home-set discovery, calendar list parsing including filtering of read-only Birthdays/Holidays calendars.

**Verify:** `cargo test --test icloud_caldav_test discovery::` → all tests pass.

**Steps:**

- [ ] **Step 1: Create `src/icloud/mod.rs`**

```rust
//! iCloud CalDAV-specific code. The generic CalDAV transport lives in
//! `src/source/caldav.rs` (and `src/source/dav.rs`); this module layers
//! iCloud-specific quirks on top: RFC 6764 discovery, the User-Agent
//! requirement, and the etag-strict write semantics iCloud enforces.

pub mod caldav;
pub mod discovery;

/// One calendar collection discovered on iCloud. The `url` is the full
/// CalDAV URL of the collection — that's the stable id used in the
/// `_calendar.json` manifest and downstream MCP tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCalendar {
    pub url: String,
    pub displayname: String,
    pub ctag: Option<String>,
    pub color: Option<String>,
    /// e.g. `["VEVENT"]`, `["VEVENT", "VTODO"]`. iCloud's Reminders
    /// calendars expose only `VTODO`; we filter those out at the
    /// caller because pimsteward only handles `VEVENT`.
    pub supported_components: Vec<String>,
}
```

- [ ] **Step 2: Implement discovery**

In `src/icloud/discovery.rs`:

```rust
use crate::error::Error;
use crate::icloud::DiscoveredCalendar;
use reqwest::{Client, Method, Request, Url};
use std::time::Duration;

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_CALENDAR_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_CALENDAR_LIST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"
            xmlns:cs="http://calendarserver.org/ns/"
            xmlns:ic="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <cs:getctag/>
    <ic:calendar-color/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

pub async fn discover(
    base_url: &str,
    user_agent: &str,
    user: &str,
    password: &str,
) -> Result<Vec<DiscoveredCalendar>, Error> {
    let client = Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| Error::network(format!("building reqwest client: {e}")))?;

    // Step 1: PROPFIND .well-known/caldav for principal URL.
    let well_known = format!("{}/.well-known/caldav", base_url.trim_end_matches('/'));
    let principal_url = propfind_principal(&client, &well_known, user, password).await?;

    // Step 2: PROPFIND principal URL for calendar-home-set.
    let cal_home = propfind_calendar_home(&client, &principal_url, user, password).await?;

    // Step 3: PROPFIND calendar-home-set for calendar collections.
    let calendars = propfind_calendar_list(&client, &cal_home, user, password).await?;

    Ok(calendars)
}

async fn propfind_principal(
    client: &Client,
    url: &str,
    user: &str,
    password: &str,
) -> Result<String, Error> {
    let resp = client
        .request(Method::from_bytes(b"PROPFIND").unwrap(), url)
        .basic_auth(user, Some(password))
        .header("Depth", "0")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(PROPFIND_PRINCIPAL)
        .send()
        .await
        .map_err(|e| Error::network(format!("PROPFIND principal: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::network(format!(
            "PROPFIND principal returned {} from {}",
            resp.status(),
            url
        )));
    }
    let body = resp.text().await
        .map_err(|e| Error::network(format!("reading PROPFIND principal body: {e}")))?;
    parse_current_user_principal(&body, url)
}

// Similar for propfind_calendar_home, propfind_calendar_list.
// XML parsing uses quick-xml::events::Event::Start/End/Text streaming
// for robustness against namespace prefix variation.

fn parse_current_user_principal(xml: &str, base: &str) -> Result<String, Error> {
    // quick-xml streaming parse; locate `<d:current-user-principal>/<d:href>`.
    // Resolve relative href against `base` URL.
    todo!("full implementation in this step — see canned response in tests")
}

// parse_calendar_home_set, parse_calendar_list defined similarly.

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat>
      <prop>
        <current-user-principal>
          <href>/123456789/principal/</href>
        </current-user-principal>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

    #[test]
    fn parse_principal_extracts_href() {
        let url = parse_current_user_principal(
            PRINCIPAL_RESPONSE,
            "https://p07-caldav.icloud.com/.well-known/caldav",
        ).unwrap();
        assert_eq!(url, "https://p07-caldav.icloud.com/123456789/principal/");
    }

    // Add canned responses for calendar-home-set and calendar-list.
}
```

(The `todo!()` is for the inline placeholder — replace it with the real parser before committing. The test fixture string above is real iCloud-shaped XML; capture more from a real PROPFIND against your account if needed, scrubbed of the actual numeric principal id.)

- [ ] **Step 3: Run discovery unit tests**

Run: `cargo test --test icloud_caldav_test discovery::`
Expected: PASS — XML parsing tests succeed.

- [ ] **Step 4: Manual smoke test against real iCloud (one-time, local-only)**

Write a `pimsteward icloud-discover` debug subcommand (or a binary in `examples/`) that takes the username/password files from env vars and prints the discovered calendars. Run it once locally to confirm the live PROPFIND chain works against a real Apple ID. Do **not** check this output into git — secrets and principal IDs leak. (If the discovery works, mention this in the PR description.)

- [ ] **Step 5: Commit**

```bash
git add src/icloud/ src/lib.rs tests/icloud_caldav_test.rs
git commit -m "icloud: RFC 6764 CalDAV discovery walk"
```

---

### Task 5: iCloud `CalendarSource` + `CalendarWriter`

**Goal:** Implement read and write paths for iCloud CalDAV. Read returns the same `Vec<Calendar>` and `Vec<CalendarEvent>` shapes the existing forwardemail CalDAV source uses, so the pull loop and storage layout don't care which provider produced them.

**Files:**
- Modify: `src/icloud/caldav.rs`
- Modify: `src/source/traits.rs` — add `CalendarWriter` trait if it doesn't already exist (existing code may have it under another name; check `src/write/calendar.rs`).
- Test: `tests/icloud_caldav_test.rs` — extend with read/write tests against mocked HTTP server.

**Acceptance Criteria:**
- [ ] `IcloudCalendarSource` implements `CalendarSource`. `list_calendars()` returns the discovered calendars filtered to those with `VEVENT` in supported components. `list_events(Some(cal_id))` issues a CalDAV REPORT for the given calendar; `list_events(None)` issues REPORTs across all discovered calendars.
- [ ] `IcloudCalendarWriter` (new trait, mirrors what's needed for restore): `create_event`, `update_event`, `delete_event` against `<calendar_url>/<uid>.ics` with proper `If-Match` etag handling.
- [ ] User-Agent header always set on every request.
- [ ] Etag conflicts (412) surface as a structured `Error::PreconditionFailed { etag: Option<String> }` (or extend the existing `Error` enum) so the MCP layer can give the AI a "re-read and retry" message.
- [ ] 200 and 201 from PUT both treated as success.
- [ ] Discovery results cached behind a `tokio::sync::OnceCell` so a long-running daemon doesn't re-walk PROPFIND every list call. Cache invalidates on any non-2xx from a cached calendar URL.
- [ ] Unit tests against a mock HTTP server cover: read path, write path with etag, etag conflict, 200 PUT response handling, calendar-list filtering of `VTODO`-only calendars.

**Verify:** `cargo test --test icloud_caldav_test caldav::` → all tests pass.

**Steps:**

- [ ] **Step 1: Inspect existing CalDAV transport**

Read `src/source/caldav.rs` and `src/source/dav.rs` to understand the existing transport surface. The new iCloud source should reuse as much of this transport as possible — only iCloud-specific behaviour (User-Agent, the discovery cache, the strict etag handling) goes in `src/icloud/caldav.rs`.

- [ ] **Step 2: Implement `IcloudCalendarSource`**

```rust
use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::icloud::{discovery, DiscoveredCalendar};
use crate::source::traits::CalendarSource;
use async_trait::async_trait;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::OnceCell;

pub struct IcloudCalendarSource {
    base_url: String,
    user_agent: String,
    user: String,
    password: String,
    discovered: OnceCell<Vec<DiscoveredCalendar>>,
    client: Client,
}

impl IcloudCalendarSource {
    pub fn new(
        base_url: String,
        user_agent: String,
        user: String,
        password: String,
    ) -> Result<Self, Error> {
        let client = Client::builder()
            .user_agent(&user_agent)
            .build()
            .map_err(|e| Error::network(format!("reqwest client: {e}")))?;
        Ok(Self {
            base_url,
            user_agent,
            user,
            password,
            discovered: OnceCell::new(),
            client,
        })
    }

    async fn discovered(&self) -> Result<&[DiscoveredCalendar], Error> {
        self.discovered
            .get_or_try_init(|| async {
                discovery::discover(&self.base_url, &self.user_agent, &self.user, &self.password).await
            })
            .await
            .map(|v| v.as_slice())
    }
}

#[async_trait]
impl CalendarSource for IcloudCalendarSource {
    fn tag(&self) -> &'static str {
        "icloud-caldav"
    }

    async fn list_calendars(&self) -> Result<Vec<Calendar>, Error> {
        let discovered = self.discovered().await?;
        Ok(discovered
            .iter()
            .filter(|d| d.supported_components.iter().any(|c| c == "VEVENT"))
            .map(|d| Calendar {
                id: d.url.clone(),
                name: d.displayname.clone(),
                color: d.color.clone(),
                ctag: d.ctag.clone(),
                // ...other Calendar fields per existing struct
            })
            .collect())
    }

    async fn list_events(
        &self,
        calendar_id: Option<&str>,
    ) -> Result<Vec<CalendarEvent>, Error> {
        // For each matching calendar URL, issue a CalDAV REPORT and parse
        // the <calendar-data> blocks plus etags into CalendarEvent values.
        // Reuse the existing CalDAV REPORT helper from src/source/dav.rs
        // if it exposes the shape we need; otherwise inline a small one
        // here keyed off self.client.
        todo!("port the REPORT logic from src/source/caldav.rs, scoped to discovered URLs")
    }
}
```

(Replace `todo!()` with real REPORT logic before commit. The existing `DavCalendarSource` in `src/source/dav.rs` is the reference implementation.)

- [ ] **Step 3: Implement `IcloudCalendarWriter`**

Write methods PUT and DELETE against `<calendar_url>/<uid>.ics`. For `update_event` and `delete_event`, send `If-Match: <etag>`. On 412, return `Error::PreconditionFailed { etag: Option<String> }` (extend `Error` if needed).

```rust
pub struct IcloudCalendarWriter { /* same fields as Source */ }

impl IcloudCalendarWriter {
    pub async fn create_event(&self, calendar_url: &str, uid: &str, ical: &str)
        -> Result<String /* etag */, Error> { ... }
    pub async fn update_event(&self, calendar_url: &str, uid: &str, ical: &str, if_match: &str)
        -> Result<String, Error> { ... }
    pub async fn delete_event(&self, calendar_url: &str, uid: &str, if_match: &str)
        -> Result<(), Error> { ... }
}
```

The exact return type and method shape should match whatever the existing forwardemail-side calendar writer exposes — read `src/write/calendar.rs` first and align signatures so the MCP layer doesn't branch on provider.

- [ ] **Step 4: Unit tests with a mock HTTP server**

Use `httpmock` (or whatever the existing forwardemail tests use). Test the read happy path, the etag-conflict path, the 200-PUT-treated-as-success path, the User-Agent header, and the calendar-list filtering.

- [ ] **Step 5: Verify**

Run: `cargo test --test icloud_caldav_test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/icloud/ src/source/traits.rs tests/icloud_caldav_test.rs Cargo.toml
git commit -m "icloud: CalendarSource + CalendarWriter with iCloud quirks"
```

---

### Task 6: `IcloudCaldavProvider` + dispatch wire-up

**Goal:** Hook `IcloudCalendarSource`/`Writer` behind a `Provider` impl, and update `provider::build()` to dispatch on `Config::active_provider_kind()`.

**Files:**
- Create: `src/provider/icloud_caldav.rs`
- Modify: `src/provider/mod.rs`
- Test: `tests/icloud_caldav_test.rs` — add a provider-level test.

**Acceptance Criteria:**
- [ ] `IcloudCaldavProvider` implements `Provider`, returning `Capabilities::calendar_only()`.
- [ ] `build_calendar_source` returns `Some(Arc<IcloudCalendarSource>)`. `build_mail_source`, `build_mail_writer`, `build_contacts_source` all return `Ok(None)`.
- [ ] `Provider::alias()` returns the Apple ID with `@` replaced by `-`.
- [ ] `provider::build(&cfg)` dispatches on `cfg.active_provider_kind()` to the right impl.
- [ ] `cargo test` passes; existing forwardemail behaviour unchanged.
- [ ] Permissions referencing unsupported resources error at startup (already implemented in Task 3, verified here).

**Verify:** `cargo test` → all green.

**Steps:**

- [ ] **Step 1: Create `src/provider/icloud_caldav.rs`**

```rust
use crate::config::{Config, IcloudCaldavConfig};
use crate::error::Error;
use crate::icloud::caldav::IcloudCalendarSource;
use crate::provider::{Capabilities, Provider};
use crate::source::{CalendarSource, ContactsSource, MailSource, MailWriter};
use std::sync::Arc;

pub struct IcloudCaldavProvider {
    cfg: IcloudCaldavConfig,
    user: String,
    password: String,
    alias: String,
}

impl IcloudCaldavProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        let cfg = top_cfg
            .provider
            .icloud_caldav
            .clone()
            .ok_or_else(|| Error::config("[provider.icloud_caldav] is required"))?;
        let user = read_required_file(cfg.username_file.as_ref(), "icloud_caldav.username_file")?;
        let password =
            read_required_file(cfg.password_file.as_ref(), "icloud_caldav.password_file")?;
        let alias = user.replace('@', "-");
        Ok(Self {
            cfg,
            user,
            password,
            alias,
        })
    }
}

fn read_required_file(p: Option<&std::path::PathBuf>, name: &str) -> Result<String, Error> {
    let path = p.ok_or_else(|| Error::config(format!("{name} is required")))?;
    let s = std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("reading {name} ({}): {e}", path.display())))?
        .trim()
        .to_string();
    if s.is_empty() {
        return Err(Error::config(format!("{name} is empty")));
    }
    Ok(s)
}

#[async_trait::async_trait]
impl Provider for IcloudCaldavProvider {
    fn name(&self) -> &'static str {
        "icloud_caldav"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::calendar_only()
    }
    fn alias(&self) -> &str {
        &self.alias
    }
    async fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error> {
        Ok(None)
    }
    async fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        Ok(None)
    }
    async fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error> {
        Ok(None)
    }
    async fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error> {
        Ok(Some(Arc::new(IcloudCalendarSource::new(
            self.cfg.discovery_url.clone(),
            self.cfg.user_agent.clone(),
            self.user.clone(),
            self.password.clone(),
        )?)))
    }
}
```

- [ ] **Step 2: Update `provider::build`**

```rust
pub fn build(cfg: &Config) -> Result<Arc<dyn Provider>, Error> {
    use crate::config::ProviderKind;
    match cfg.active_provider_kind()? {
        ProviderKind::Forwardemail => Ok(Arc::new(forwardemail::ForwardemailProvider::new(cfg)?)),
        ProviderKind::IcloudCaldav => {
            Ok(Arc::new(icloud_caldav::IcloudCaldavProvider::new(cfg)?))
        }
    }
}

pub mod forwardemail;
pub mod icloud_caldav;
```

- [ ] **Step 3: MCP tool registration honours capabilities**

In `src/mcp/server.rs`, when registering tools: skip every tool whose required resource is not supported by `provider.capabilities()`. The mail/contacts/sieve/email_send tools must not appear in the iCloud daemon's tool list at all.

Concretely: a per-tool `required_resource()` lookup table that maps tool name → `Resource` (or `Option<Resource>` for cross-cutting tools like `history`). At registration time, retain only tools whose required resource is supported.

- [ ] **Step 4: Pull task spawning honours capabilities**

In `daemon::run`, gate each pull task spawn on both the permission check **and** `provider.capabilities().<resource>`:

```rust
if cfg.permissions.check_read(Resource::Calendar).is_ok()
    && provider.capabilities().calendar
{
    // spawn calendar puller
}
```

This way, a forwardemail config with `calendar = "none"` skips the puller as today, AND an iCloud config that doesn't support mail skips the mail puller without ever building a mail source.

- [ ] **Step 5: Verify**

Run: `cargo test`
Expected: PASS — all existing tests + new provider tests.

Run: `cargo clippy -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/provider/ src/daemon.rs src/mcp/server.rs
git commit -m "icloud: wire IcloudCaldavProvider into provider dispatch"
```

---

## Phase 3 — Safety + e2e

### Task 7: iCloud calendar-name `_test` safety guard

**Goal:** Defense-in-depth guard that prevents tests from writing to a non-test iCloud calendar. Mirrors the existing `_test` alias guard for forwardemail.

**Files:**
- Modify: `src/safety.rs`
- Test: `src/safety.rs` inline tests.

**Acceptance Criteria:**
- [ ] `assert_icloud_test_calendar(calendar_url, displayname)` panics immediately if:
  - `displayname` does not contain `_test` (case-sensitive match).
  - The repo path being used in the test is `/var/lib/pimsteward*`.
- [ ] Optional explicit allowlist: `assert_icloud_test_calendar` consults `PIMSTEWARD_TEST_ICLOUD_CALENDAR_URL_ALLOW` (comma-separated) and rejects any URL not on the list, if the env var is set. If the env var is unset, the URL is not pre-restricted (display name guard is the floor).
- [ ] Panic messages name what failed and how to fix it.
- [ ] Tests cover: passing case, missing-`_test` displayname, repo path under prod, allowlist mismatch.

**Verify:** `cargo test --lib safety::` → all tests pass.

**Steps:**

- [ ] **Step 1: Read existing `src/safety.rs`**

Understand the alias-guard pattern (substring match + explicit deny list + repo-path constraint).

- [ ] **Step 2: Add the iCloud guard**

```rust
/// Defense in depth for iCloud CalDAV tests. Panics immediately on any
/// failure — silent return is a footgun.
///
/// Required: `displayname` contains `_test` AND `repo_path` is under
/// `/tmp` or another tempdir (NOT `/var/lib/pimsteward*`).
/// Optional: if env var
/// `PIMSTEWARD_TEST_ICLOUD_CALENDAR_URL_ALLOW` is set, the calendar URL
/// must be one of the comma-separated entries.
pub fn assert_icloud_test_calendar(
    calendar_url: &str,
    displayname: &str,
    repo_path: &std::path::Path,
) {
    if !displayname.contains("_test") {
        panic!(
            "REFUSING to run iCloud test against calendar {:?} (url {}): \
             displayname must contain '_test'",
            displayname, calendar_url
        );
    }
    if let Ok(allow) = std::env::var("PIMSTEWARD_TEST_ICLOUD_CALENDAR_URL_ALLOW") {
        let allowed: std::collections::HashSet<&str> = allow.split(',').map(|s| s.trim()).collect();
        if !allowed.contains(calendar_url) {
            panic!(
                "REFUSING to run iCloud test against {} — not in PIMSTEWARD_TEST_ICLOUD_CALENDAR_URL_ALLOW",
                calendar_url
            );
        }
    }
    let s = repo_path.to_string_lossy();
    if s.starts_with("/var/lib/pimsteward") {
        panic!(
            "REFUSING to run iCloud test against repo path {} — production paths forbidden",
            s
        );
    }
}
```

- [ ] **Step 3: Tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn happy_path() {
        let td = tempfile::tempdir().unwrap();
        assert_icloud_test_calendar(
            "https://p07-caldav.icloud.com/123/calendars/abc/",
            "pimsteward_test",
            td.path(),
        );
    }

    #[test]
    #[should_panic(expected = "displayname must contain '_test'")]
    fn missing_test_substring_panics() {
        let td = tempfile::tempdir().unwrap();
        assert_icloud_test_calendar(
            "https://p07-caldav.icloud.com/123/calendars/abc/",
            "Personal",
            td.path(),
        );
    }

    #[test]
    #[should_panic(expected = "production paths forbidden")]
    fn prod_repo_panics() {
        assert_icloud_test_calendar(
            "https://p07-caldav.icloud.com/123/calendars/abc/",
            "pimsteward_test",
            Path::new("/var/lib/pimsteward-icloud"),
        );
    }
}
```

- [ ] **Step 4: Verify**

Run: `cargo test --lib safety::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/safety.rs
git commit -m "safety: add iCloud test-calendar guard parallel to forwardemail _test alias"
```

---

### Task 8: e2e suite against real iCloud `pimsteward_test` calendar

**Goal:** Live e2e suite exercising discovery + read + write + restore against the real iCloud `pimsteward_test` calendar in the project owner's account. Opt-in via env vars, never run in CI, never commits secrets.

**Files:**
- Create: `tests/icloud_e2e.rs`
- Modify: `CONTRIBUTING.md` — add the iCloud setup walkthrough (covered in Task 9, but reference here).

**Acceptance Criteria:**
- [ ] Tests run only when `PIMSTEWARD_RUN_E2E_ICLOUD=1` is set; otherwise they're skipped (`#[ignore]` + custom skip-with-message).
- [ ] Each test calls `assert_icloud_test_calendar(...)` before any write.
- [ ] Coverage: discovery, list_calendars, list_events on the empty `_test` calendar, create_event, fetch back, update_event with etag round-trip, delete_event, restore dry-run/apply round-trip.
- [ ] Repo path is a fresh `tempfile::tempdir()` — never reused across tests.
- [ ] After every test, the test cleans up any events it created (so the live `_test` calendar doesn't accumulate detritus across runs).

**Verify:** Locally: `PIMSTEWARD_RUN_E2E_ICLOUD=1 PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE=... PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE=... cargo nextest run --run-ignored all -- icloud_e2e` → all pass.

**Steps:**

- [ ] **Step 1: Sketch the test harness**

```rust
//! Live e2e tests against a real iCloud account. Opt-in only —
//! see CONTRIBUTING.md for setup.

use pimsteward::icloud::caldav::IcloudCalendarSource;
use pimsteward::safety::assert_icloud_test_calendar;

fn skip_unless_opted_in() -> Option<(String, String)> {
    if std::env::var("PIMSTEWARD_RUN_E2E_ICLOUD").ok().as_deref() != Some("1") {
        return None;
    }
    let user_file = std::env::var("PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE")
        .expect("PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE required");
    let pass_file = std::env::var("PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE")
        .expect("PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE required");
    let user = std::fs::read_to_string(user_file).unwrap().trim().to_string();
    let pass = std::fs::read_to_string(pass_file).unwrap().trim().to_string();
    Some((user, pass))
}

#[tokio::test]
#[ignore]
async fn discovery_finds_pimsteward_test_calendar() {
    let Some((user, pass)) = skip_unless_opted_in() else {
        eprintln!("skip: set PIMSTEWARD_RUN_E2E_ICLOUD=1 to run iCloud e2e");
        return;
    };
    let calendars = pimsteward::icloud::discovery::discover(
        "https://caldav.icloud.com/",
        "pimsteward-e2e",
        &user,
        &pass,
    )
    .await
    .unwrap();
    let test_cal = calendars
        .iter()
        .find(|c| c.displayname.contains("_test"))
        .expect("pimsteward_test calendar not found in iCloud account");
    let td = tempfile::tempdir().unwrap();
    assert_icloud_test_calendar(&test_cal.url, &test_cal.displayname, td.path());
}

#[tokio::test]
#[ignore]
async fn create_update_delete_event_roundtrip() {
    let Some((user, pass)) = skip_unless_opted_in() else {
        eprintln!("skip: set PIMSTEWARD_RUN_E2E_ICLOUD=1");
        return;
    };
    // Create a unique UID per run to avoid stale event collisions.
    let uid = format!("pimsteward-e2e-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let ical = format!("BEGIN:VCALENDAR\r\n...\r\nUID:{uid}\r\n...\r\nEND:VCALENDAR\r\n");
    // ... build source/writer, find _test calendar URL, run create/update/delete.
    // Cleanup at the end (always — even on test failure, use a panic-safe drop).
}
```

- [ ] **Step 2: Implement the tests with cleanup**

Each test that creates an event registers a cleanup closure (a small RAII guard struct) that issues a DELETE on drop. This means even a panicked test leaves the calendar tidy.

- [ ] **Step 3: Run locally to confirm**

```bash
PIMSTEWARD_RUN_E2E_ICLOUD=1 \
PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE=$HOME/.config/secrets/icloud-username \
PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE=$HOME/.config/secrets/icloud-app-password \
cargo nextest run --run-ignored all -- icloud_e2e
```

(Adjust paths to wherever the dotvault deploy lands the iCloud creds. **Do not** put real values in this plan.)

Expected: PASS — discovery, CRUD, and restore round-trips all green against the real `pimsteward_test` calendar.

- [ ] **Step 4: Confirm `.gitignore` defenses**

Run: `git check-ignore -v $HOME/.config/secrets/icloud-app-password`
(This file is outside the repo so it's already not tracked — but verify that nothing in the repo path could accidentally check in a copy by adding `**/icloud-username*`, `**/icloud-app-password*`, `**/icloud-test-creds*` to `.gitignore`.)

- [ ] **Step 5: Commit**

```bash
git add tests/icloud_e2e.rs .gitignore
git commit -m "tests: iCloud e2e suite against pimsteward_test calendar (opt-in)"
```

---

## Phase 4 — Documentation

### Task 9: README + banner + CONTRIBUTING + CLAUDE.md updates

**Goal:** Reframe the project as multi-provider in user-facing docs. Forwardemail stays documented as the primary, fully-supported target; iCloud appears as the partial-functionality example. Internal docs (CLAUDE.md) gain parallel verification steps.

**Files:**
- Modify: `README.md`
- Modify: `CONTRIBUTING.md`
- Modify: `CLAUDE.md`
- Modify: `assets/banner.svg`
- Modify: `Non-goals` section in README

**Acceptance Criteria:**
- [ ] README leads with provider-agnostic framing. Specifically: the first paragraph says pimsteward is a permission-aware MCP mediator + git backup for personal data, with forwardemail as the primary provider and standards-based providers (e.g. iCloud CalDAV) for partial functionality.
- [ ] A new "Supported providers" section appears near the top with a capability matrix (forwardemail full row, iCloud calendar-only row).
- [ ] The existing "Why forwardemail" section is renamed/scoped to make clear it's about that provider, not pimsteward as a whole.
- [ ] The "Non-goals" line `Not a multi-provider sync tool. v1 is forwardemail-only by design` is replaced with the new wording from the spec.
- [ ] A "Running multiple providers" section explains the one-daemon-per-provider model with an example pointing at `examples/config-icloud-caldav.toml`.
- [ ] Banner SVG subtitle updated to a provider-agnostic phrasing (no longer says "for forwardemail.net").
- [ ] CONTRIBUTING.md gains an "iCloud e2e setup" section (Apple ID, app-specific password, `_test` calendar creation, env vars, where to put credential files).
- [ ] CLAUDE.md's deploy-verification section makes clear it applies to whichever daemon was changed; the iCloud daemon's parallel verification command is documented.
- [ ] No secrets in any of these files. Apple ID examples use `apple.id@example.com`, paths use `/run/pimsteward-icloud-secrets/...` placeholders.

**Verify:**
- `git grep "forwardemail-only" README.md` → empty.
- `git grep "for forwardemail.net" assets/banner.svg` → empty.
- README renders cleanly via `markdownlint` or visual inspection.

**Steps:**

- [ ] **Step 1: README rewrites**

The pieces:

1. Replace the lead blockquote (currently "pimsteward is a PIM steward for forwardemail.net…") with a provider-agnostic version. Keep the tone — terse, opinionated, pull-no-punches.
2. Above the existing "Why forwardemail" section, insert a new section:

   ```markdown
   ## Supported providers

   pimsteward runs as a single daemon per provider. Each daemon owns its
   own credentials, its own git repo, and its own MCP endpoint.

   | Provider          | Mail | Calendar | Contacts | Sieve | Send |
   | ----------------- | :--: | :------: | :------: | :---: | :--: |
   | forwardemail.net  |  ✅  |   ✅     |   ✅     |  ✅   |  ✅  |
   | iCloud (CalDAV)   |  —   |   ✅     |   —      |  —    |  —   |

   Add as many as you have accounts. They don't share state — they're parallel
   daemons with parallel git repos.

   New providers are added when a contributor brings a real, testable account
   to the table. Fastmail/JMAP, Gmail, generic IMAP-only providers are out of
   scope until then.
   ```

3. Rename the "Why forwardemail.net" section to "Why forwardemail (the primary target)" and add a one-sentence preamble making clear this is about the primary provider, not about pimsteward as a whole.
4. Replace the "Not a multi-provider sync tool" non-goal line with the spec wording.
5. Add a new section "Running multiple providers" near the architecture diagram, pointing at `examples/config-icloud-caldav.toml` and explaining that each daemon = its own systemd unit, port, repo, bearer token, MCP entry.

- [ ] **Step 2: Banner SVG**

Open `assets/banner.svg`, find the `<text>` node with the forwardemail-specific subtitle, replace its content with a provider-agnostic phrasing — e.g. "permission-aware MCP mediator for your personal data". Verify the rendered SVG width still fits the layout. (If you're unsure of the rendered look, render with `inkscape` or open the SVG in a browser to eyeball it.)

- [ ] **Step 3: CONTRIBUTING.md — iCloud setup**

Add a section after the existing forwardemail e2e walkthrough:

```markdown
### iCloud CalDAV e2e setup

The iCloud e2e suite runs against your own iCloud account and a calendar
named `pimsteward_test` that you create in advance.

1. Create a calendar in the iCloud Calendar app named `pimsteward_test`.
   The substring `_test` is enforced by `safety::assert_icloud_test_calendar`.
2. At <https://appleid.apple.com>, generate an app-specific password.
3. Save your Apple ID email and the app-specific password into two
   files outside this repo (e.g. via dotvault):

   ```sh
   echo "you@example.com"          > ~/.config/secrets/icloud-username
   echo "app-specific-password"    > ~/.config/secrets/icloud-app-password
   chmod 600 ~/.config/secrets/icloud-*
   ```

   **Never check these files into git.** `.gitignore` includes defensive
   patterns; `git check-ignore` should report them as ignored even if
   you accidentally place them inside the repo.

4. Run the e2e suite:

   ```sh
   export PIMSTEWARD_RUN_E2E_ICLOUD=1
   export PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE=$HOME/.config/secrets/icloud-username
   export PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE=$HOME/.config/secrets/icloud-app-password

   cargo nextest run --run-ignored all -- icloud_e2e
   ```

   The suite cleans up after itself — events created during a run are
   deleted before the test exits.

CI does not have iCloud credentials and never runs this suite.
```

- [ ] **Step 4: CLAUDE.md — parallel verification**

In `CLAUDE.md`, the deploy-verification section currently targets the forwardemail daemon (rockycc → pimsteward-dan etc.). Add a sibling block for the iCloud daemon:

```markdown
### Verifying the iCloud daemon

The iCloud daemon (`pimsteward-icloud-...`) is a separate container with its
own port and bearer token. After deploying changes that affect iCloud-side
behaviour, verify it independently:

1. Run the verification script with the iCloud daemon's port and token:

   ```
   sudo machinectl shell --uid=1000 rockycc /bin/sh -c \
     "PIMSTEWARD_PORT=8102 PIMSTEWARD_TOKEN=$(cat /rockycc/.config/icloud-token) \
      bash /rockycc/scripts/verify-pimsteward.sh"
   ```
   (Or whichever exact port/token wiring the deployment uses — match the unit
   file. The host and bearer-token-file paths come from the iCloud daemon's
   systemd unit, not the forwardemail unit.)

2. Confirm `list_calendars` returns the iCloud calendar set, not the
   forwardemail one.

3. Restart Rocky if iCloud-side tool schemas changed (the same caching
   gotcha applies to the iCloud MCP entry as to the forwardemail one).
```

- [ ] **Step 5: Verify**

```bash
git grep "forwardemail-only" README.md
git grep "for forwardemail.net" assets/banner.svg
```

Both must return empty. Visually inspect the rendered README in a markdown viewer to confirm the matrix and section flow read well.

- [ ] **Step 6: Commit**

```bash
git add README.md CONTRIBUTING.md CLAUDE.md assets/banner.svg
git commit -m "docs: reframe pimsteward as multi-provider; document iCloud as second"
```

---

### Task 10: Example iCloud config

**Goal:** A complete, runnable example config file for the iCloud daemon. No secrets — placeholder paths only.

**Files:**
- Create: `examples/config-icloud-caldav.toml`

**Acceptance Criteria:**
- [ ] Loads cleanly with `pimsteward daemon --config examples/config-icloud-caldav.toml --port 8102` (in a smoke environment with the credential files present at the given paths).
- [ ] Has every config field commented with what it does and what its acceptable values are.
- [ ] `[permissions]` only sets `calendar`. Comments explicitly mention that `email`, `contacts`, `sieve`, `email_send` are rejected at load time.
- [ ] No real Apple ID, no real password, no real bearer token — placeholder paths only.

**Verify:** `cargo run -- daemon --config examples/config-icloud-caldav.toml --validate-only` (or whatever validate-config subcommand exists, or wrap in a unit test) → exits 0 once the placeholder paths are replaced with valid (even empty-but-existing) files. With invalid config it exits non-zero with a clear message.

**Steps:**

- [ ] **Step 1: Write the example**

```toml
# pimsteward — iCloud CalDAV daemon (calendars only).
#
# This config runs ONE pimsteward daemon dedicated to backing up and
# mediating an iCloud calendar account. It does NOT replace the
# forwardemail daemon — the two run as parallel processes with
# separate ports, bearer tokens, and git repos.
#
# Save credential files outside this repository. The .gitignore patterns
# defend against accidental check-in but the only safe path is to never
# put secrets under the repo root in the first place.

log_level = "info"

[provider.icloud_caldav]
discovery_url = "https://caldav.icloud.com/"
username_file = "/run/pimsteward-icloud-secrets/icloud-username"
password_file = "/run/pimsteward-icloud-secrets/icloud-app-password"
# iCloud rejects empty User-Agent headers with 403. Keep this set.
user_agent    = "pimsteward (iCloud CalDAV)"

[storage]
# Distinct from the forwardemail daemon's repo. Each daemon owns its own.
repo_path = "/var/lib/pimsteward-icloud"

[pull]
# iCloud doesn't publish a CalDAV rate limit. 5 minutes is conservative.
calendar_interval_seconds = 300
# email/contacts/sieve/mail intervals MUST NOT be set here — the iCloud
# provider does not support those resources and config-load will reject
# unknown intervals.

[permissions]
# Only `calendar` is supported. Setting `email`, `contacts`, `sieve`, or
# `email_send` here causes the daemon to refuse to start with a clear
# capability-mismatch error.
#
# Use the flat form for read or read_write across all iCloud calendars,
# or the scoped per-calendar-id form below to lock specific calendars
# down. iCloud calendar IDs are full URLs returned by discovery, e.g.
# `https://p07-caldav.icloud.com/<principal>/calendars/<uuid>/`.
calendar = "read"

# Example scoped form (commented):
# [permissions.calendar]
# default = "read"
# [permissions.calendar.by_id]
# "https://p07-caldav.icloud.com/123/calendars/work/" = "read_write"
```

- [ ] **Step 2: Verify**

If `pimsteward` has a config-validate subcommand, run it. If not, write a unit test that loads the example and exercises `Config::active_provider_kind()`:

```rust
#[test]
fn example_icloud_config_parses() {
    let cfg = Config::load(std::path::Path::new("examples/config-icloud-caldav.toml")).unwrap();
    assert_eq!(
        cfg.active_provider_kind().unwrap(),
        crate::config::ProviderKind::IcloudCaldav
    );
    assert_eq!(cfg.permissions.calendar.default_access(), Access::Read);
}
```

- [ ] **Step 3: Commit**

```bash
git add examples/config-icloud-caldav.toml src/config.rs
git commit -m "examples: iCloud CalDAV daemon config sample"
```

---

## Self-Review

Done while writing. Findings:

1. **Spec coverage:** All goals 1–7 from the spec map onto Tasks 1–10. Open-questions/risks (rate limits, password rotation, principal URL drift, read-only system calendars, restore safety) all have implementation hooks: discovery cache invalidation on non-2xx (Task 5), structured 412 errors (Task 5), capability/permission validation (Task 3), `_test` safety guard (Task 7).
2. **Placeholder scan:** Two `todo!()` markers appear in code samples in Tasks 4 and 5 explicitly marked "replace before commit". Acceptable in plan steps because the surrounding instructions name what to put there ("port the REPORT logic from `src/source/dav.rs`").
3. **Type consistency:** `Capabilities`, `Provider`, `Resource`, `Error` types are reused consistently across tasks. `IcloudCalendarSource::new` signature matches what `IcloudCaldavProvider::build_calendar_source` calls.
4. **Verification scan:** Spec marked NO — no human-in-the-loop validation requirement. No verification tasks needed.

---

## Out of scope (not in this plan)

- iCloud CardDAV (contacts).
- iCloud `@icloud.com` mail.
- JMAP / Fastmail / Gmail.
- Generic CalDAV against arbitrary servers.
- Cross-provider operations (search, restore-across-providers).
- Migrating the existing `[forwardemail]` config block to namespaced form for production deployments — backwards-compat keeps the legacy form working indefinitely.
