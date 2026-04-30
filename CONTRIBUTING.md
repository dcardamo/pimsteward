# Contributing to pimsteward

Welcome. This document covers the mechanics: build, test, style, and how
to get a change reviewed. For the architectural "why" of pimsteward, read
[DESIGN.md](DESIGN.md) first. For the implementation history and phased
build plan, [PLAN.md](PLAN.md).

## Setup

pimsteward is a Rust crate. Any recent stable toolchain (≥ 1.80) works.

```sh
git clone <repo-url> pimsteward
cd pimsteward
cargo build --release
```

On NixOS or with Nix installed:

```sh
nix-shell ~/git/dotfiles/shells/shell-rust.nix
cargo build --release
```

The Nix devshell (see [dotfiles/shells/shell-rust.nix][nixshell]) bundles
`cargo-nextest`, `rust-analyzer`, `bacon`, `cargo-watch`, and the native
dependencies (`pkg-config`, `openssl`, `sqlite`). Not required — plain
cargo works fine — but it's how the author tests changes locally.

[nixshell]: https://git.purpose.dev/dan/dotfiles/src/branch/main/shells/shell-rust.nix

## Tests

pimsteward has four tiers. Respect the boundaries: a test in one tier
doesn't silently become a test in another.

| Tier          | Location                    | Mocks         | Network | Runs by default |
| ------------- | --------------------------- | ------------- | ------- | --------------- |
| unit          | `src/**/*.rs` `#[cfg(test)]`| allowed       | no      | yes             |
| integration   | `tests/integration_test.rs` | wiremock only | no      | yes             |
| e2e safety    | `tests/e2e_safety.rs`       | none          | no      | yes             |
| e2e live      | `tests/e2e_*.rs`            | none          | yes     | **no** — gated  |

### Running tests

```sh
# Everything network-free: 52 tests, < 5s
cargo nextest run

# Including the 10 live e2e tests against a real forwardemail test alias
PIMSTEWARD_RUN_E2E=1 cargo nextest run --run-ignored all

# Just one test tier
cargo nextest run --test e2e_safety
cargo nextest run --test integration_test
```

### e2e live tests — safety guardrail

Live e2e tests hit a real forwardemail API. There is a mandatory safety
guardrail (`src/safety.rs`) that **panics** unless:

1. The alias email contains the substring `_test` (case-insensitive).
2. The alias is not on the explicit production deny list.
3. The git repo path used for the test is not under a known production
   directory (`/data/Backups/...`, `/var/lib/pimsteward`).

**This check cannot be bypassed.** Every e2e test constructs its client
through `tests/common/mod.rs::E2eContext::from_env`, which calls both
guards before returning anything. Don't add an e2e test that constructs
a `Client` directly.

To run live tests, point the env vars at a disposable test alias:

```sh
export PIMSTEWARD_RUN_E2E=1
export PIMSTEWARD_TEST_ALIAS_USER_FILE=/path/to/test-alias-email
export PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE=/path/to/test-alias-password
cargo nextest run --run-ignored all
```

The alias email must contain `_test`. For example,
`my_test@example.com` is accepted; `me@example.com` is not.

### iCloud CalDAV e2e setup

The iCloud provider has its own opt-in e2e suite, parallel to the
forwardemail one above but pointed at a real iCloud account over CalDAV.
It exercises calendar create/read/update/delete against Apple's servers
and is gated behind a separate env var so it never runs accidentally
alongside the forwardemail suite.

The safety guard for these tests lives in
[`src/safety.rs`](src/safety.rs) as `assert_icloud_test_calendar` and
enforces that the target calendar's displayname contains the substring
`_test`. Just like the forwardemail guard, it **panics** if the rule is
violated — there is no way to silently bypass it.

**One-time setup:**

1. In the iCloud Calendar app (macOS or iCloud.com), create a new
   calendar named `pimsteward_test`. The substring `_test` is what the
   safety guard checks; the exact name is convention.
2. At [appleid.apple.com](https://appleid.apple.com/) → *Sign-In and
   Security → App-Specific Passwords*, generate a password labelled e.g.
   `pimsteward-test`. Apple will only show the password once — copy it
   immediately.
3. Save your Apple ID email and the app-specific password into two files
   **outside the repo**, ideally via dotvault or a similar secret store.
   Example placeholder paths:

   ```sh
   ~/.config/secrets/icloud-username       # contains: apple.id@example.com
   ~/.config/secrets/icloud-app-password   # contains: the 16-char app password
   ```

   Never commit either file. Never paste either value into a config
   file checked into git.

**Running the suite:**

```sh
export PIMSTEWARD_RUN_E2E_ICLOUD=1
export PIMSTEWARD_TEST_ICLOUD_USERNAME_FILE=$HOME/.config/secrets/icloud-username
export PIMSTEWARD_TEST_ICLOUD_PASSWORD_FILE=$HOME/.config/secrets/icloud-app-password
cargo nextest run --run-ignored all -- icloud_e2e
```

Each test creates events with a unique per-run UID and cleans up after
itself (including on partial failure), so collisions between parallel
runs are not a concern. CI does not have iCloud credentials and never
runs this suite — it's strictly a local-developer workflow.

**Optional defense in depth:** set
`PIMSTEWARD_TEST_ICLOUD_CALENDAR_URL_ALLOW` to a comma-separated list of
calendar URLs the suite is allowed to touch. If set, the safety guard
will refuse to operate on any calendar URL not in the list, on top of
the displayname `_test` check. This is useful if you have several
test calendars and want to pin the suite to one of them.

### Writing new tests

- **Unit tests** for pure functions: canonicalisation helpers, permission
  matrix, config parsers, git path logic. Mocks allowed but must stay
  honest — if deleting the real implementation would still let the test
  pass, the test is broken.
- **Integration tests** for anything that touches the HTTP layer:
  `wiremock::MockServer` for forwardemail, real temp git repos via
  `tempfile::tempdir()`. No in-process mocks of our own modules.
- **e2e live tests** for full-lifecycle verification: create real
  resources, assert they land in both forwardemail and git, mutate,
  restore, clean up. Each test must use a unique per-process marker in
  resource names to avoid collisions between parallel runs.

Every e2e test must clean up after itself, even on partial failure. Use
`let _ = ... .await` for cleanup calls so a cleanup failure doesn't mask
the real test failure.

## Code style

### Required for every commit

```sh
cargo fmt --check
cargo clippy --all-targets -- --deny warnings
cargo nextest run
```

All three must pass. CI (when it exists) will enforce these. clippy is
set to `-D warnings`; fix the lint or explicitly `#[allow(...)]` with a
comment explaining why.

### Conventions

- **Errors:** `Error::config(...)` for config problems, `Error::store(...)`
  for git/fs, `Error::Api { status, message }` for HTTP responses from
  forwardemail, `Error::Io` via `#[from]`, `Error::Json` via `#[from]`.
  Never swallow an error with `let _ = ...` except in cleanup paths.
- **Logging:** `tracing::info!` for normal events, `tracing::debug!` for
  details, `tracing::warn!` for recoverable issues, `tracing::error!` for
  failures that stop useful work. Never log credential values — the
  `Error::Display` impls are safe, anything you wrap manually is not.
- **Public API surface:** keep it small. Prefer `pub(crate)` unless
  something is genuinely needed by a consumer (the binary, tests, or
  another library). Every `pub` item is a commitment to stability.
- **Comments:** explain *why*, not *what*. If the code is boring, no
  comment is needed. If the code is surprising, say why it has to be
  that way. Reference PLAN.md or docs/api-findings.md when the choice
  was driven by an external constraint.

## Submitting changes

1. Branch from `main`.
2. Make your change. Keep commits logically independent — one feature or
   fix per commit, with a descriptive message body explaining the why.
3. Run the full check: `cargo fmt --check && cargo clippy --all-targets -- --deny warnings && cargo nextest run`.
4. If your change touches write paths, restore flows, or MCP tool
   surface, add an e2e test that exercises it.
5. Push and open a PR (or a patchset via email — either is fine).

Commit messages follow the usual format: imperative subject line under
72 characters, blank line, body explaining the motivation. No
Co-Authored-By trailers. No Signed-off-by unless you have a specific
reason.

## Reporting bugs

File an issue with:

- What you expected to happen
- What actually happened
- Steps to reproduce (minimal config, which subcommand, which MCP tool
  call if relevant)
- Relevant excerpts from the daemon's `tracing` output (ideally with
  `RUST_LOG=info,pimsteward=debug`)

If the bug involves data corruption or loss, **include the git log of
the affected subtree**. pimsteward's whole design premise is that git
is your audit trail; any bug report about data behaviour is much easier
to diagnose with the history attached.

## License

MIT. By submitting a change you agree to license it under the same
terms as the rest of the project.
