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
use pimsteward::{
    config::MailSourceKind,
    forwardemail::Client,
    mcp::PimstewardServer,
    pull,
    source::{imap::ImapConfig, ImapMailSource, MailSource, RestMailSource},
    store::Repo,
    Config,
};
use rmcp::{transport::stdio, ServiceExt};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Build the configured MailSource for this run. REST is the default;
/// IMAP is selected when `forwardemail.mail_source = "imap"` in the
/// config, in which case the IMAP host/port come from config too.
fn build_mail_source(
    cfg: &Config,
    client: Client,
    user: &str,
    password: &str,
) -> Arc<dyn MailSource> {
    match cfg.forwardemail.mail_source {
        MailSourceKind::Rest => Arc::new(RestMailSource::new(client)),
        MailSourceKind::Imap => {
            let imap_cfg = ImapConfig {
                host: cfg.forwardemail.imap_host.clone(),
                port: cfg.forwardemail.imap_port,
                user: user.to_string(),
                password: password.to_string(),
            };
            Arc::new(ImapMailSource::new(imap_cfg))
        }
    }
}

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

    /// Run one pull cycle for mail (folders + messages) and commit.
    PullMail,

    /// Run one pull cycle for calendars and calendar events and commit.
    PullCalendar,

    /// Run all pull cycles in sequence.
    PullAll,

    /// Run the MCP server over stdio — exposes read tools to an AI assistant.
    /// Pipe its stdin/stdout to any MCP-compatible client (Claude Desktop,
    /// Cursor, etc.) via their server configuration.
    Mcp,

    /// Long-running daemon mode (pull timers + MCP server). NOT YET IMPLEMENTED.
    Daemon,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    // Install rustls's `ring` crypto provider once, process-wide. reqwest's
    // rustls-tls feature manages its own crypto internally; our direct
    // rustls usage (for the IMAP source's TLS connection) needs an
    // explicit default provider. Ignoring the Err because it only fires
    // if a provider was already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Always write logs to stderr so stdout is clean for JSON-RPC (MCP) or
    // JSON output from pull-* subcommands. A real MCP client parses stdout
    // strictly and would choke on log lines mixed in.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(
            fmt::layer()
                .with_target(true)
                .with_line_number(true)
                .with_writer(std::io::stderr),
        )
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
        Command::PullMail => {
            cfg.permissions.check_read(pimsteward::Resource::Email)?;
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(
                cfg.forwardemail.api_base.clone(),
                user.clone(),
                pass.clone(),
            )?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let source = build_mail_source(&cfg, client, &user, &pass);
            let summary = pull::mail::pull_mail(
                source.as_ref(),
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, source = source.tag(), "pull-mail done");
            println!("{summary}");
        }
        Command::PullCalendar => {
            cfg.permissions.check_read(pimsteward::Resource::Calendar)?;
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(cfg.forwardemail.api_base.clone(), user, pass)?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let summary = pull::calendar::pull_calendar(
                &client,
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, "pull-calendar done");
            println!("{summary}");
        }
        Command::PullAll => {
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(
                cfg.forwardemail.api_base.clone(),
                user.clone(),
                pass.clone(),
            )?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let author = ("pimsteward-pull", "pull@pimsteward.local");

            // Each is independently gated; skip the ones the config denies.
            if cfg
                .permissions
                .check_read(pimsteward::Resource::Contacts)
                .is_ok()
            {
                let s = pull::contacts::pull_contacts(&client, &repo, &alias, author.0, author.1)
                    .await?;
                tracing::info!(summary = %s, "pull-contacts done");
                println!("{s}");
            }
            if cfg
                .permissions
                .check_read(pimsteward::Resource::Sieve)
                .is_ok()
            {
                let s = pull::sieve::pull_sieve(&client, &repo, &alias, author.0, author.1).await?;
                tracing::info!(summary = %s, "pull-sieve done");
                println!("{s}");
            }
            if cfg
                .permissions
                .check_read(pimsteward::Resource::Calendar)
                .is_ok()
            {
                let s = pull::calendar::pull_calendar(&client, &repo, &alias, author.0, author.1)
                    .await?;
                tracing::info!(summary = %s, "pull-calendar done");
                println!("{s}");
            }
            if cfg
                .permissions
                .check_read(pimsteward::Resource::Email)
                .is_ok()
            {
                let mail_source = build_mail_source(&cfg, client.clone(), &user, &pass);
                let s =
                    pull::mail::pull_mail(mail_source.as_ref(), &repo, &alias, author.0, author.1)
                        .await?;
                tracing::info!(summary = %s, source = mail_source.tag(), "pull-mail done");
                println!("{s}");
            }
        }
        Command::Mcp => {
            let (user, pass) = cfg.load_credentials()?;
            let alias = alias_from_user(&user);
            let client = Client::new(cfg.forwardemail.api_base.clone(), user, pass)?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let server =
                PimstewardServer::new(client, repo, cfg.permissions.clone(), alias.clone());
            tracing::info!(alias = %alias, "mcp server starting over stdio");
            let running = server.serve(stdio()).await?;
            running.waiting().await?;
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
