//! pimsteward — permission-aware MCP mediator for forwardemail.net with
//! time-travel backup built in. See PLAN.md for architecture.

pub mod config;
pub mod daemon;
pub mod error;
pub mod forwardemail;
pub mod mcp;
pub mod permission;
pub mod pull;
pub mod restore;
pub mod safety;
pub mod source;
pub mod store;
pub mod write;

pub use config::Config;
pub use error::Error;
pub use permission::{Access, Permissions, Resource};

/// Daemon entry point — long-running mode with periodic pull timers.
///
/// MCP is intentionally NOT part of the daemon. AI clients spawn the
/// `pimsteward mcp` subcommand as a child process with stdio transport,
/// matching the forwardemail MCP server pattern.
pub async fn run(cfg: Config) -> Result<(), Error> {
    daemon::run(cfg).await
}
