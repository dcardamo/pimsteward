//! pimsteward — permission-aware MCP mediator for forwardemail.net with
//! time-travel backup built in. See PLAN.md for architecture.

pub mod config;
pub mod daemon;
pub mod error;
pub mod forwardemail;
pub mod index;
pub mod mcp;
pub mod permission;
pub mod provider;
pub mod pull;
pub mod restore;
pub mod safety;
pub mod source;
pub mod store;
pub mod write;

pub use config::Config;
pub use daemon::HttpOptions;
pub use error::Error;
pub use permission::{Access, Permissions, Resource};

/// Daemon entry point — long-running mode with periodic pull timers and
/// an optional MCP HTTP server for AI clients.
pub async fn run(cfg: Config, http: Option<HttpOptions>) -> Result<(), Error> {
    daemon::run(cfg, http).await
}
