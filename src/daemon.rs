//! Long-running daemon: spawns one tokio task per resource with its own
//! pull interval, plus an HTTP MCP server so AI clients can connect via
//! Streamable HTTP / SSE. Errors are logged and pull loops keep running —
//! a flaky network shouldn't kill the whole daemon.
//!
//! Each MCP session gets a fresh `PimstewardServer` via the HTTP service's
//! factory closure, giving session isolation without process-per-client
//! overhead.

use crate::config::{CalendarSourceKind, Config, ContactsSourceKind, MailSourceKind};
use crate::error::Error;
use crate::forwardemail::Client;
use crate::mcp::PimstewardServer;
use crate::permission::Resource;
use crate::pull;
use crate::source::{
    imap::{idle_loop, ImapConfig},
    CalendarSource, ContactsSource, DavCalendarSource, DavContactsSource, ImapMailSource,
    MailSource, MailWriter, RestCalendarSource, RestContactsSource, RestMailSource,
};
use crate::store::Repo;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Notify};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Notification emitted when the mail pull loop detects changes.
/// Consumers (e.g. rockycc email watchers) subscribe via the
/// `/notifications` SSE endpoint instead of watching files.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MailNotification {
    /// Which account (alias) this notification is for.
    pub alias: String,
    /// Number of new messages added.
    pub added: usize,
    /// Number of messages updated (flags, folder moves).
    pub updated: usize,
    /// Number of messages deleted.
    pub deleted: usize,
    /// ISO8601 timestamp of the pull.
    pub timestamp: String,
}

/// HTTP server options for the embedded MCP endpoint. When `port` is
/// `Some`, the daemon serves MCP over Streamable HTTP alongside its pull
/// loops. When `None`, the daemon only pulls (useful for testing or
/// headless backup-only deployments).
pub struct HttpOptions {
    pub host: String,
    pub port: u16,
    pub bearer_token_file: Option<PathBuf>,
}

/// Build the configured MailSource + MailWriter pair. Extracted here so
/// both the pull loops and the MCP server factory can reuse it.
fn build_mail_source(
    cfg: &Config,
    client: Client,
    user: &str,
    password: &str,
) -> (Arc<dyn MailSource>, Arc<dyn MailWriter>) {
    match cfg.forwardemail.mail_source {
        MailSourceKind::Rest => {
            let rest = Arc::new(crate::source::RestMailSource::new(client));
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

/// Run the pull daemon indefinitely, optionally serving MCP over HTTP.
/// Returns when a fatal error occurs or when a shutdown signal is received.
pub async fn run(cfg: Config, http: Option<HttpOptions>) -> Result<(), Error> {
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

    // Broadcast channel for mail notifications. Consumers subscribe via
    // the /notifications SSE endpoint. Buffer 64 events — slow consumers
    // that fall behind will miss events (acceptable; they'll catch up on
    // the next pull cycle).
    let (mail_tx, _) = broadcast::channel::<MailNotification>(64);

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
            mail_tx.clone(),
        );
        handles.push(h);
    }

    // Weekly git gc timer. Runs `git gc --auto` against the backup repo
    // on a fixed cadence. Tiny cost, prevents long-term loose-object
    // fragmentation on long-running deployments.
    handles.push(spawn_gc_timer(repo.clone()));

    // ── MCP HTTP server (optional) ────────────────────────────────
    let ct = CancellationToken::new();
    if let Some(http_opts) = http {
        let expected_token = match &http_opts.bearer_token_file {
            Some(path) => {
                let token = std::fs::read_to_string(path)
                    .map_err(|e| Error::config(format!("reading bearer token file: {e}")))?
                    .trim()
                    .to_string();
                if token.is_empty() {
                    return Err(Error::config(format!(
                        "bearer token file is empty: {}",
                        path.display()
                    )));
                }
                tracing::info!("bearer token auth enabled");
                Some(token)
            }
            None => {
                tracing::warn!("no --bearer-token-file set, MCP HTTP endpoint is unauthenticated");
                None
            }
        };

        let cfg_for_factory = cfg.clone();
        let ct_http = ct.clone();

        use axum::{
            extract::Request,
            http::StatusCode,
            middleware::{self, Next},
            response::{IntoResponse, Response},
        };
        use rmcp::transport::streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig,
            StreamableHttpService,
        };

        let service: StreamableHttpService<PimstewardServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || {
                    let cfg = cfg_for_factory.clone();
                    let (user, pass) = cfg
                        .load_credentials()
                        .map_err(std::io::Error::other)?;
                    let alias = user.replace('@', "-");
                    let client = Client::new(
                        cfg.forwardemail.api_base.clone(),
                        user.clone(),
                        pass.clone(),
                    )
                    .map_err(std::io::Error::other)?;
                    let (mail_source, mail_writer) =
                        build_mail_source(&cfg, client.clone(), &user, &pass);
                    let repo = Repo::open_or_init(&cfg.storage.repo_path)
                        .map_err(std::io::Error::other)?;
                    let caller = std::env::var("PIMSTEWARD_CALLER")
                        .ok()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| "ai".to_string());
                    let managesieve = crate::mcp::ManageSieveConfig {
                        host: cfg.forwardemail.managesieve_host.clone(),
                        port: cfg.forwardemail.managesieve_port,
                        user: user.clone(),
                        password: pass.clone(),
                    };
                    Ok(PimstewardServer::new(
                        client,
                        repo,
                        cfg.permissions.clone(),
                        alias,
                        caller,
                        mail_source,
                        mail_writer,
                        managesieve,
                    ))
                },
                Default::default(),
                StreamableHttpServerConfig::default()
                    .with_cancellation_token(ct_http.child_token()),
            );

        let mail_rx = mail_tx.clone();
        let router = axum::Router::new()
            .nest_service("/mcp", service)
            .route("/notifications", axum::routing::get(move || {
                let rx = mail_rx.subscribe();
                async move { notifications_sse(rx) }
            }))
            .layer(
            middleware::from_fn(move |req: Request, next: Next| {
                let token = expected_token.clone();
                async move {
                    if let Some(ref expected) = token {
                        let provided = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("Bearer "));
                        use subtle::ConstantTimeEq;
                        let ok = match provided {
                            Some(t) if t.len() == expected.len() => {
                                t.as_bytes().ct_eq(expected.as_bytes()).into()
                            }
                            _ => false,
                        };
                        if !ok {
                            return Ok(StatusCode::UNAUTHORIZED.into_response());
                        }
                    }
                    Ok::<Response, std::convert::Infallible>(next.run(req).await)
                }
            }),
        );

        let bind_addr = format!("{}:{}", http_opts.host, http_opts.port);
        let tcp_listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| Error::config(format!("binding {bind_addr}: {e}")))?;
        tracing::info!(addr = %bind_addr, "mcp-http server listening");

        let ct_shutdown = ct.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp_listener, router)
                .with_graceful_shutdown(async move { ct_shutdown.cancelled().await })
                .await
            {
                tracing::error!(error = %e, "mcp-http server error");
            }
        }));
    }

    if handles.is_empty() {
        return Err(Error::config(
            "daemon has nothing to do — config.permissions grants no resource read access and no HTTP server configured",
        ));
    }

    // Wait for a shutdown signal (SIGINT/SIGTERM on unix). Whichever comes
    // first, cancel the HTTP server and return. Pull tasks are detached;
    // they'll be cancelled when the main task returns and tokio shuts down.
    wait_for_shutdown().await;
    ct.cancel();
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
    mail_tx: broadcast::Sender<MailNotification>,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("puller", resource = "mail");
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(
                period_secs = period.as_secs(),
                source = source.tag(),
                idle = idle_notify.is_some(),
                "mail puller started"
            );
            loop {
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
                    Ok(ref s) => {
                        tracing::info!(summary = %s, "pull ok");
                        // Broadcast notification if there were changes
                        if !s.is_noop() {
                            let _ = mail_tx.send(MailNotification {
                                alias: alias.clone(),
                                added: s.added,
                                updated: s.updated,
                                deleted: s.deleted,
                                timestamp: chrono::Utc::now().to_rfc3339(),
                            });
                        }
                    }
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

/// SSE handler for `/notifications`. Streams mail pull notifications as
/// Server-Sent Events. Consumers (e.g. rockycc email watchers) subscribe
/// to this instead of watching pimsteward's git repo via bind mounts.
fn notifications_sse(
    rx: broadcast::Receiver<MailNotification>,
) -> axum::response::Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    use axum::response::sse::Event;
    use futures_util::stream;

    let stream = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    let data = serde_json::to_string(&notif).unwrap_or_default();
                    let event = Event::default()
                        .event("mail")
                        .data(data);
                    return Some((Ok(event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "SSE subscriber lagged, skipping events");
                    // Continue receiving — don't disconnect lagged clients
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return None; // Channel closed, end stream
                }
            }
        }
    });

    axum::response::Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("ping"),
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
