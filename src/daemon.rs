//! Long-running daemon: spawns one tokio task per resource with its own
//! pull interval. Errors are logged and the loop keeps running — a flaky
//! network shouldn't kill the whole daemon.
//!
//! MCP is intentionally NOT part of the daemon. AI clients (Claude Desktop,
//! rockycc, etc.) spawn `pimsteward mcp` as a child process with stdio
//! transport, matching the forwardemail MCP server pattern. Decoupling
//! means the daemon can run as a low-privilege user doing nothing but
//! pulling, and each MCP client gets its own isolated process on demand.

use crate::config::{CalendarSourceKind, Config, ContactsSourceKind, MailSourceKind};
use crate::error::Error;
use crate::forwardemail::Client;
use crate::permission::Resource;
use crate::pull;
use crate::source::{
    imap::{idle_loop, ImapConfig},
    CalendarSource, ContactsSource, DavCalendarSource, DavContactsSource, ImapMailSource,
    MailSource, RestCalendarSource, RestContactsSource, RestMailSource,
};
use crate::store::Repo;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
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
        let contacts_source: Arc<dyn ContactsSource> = match cfg.forwardemail.contacts_source {
            ContactsSourceKind::Rest => Arc::new(RestContactsSource::new(client.clone())),
            ContactsSourceKind::Carddav => {
                let (u, p) = cfg.load_credentials()?;
                Arc::new(DavContactsSource::new(
                    cfg.forwardemail.carddav_base_url.clone(),
                    u,
                    p,
                )?)
            }
        };
        handles.push(spawn_contacts_puller(
            Duration::from_secs(cfg.pull.contacts_interval_seconds),
            contacts_source,
            repo.clone(),
            alias.clone(),
        ));
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
        let calendar_source: Arc<dyn CalendarSource> = match cfg.forwardemail.calendar_source {
            CalendarSourceKind::Rest => Arc::new(RestCalendarSource::new(client.clone())),
            CalendarSourceKind::Caldav => {
                let (u, p) = cfg.load_credentials()?;
                Arc::new(DavCalendarSource::new(
                    cfg.forwardemail.caldav_base_url.clone(),
                    u,
                    p,
                )?)
            }
        };
        handles.push(spawn_calendar_puller(
            Duration::from_secs(cfg.pull.calendar_interval_seconds),
            calendar_source,
            repo.clone(),
            alias.clone(),
        ));
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

        // If IMAP IDLE is enabled (only meaningful with mail_source=imap),
        // spawn a dedicated IDLE listener on its own connection and wire a
        // Notify to wake the puller when new data arrives. The periodic
        // ticker still runs as a safety net: if the IDLE connection dies
        // the puller keeps syncing on its interval.
        let idle_notify = if cfg.forwardemail.imap_idle
            && matches!(cfg.forwardemail.mail_source, MailSourceKind::Imap)
        {
            let notify = Arc::new(Notify::new());
            let (u, p) = cfg.load_credentials()?;
            let idle_cfg = ImapConfig {
                host: cfg.forwardemail.imap_host.clone(),
                port: cfg.forwardemail.imap_port,
                user: u,
                password: p,
            };
            let notify_clone = notify.clone();
            let span = tracing::info_span!("imap_idle");
            handles.push(tokio::spawn(
                async move {
                    // INBOX is where new mail lands; IDLE there covers the
                    // overwhelming majority of push-worthy events. Non-INBOX
                    // changes fall through to the periodic ticker.
                    idle_loop(idle_cfg, "INBOX".to_string(), notify_clone, None).await;
                }
                .instrument(span),
            ));
            Some(notify)
        } else {
            None
        };

        let h = spawn_mail_puller(
            Duration::from_secs(cfg.pull.mail_interval_seconds),
            mail_source,
            repo.clone(),
            alias.clone(),
            idle_notify,
        );
        handles.push(h);
    }

    if handles.is_empty() {
        return Err(Error::config(
            "daemon has nothing to do — config.permissions grants no resource read access",
        ));
    }

    // Weekly git gc timer. Runs `git gc --auto` against the backup repo
    // on a fixed cadence. Tiny cost, prevents long-term loose-object
    // fragmentation on long-running deployments.
    handles.push(spawn_gc_timer(repo.clone()));

    // Wait for a shutdown signal (SIGINT/SIGTERM on unix). Whichever comes
    // first, log it and return. The tokio task for each puller is detached;
    // they'll be cancelled when the main task returns and tokio shuts down.
    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, daemon exiting");
    Ok(())
}

/// Background task that runs `git gc --auto` on the backup repo every
/// seven days. The `--auto` flag means git decides whether gc is actually
/// needed based on the repo's loose-object count — no-ops when there's
/// nothing to compact. Cheap to run; prevents slow fragmentation.
fn spawn_gc_timer(repo: Arc<Repo>) -> tokio::task::JoinHandle<()> {
    const GC_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    let span = tracing::info_span!("gc_timer");
    tokio::spawn(
        async move {
            let mut ticker = interval(GC_INTERVAL);
            // The first tick fires immediately; skip it so we don't gc at
            // every daemon start.
            ticker.tick().await;
            tracing::info!(
                interval_days = 7,
                "git gc timer started (first run in 7 days)"
            );
            loop {
                ticker.tick().await;
                let root = repo.root().to_path_buf();
                let result = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("git")
                        .args(["gc", "--auto"])
                        .current_dir(&root)
                        .output()
                })
                .await;
                match result {
                    Ok(Ok(out)) if out.status.success() => {
                        tracing::info!("git gc --auto ok");
                    }
                    Ok(Ok(out)) => {
                        tracing::warn!(
                            status = ?out.status,
                            stderr = %String::from_utf8_lossy(&out.stderr),
                            "git gc --auto non-zero exit"
                        );
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "git gc --auto spawn failed"),
                    Err(e) => tracing::warn!(error = %e, "git gc --auto join failed"),
                }
            }
        }
        .instrument(span),
    )
}

fn spawn_contacts_puller(
    period: Duration,
    source: Arc<dyn ContactsSource>,
    repo: Arc<Repo>,
    alias: String,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("puller", resource = "contacts");
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(
                period_secs = period.as_secs(),
                source = source.tag(),
                "contacts puller started"
            );
            loop {
                ticker.tick().await;
                let result = pull::contacts::pull_contacts(
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

fn spawn_calendar_puller(
    period: Duration,
    source: Arc<dyn CalendarSource>,
    repo: Arc<Repo>,
    alias: String,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("puller", resource = "calendar");
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(
                period_secs = period.as_secs(),
                source = source.tag(),
                "calendar puller started"
            );
            loop {
                ticker.tick().await;
                let result = pull::calendar::pull_calendar(
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

fn spawn_mail_puller(
    period: Duration,
    source: Arc<dyn MailSource>,
    repo: Arc<Repo>,
    alias: String,
    idle_notify: Option<Arc<Notify>>,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("puller", resource = "mail");
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            // The first tick fires immediately, so we always do an initial
            // pull on startup regardless of whether IDLE is wired up.
            tracing::info!(
                period_secs = period.as_secs(),
                source = source.tag(),
                idle = idle_notify.is_some(),
                "mail puller started"
            );
            loop {
                // Wake on whichever fires first: the periodic ticker, or
                // (when IDLE is enabled) a signal from the idle listener.
                // `Notify` semantics: a notify_one() that arrives while
                // nobody is notified().await-ing is latched, so we never
                // miss an event between pulls.
                match &idle_notify {
                    Some(n) => {
                        tokio::select! {
                            _ = ticker.tick() => {
                                tracing::trace!("mail pull: ticker fired");
                            }
                            _ = n.notified() => {
                                tracing::debug!("mail pull: IDLE wake");
                            }
                        }
                    }
                    None => {
                        ticker.tick().await;
                    }
                }

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
