//! pimsteward — permission-aware MCP mediator for forwardemail.net with
//! time-travel backup built in. See PLAN.md for architecture.

pub mod config;
pub mod error;
pub mod forwardemail;
pub mod mcp;
pub mod permission;
pub mod pull;
pub mod store;

pub use config::Config;
pub use error::Error;
pub use permission::{Access, Permissions, Resource};

/// Daemon entry point — long-running mode with pull timers + MCP listener.
///
/// NOT IMPLEMENTED YET. v1 ships `probe`, `pull-contacts`, and `pull-sieve`
/// as standalone subcommands. The daemon form lands in a later phase.
pub async fn run(_cfg: Config) -> Result<(), Error> {
    Err(Error::NotImplemented(
        "daemon mode — use `probe` / `pull-contacts` / `pull-sieve` subcommands",
    ))
}
