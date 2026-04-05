//! Long-running daemon: spawns one tokio task per resource with its own
//! pull interval. Errors are logged and the loop keeps running — a flaky
//! network shouldn't kill the whole daemon.
//!
//! MCP is intentionally NOT part of the daemon. AI clients (Claude Desktop,
//! rockycc, etc.) spawn `pimsteward mcp` as a child process with stdio
//! transport, matching the forwardemail MCP server pattern. Decoupling
//! means the daemon can run as a low-privilege user doing nothing but
//! pulling, and each MCP client gets its own isolated process on demand.

use crate::config::{Config, MailSourceKind};
use crate::error::Error;
use crate::forwardemail::Client;
use crate::permission::Resource;
use crate::pull;
use crate::source::{imap::ImapConfig, ImapMailSource, MailSource, RestMailSource};
use crate::store::Repo;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::Instrument;

/// Run the pull daemon indefinitely. Returns when a fatal error occurs or
/// when a shutdown signal is received.
pub async fn run(cfg: Config) -> Result<(), Error> {
    let (user, pass) = cfg.load_credentials()?;
    let alias = user.replace('@', "-");
    let client = Client::new(cfg.forwardemail.api_base.clone(), user, pass)?;
    let repo = Arc::new(Repo::open_or_init(&cfg.storage.repo_path)?);
    let cfg = Arc::new(cfg);

    tracing::info!(alias = %alias, repo = ?cfg.storage.repo_path, "pimsteward daemon starting");

    let mut handles = Vec::new();

    if cfg.permissions.check_read(Resource::Contacts).is_ok() {
        let h = spawn_puller(
            "contacts",
            Duration::from_secs(cfg.pull.contacts_interval_seconds),
            client.clone(),
            repo.clone(),
            alias.clone(),
            |c, r, a| {
                Box::pin(async move {
                    pull::contacts::pull_contacts(
                        &c,
                        &r,
                        &a,
                        "pimsteward-pull",
                        "pull@pimsteward.local",
                    )
                    .await
                    .map(|s| s.to_string())
                })
            },
        );
        handles.push(h);
    }

    if cfg.permissions.check_read(Resource::Sieve).is_ok() {
        let h = spawn_puller(
            "sieve",
            Duration::from_secs(cfg.pull.sieve_interval_seconds),
            client.clone(),
            repo.clone(),
            alias.clone(),
            |c, r, a| {
                Box::pin(async move {
                    pull::sieve::pull_sieve(&c, &r, &a, "pimsteward-pull", "pull@pimsteward.local")
                        .await
                        .map(|s| s.to_string())
                })
            },
        );
        handles.push(h);
    }

    if cfg.permissions.check_read(Resource::Calendar).is_ok() {
        let h = spawn_puller(
            "calendar",
            Duration::from_secs(cfg.pull.calendar_interval_seconds),
            client.clone(),
            repo.clone(),
            alias.clone(),
            |c, r, a| {
                Box::pin(async move {
                    pull::calendar::pull_calendar(
                        &c,
                        &r,
                        &a,
                        "pimsteward-pull",
                        "pull@pimsteward.local",
                    )
                    .await
                    .map(|s| s.to_string())
                })
            },
        );
        handles.push(h);
    }

    if cfg.permissions.check_read(Resource::Email).is_ok() {
        // Mail has a dedicated puller because it dispatches on a
        // MailSource trait object, not a concrete Client.
        let mail_source: Arc<dyn MailSource> = match cfg.forwardemail.mail_source {
            MailSourceKind::Rest => Arc::new(RestMailSource::new(client.clone())),
            MailSourceKind::Imap => {
                let (u, p) = cfg.load_credentials()?;
                Arc::new(ImapMailSource::new(ImapConfig {
                    host: cfg.forwardemail.imap_host.clone(),
                    port: cfg.forwardemail.imap_port,
                    user: u,
                    password: p,
                }))
            }
        };
        let h = spawn_mail_puller(
            Duration::from_secs(cfg.pull.mail_interval_seconds),
            mail_source,
            repo.clone(),
            alias.clone(),
        );
        handles.push(h);
    }

    if handles.is_empty() {
        return Err(Error::config(
            "daemon has nothing to do — config.permissions grants no resource read access",
        ));
    }

    // Wait for a shutdown signal (SIGINT/SIGTERM on unix). Whichever comes
    // first, log it and return. The tokio task for each puller is detached;
    // they'll be cancelled when the main task returns and tokio shuts down.
    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, daemon exiting");
    Ok(())
}

fn spawn_mail_puller(
    period: Duration,
    source: Arc<dyn MailSource>,
    repo: Arc<Repo>,
    alias: String,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("puller", resource = "mail");
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(
                period_secs = period.as_secs(),
                source = source.tag(),
                "mail puller started"
            );
            loop {
                ticker.tick().await;
                let result = pull::mail::pull_mail(
                    source.as_ref(),
                    &repo,
                    &alias,
                    "pimsteward-pull",
                    "pull@pimsteward.local",
                )
                .await;
                match result {
                    Ok(s) => tracing::info!(summary = %s, "pull ok"),
                    Err(e) => tracing::error!(error = %e, "pull failed"),
                }
            }
        }
        .instrument(span),
    )
}

type PullFn = Box<
    dyn Fn(
            Client,
            Arc<Repo>,
            String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>
        + Send
        + Sync,
>;

fn spawn_puller(
    name: &'static str,
    period: Duration,
    client: Client,
    repo: Arc<Repo>,
    alias: String,
    f: impl Fn(
            Client,
            Arc<Repo>,
            String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>
        + Send
        + Sync
        + 'static,
) -> tokio::task::JoinHandle<()> {
    let f: PullFn = Box::new(f);
    let span = tracing::info_span!("puller", resource = name);
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(period_secs = period.as_secs(), "puller started");
            loop {
                ticker.tick().await;
                match (f)(client.clone(), repo.clone(), alias.clone()).await {
                    Ok(summary) => tracing::info!(%summary, "pull ok"),
                    Err(e) => tracing::error!(error = %e, "pull failed"),
                }
            }
        }
        .instrument(span),
    )
}

use std::future::Future;

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        _ = sigint.recv() => tracing::info!("SIGINT received"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
