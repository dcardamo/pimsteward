//! pimsteward binary entry point.
//!
//! Subcommands:
//!
//! - `probe` — hit `GET /v1/account` to verify auth + network
//! - `pull-contacts` — run one pull cycle for contacts
//! - `pull-sieve` — run one pull cycle for sieve scripts
//! - `pull-mail` — run one pull cycle for mail
//! - `pull-calendar` — run one pull cycle for calendars
//! - `pull-all` — run all pull cycles in sequence
//! - `daemon` — long-running service: pull timers + MCP HTTP server

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use pimsteward::{
    config::{CalendarSourceKind, ContactsSourceKind, MailSourceKind},
    forwardemail::Client,
    pull,
    source::{
        imap::ImapConfig, CalendarSource, ContactsSource, DavCalendarSource, DavContactsSource,
        ImapMailSource, MailSource, MailWriter, RestCalendarSource, RestContactsSource,
        RestMailSource,
    },
    store::Repo,
    Config, HttpOptions,
};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Build the configured MailSource for this run. REST is the default;
/// IMAP is selected when `forwardemail.mail_source = "imap"` in the config.
fn build_mail_source(
    cfg: &Config,
    client: Client,
    user: &str,
    password: &str,
) -> (Arc<dyn MailSource>, Arc<dyn MailWriter>) {
    match cfg.forwardemail.mail_source {
        MailSourceKind::Rest => {
            let rest = Arc::new(RestMailSource::new(client));
            (rest.clone(), rest)
        }
        MailSourceKind::Imap => {
            let imap_cfg = ImapConfig {
                host: cfg.forwardemail.imap_host.clone(),
                port: cfg.forwardemail.imap_port,
                user: user.to_string(),
                password: password.to_string(),
            };
            let imap = Arc::new(ImapMailSource::new(imap_cfg));
            (imap.clone(), imap)
        }
    }
}

fn build_calendar_source(
    cfg: &Config,
    client: Client,
    user: &str,
    password: &str,
) -> Result<Arc<dyn CalendarSource>, pimsteward::Error> {
    Ok(match cfg.forwardemail.calendar_source {
        CalendarSourceKind::Rest => Arc::new(RestCalendarSource::new(client)),
        CalendarSourceKind::Caldav => Arc::new(DavCalendarSource::new(
            cfg.forwardemail.caldav_base_url.clone(),
            user,
            password,
        )?),
    })
}

fn build_contacts_source(
    cfg: &Config,
    client: Client,
    user: &str,
    password: &str,
) -> Result<Arc<dyn ContactsSource>, pimsteward::Error> {
    Ok(match cfg.forwardemail.contacts_source {
        ContactsSourceKind::Rest => Arc::new(RestContactsSource::new(client)),
        ContactsSourceKind::Carddav => Arc::new(DavContactsSource::new(
            cfg.forwardemail.carddav_base_url.clone(),
            user,
            password,
        )?),
    })
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

    /// Long-running service: pull timers for every resource granted at
    /// least `read` in config, weekly `git gc --auto`, and (when --port
    /// is set) an MCP HTTP server for AI clients. This is the only way
    /// AI accesses pimsteward — all interaction goes through the MCP
    /// HTTP endpoint.
    Daemon {
        /// Address to bind the MCP HTTP server to.
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Port for the MCP HTTP server. When set, the daemon serves the
        /// MCP protocol over Streamable HTTP / SSE at `/mcp`.
        #[arg(long)]
        port: Option<u16>,

        /// Path to a file containing a bearer token for authentication.
        /// If set, every MCP HTTP request must include
        /// `Authorization: Bearer <token>`. Recommended for any
        /// network-accessible deployment.
        #[arg(long)]
        bearer_token_file: Option<PathBuf>,
    },

    /// Manage the local search index (mail) backing search_email.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
}

#[derive(Debug, Subcommand)]
enum IndexAction {
    /// Rebuild the index from disk. Incremental by default (resumable;
    /// safe to re-run as often as you like). Orphan rows are swept at
    /// the end of each run.
    Rebuild {
        /// Drop all rows first and reindex from scratch. Use for schema
        /// migrations or suspected index corruption.
        #[arg(long)]
        force: bool,
    },
    /// Print JSON stats about the index (row count, size, date range).
    Stat,
    /// Compare index to disk; report orphan rows and unindexed `.eml`
    /// files.  Exits 0 when both sides match, 1 otherwise (cron-friendly).
    Verify {
        /// After the dry-run report, delete orphan rows and upsert
        /// unindexed .eml files.
        #[arg(long)]
        clean: bool,
    },
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
            let client = Client::new(
                cfg.forwardemail.api_base.clone(),
                user.clone(),
                pass.clone(),
            )?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let source = build_contacts_source(&cfg, client, &user, &pass)?;
            let summary = pull::contacts::pull_contacts(
                source.as_ref(),
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, source = source.tag(), "pull-contacts done");
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
            let (source, _writer) = build_mail_source(&cfg, client, &user, &pass);
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
            let client = Client::new(
                cfg.forwardemail.api_base.clone(),
                user.clone(),
                pass.clone(),
            )?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let source = build_calendar_source(&cfg, client, &user, &pass)?;
            let summary = pull::calendar::pull_calendar(
                source.as_ref(),
                &repo,
                &alias,
                "pimsteward-pull",
                "pull@pimsteward.local",
            )
            .await?;
            tracing::info!(summary = %summary, source = source.tag(), "pull-calendar done");
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
                let source = build_contacts_source(&cfg, client.clone(), &user, &pass)?;
                let s = pull::contacts::pull_contacts(
                    source.as_ref(),
                    &repo,
                    &alias,
                    author.0,
                    author.1,
                )
                .await?;
                tracing::info!(summary = %s, source = source.tag(), "pull-contacts done");
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
                let source = build_calendar_source(&cfg, client.clone(), &user, &pass)?;
                let s = pull::calendar::pull_calendar(
                    source.as_ref(),
                    &repo,
                    &alias,
                    author.0,
                    author.1,
                )
                .await?;
                tracing::info!(summary = %s, source = source.tag(), "pull-calendar done");
                println!("{s}");
            }
            if cfg
                .permissions
                .check_read(pimsteward::Resource::Email)
                .is_ok()
            {
                let (mail_source, _writer) = build_mail_source(&cfg, client.clone(), &user, &pass);
                let s =
                    pull::mail::pull_mail(mail_source.as_ref(), &repo, &alias, author.0, author.1)
                        .await?;
                tracing::info!(summary = %s, source = mail_source.tag(), "pull-mail done");
                println!("{s}");
            }
        }
        Command::Daemon {
            host,
            port,
            bearer_token_file,
        } => {
            let http = port.map(|p| HttpOptions {
                host,
                port: p,
                bearer_token_file,
            });
            pimsteward::run(cfg, http).await?;
        }
        Command::Index { action } => {
            // Email is the only indexed resource today, so gate on the
            // email read permission.  Rebuild and verify both touch the
            // on-disk mail tree; stat is derivable from the DB alone
            // but keeping them gated consistently is simpler.
            cfg.permissions.check_read(pimsteward::Resource::Email)?;
            let repo = Repo::open_or_init(&cfg.storage.repo_path)?;
            let index = pimsteward::index::SearchIndex::open(repo.root())?;
            match action {
                IndexAction::Rebuild { force } => {
                    let opts = if force {
                        pimsteward::index::RebuildOpts::force()
                    } else {
                        pimsteward::index::RebuildOpts::incremental()
                    };
                    let stats = index.rebuild_from_disk(repo.root(), opts)?;
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "scanned": stats.scanned,
                        "upserted": stats.upserted,
                        "skipped": stats.skipped,
                        "orphaned_deleted": stats.orphaned_deleted,
                        "errors": stats.errors,
                        "elapsed_ms": stats.elapsed_ms,
                        "force": force,
                    }))?);
                }
                IndexAction::Stat => {
                    let s = index.stats()?;
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "db_path": s.db_path,
                        "db_size_bytes": s.db_size_bytes,
                        "schema_version": s.schema_version,
                        "messages": s.messages,
                        "folders": s.folders,
                        "oldest_date_unix": s.oldest_date_unix,
                        "newest_date_unix": s.newest_date_unix,
                        "last_indexed_at": s.last_indexed_at,
                    }))?);
                }
                IndexAction::Verify { clean } => {
                    let report = index.verify(repo.root(), clean)?;
                    let is_clean = report.is_clean();
                    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                        "orphan_rows": report.orphan_rows,
                        "unindexed_emls": report.unindexed_emls
                            .iter()
                            .map(|(id, folder)| serde_json::json!({
                                "canonical_id": id,
                                "folder_safe": folder,
                            }))
                            .collect::<Vec<_>>(),
                        "cleaned": report.cleaned,
                        "clean": is_clean,
                    }))?);
                    if !is_clean && !clean {
                        std::process::exit(1);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Convert an alias email into a filesystem-safe directory name.
/// `dan@hld.ca` → `dan-hld.ca`.
fn alias_from_user(user: &str) -> String {
    user.replace('@', "-")
}
