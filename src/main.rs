//! pimsteward binary entry point.
//!
//! v1 subcommands (what ships in this first shipment):
//!
//! - `probe` — hit `GET /v1/account` to verify auth + network
//! - `pull-contacts` — run one pull cycle for contacts
//! - `pull-sieve` — run one pull cycle for sieve scripts
//!
//! Everything else (pull-mail, pull-calendar, daemon mode, MCP server,
//! write tools, restore) lands in later phases — the scaffolding is in
//! place but those code paths return `NotImplemented`.

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use pimsteward::{forwardemail::Client, pull, store::Repo, Config};
use std::path::PathBuf;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Debug, Parser)]
#[command(
    name = "pimsteward",
    version,
    about = "Permission-aware MCP mediator for forwardemail.net, with time-travel backup."
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(
        long,
        env = "PIMSTEWARD_CONFIG",
        default_value = "/etc/pimsteward/config.toml"
    )]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Hit GET /v1/account to verify that credentials and connectivity work.
    Probe,

    /// Run one pull cycle for contacts and commit.
    PullContacts,

    /// Run one pull cycle for sieve scripts and commit.
    PullSieve,

    /// Long-running daemon mode (pull timers + MCP server). NOT YET IMPLEMENTED.
    Daemon,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(true).with_line_number(true))
        .init();

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;

    match cli.command {
        Command::Probe => {
            let (user, pass) = cfg.load_credentials()?;
            let client = Client::new(cfg.forwardemail.api_base, user, pass)?;
            let info = client.account().await?;
            let remaining = client.rate_limit_remaining();
            tracing::info!(
                email = info.get("email").and_then(|v| v.as_str()).unwrap_or("?"),
                storage_used = info.get("storage_used").and_then(|v| v.as_u64()),
                rate_limit_remaining = remaining,
                "probe ok"
            );
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::PullContacts => {
            cfg.permissions.check_read(pimsteward::Resource::Contacts)?;
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(cfg.forwardemail.api_base.clone(), user, pass)?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let summary = pull::contacts::pull_contacts(
                &client,
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, "pull-contacts done");
            println!("{summary}");
        }
        Command::PullSieve => {
            cfg.permissions.check_read(pimsteward::Resource::Sieve)?;
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(cfg.forwardemail.api_base.clone(), user, pass)?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let summary = pull::sieve::pull_sieve(
                &client,
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, "pull-sieve done");
            println!("{summary}");
        }
        Command::Daemon => {
            pimsteward::run(cfg).await?;
        }
    }
    Ok(())
}

/// Convert an alias email into a filesystem-safe directory name.
/// `dan@hld.ca` → `dan-hld.ca`.
fn alias_from_user(user: &str) -> String {
    user.replace('@', "-")
}
