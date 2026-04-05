//! Write orchestration: execute a forwardemail mutation and record the
//! outcome as an attributed git commit.
//!
//! Every write follows the same three-step dance:
//!
//! 1. **Stage** — record the caller's intent on disk (so if the process
//!    dies mid-call, the audit trail still has "we were about to do X").
//! 2. **Execute** — hit the forwardemail API.
//! 3. **Record** — re-pull the affected resource and commit the new state
//!    with `author = <caller>`, message = structured YAML describing the
//!    tool call, parameters, and the caller's stated reason.
//!
//! For v1 the stage step is folded into record (we don't fsync a pending
//! marker; if the process dies mid-call the pull loop will reconcile on
//! next startup). A durable WAL is a phase-later hardening.

pub mod audit;
pub mod contacts;
pub mod mail;
pub mod sieve;

pub use audit::{Attribution, WriteAudit};
