//! Long-running daemon: spawns one tokio task per resource with its own
//! pull interval, plus an HTTP MCP server so AI clients can connect via
//! Streamable HTTP / SSE. Errors are logged and pull loops keep running —
//! a flaky network shouldn't kill the whole daemon.
//!
//! Each MCP session gets a fresh `PimstewardServer` via the HTTP service's
//! factory closure, giving session isolation without process-per-client
//! overhead.

use crate::config::Config;
use crate::error::Error;
use crate::forwardemail::writes::NewMessage;
use crate::forwardemail::Client;
use crate::mcp::PimstewardServer;
use crate::scheduling::Sender;
use crate::permission::{Permissions, Resource};
use crate::provider::{
    forwardemail::ForwardemailProvider, icloud_caldav::IcloudCaldavProvider, Provider,
};
use crate::pull;
use crate::source::{
    imap::idle_loop, CalendarSource, ContactsSource, MailSource,
};
use crate::store::Repo;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
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

/// Read and validate a bearer token from a file. Trims whitespace and
/// rejects empty files so a deployment bug can never silently produce
/// an unauthenticated endpoint.
fn read_bearer_token(path: &Path) -> Result<String, Error> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| {
            Error::config(format!("reading bearer token file {}: {e}", path.display()))
        })?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(Error::config(format!(
            "bearer token file is empty: {}",
            path.display()
        )));
    }
    Ok(token)
}

/// Spawn a single MCP HTTP listener with its own bearer token and
/// permission matrix. Shared between the default CLI-configured
/// listener and every entry in `config.mcp_profiles`.
///
/// Each listener is a fully independent axum service: its factory
/// closure clones `profile_permissions` into every new `PimstewardServer`
/// session, and its auth middleware checks the caller's bearer against
/// the token captured here — no shared state between listeners.
#[allow(clippy::too_many_arguments)]
async fn spawn_mcp_http_listener(
    profile_name: &str,
    host: &str,
    port: u16,
    expected_token: Option<String>,
    cfg: Arc<Config>,
    provider: Arc<dyn Provider>,
    fe_provider: Option<Arc<ForwardemailProvider>>,
    profile_permissions: Permissions,
    caller: String,
    mail_tx: broadcast::Sender<MailNotification>,
    ct: CancellationToken,
) -> Result<tokio::task::JoinHandle<()>, Error> {
    use axum::{
        extract::Request,
        http::StatusCode,
        middleware::{self, Next},
        response::{IntoResponse, Response},
    };
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    };

    let cfg_for_factory = cfg.clone();
    let perms_for_factory = profile_permissions;
    let caller_for_factory = caller;
    let provider_for_factory = provider.clone();
    // `fe_for_factory` is `None` for providers that don't surface a
    // forwardemail-style REST client (e.g. iCloud CalDAV). The MCP server
    // accepts an `Option<Client>` and individual tools gate behind
    // `require_*` helpers that return a structured "not supported by the
    // active provider" error if invoked against a provider that lacks
    // the resource. SearchIndex is repo-local — every daemon gets its
    // own, regardless of which resources the provider exposes.
    let fe_for_factory = fe_provider.clone();
    let ct_http = ct.clone();

    let service: StreamableHttpService<PimstewardServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                let cfg = cfg_for_factory.clone();
                let perms = perms_for_factory.clone();
                let caller = caller_for_factory.clone();
                let provider = provider_for_factory.clone();
                let fe = fe_for_factory.clone();
                let alias = provider.alias().to_string();
                let provider_name = provider.name();

                // Mail / contacts are Optional — the factory tolerates
                // calendar-only providers like iCloud by passing `None`
                // through to the MCP server.
                let mail_source =
                    provider.build_mail_source().map_err(std::io::Error::other)?;
                let mail_writer =
                    provider.build_mail_writer().map_err(std::io::Error::other)?;
                let contacts_source =
                    provider.build_contacts_source().map_err(std::io::Error::other)?;

                // Calendar source + writer are required: every provider
                // pimsteward supports today exposes calendar. Unwrap loudly
                // — a `None` here is a provider-impl bug, not a config issue.
                let calendar_source = provider
                    .build_calendar_source()
                    .map_err(std::io::Error::other)?
                    .ok_or_else(|| {
                        std::io::Error::other("provider returned no calendar source for MCP factory")
                    })?;
                let calendar_writer = provider
                    .build_calendar_writer()
                    .map_err(std::io::Error::other)?
                    .ok_or_else(|| {
                        std::io::Error::other("provider returned no calendar writer for MCP factory")
                    })?;

                // Client + ManageSieve + SieveBackend: Forward Email surfaces
                // a REST `client` and its own ManageSieve endpoint (implicit
                // TLS); Stalwart has no REST surface but DOES expose
                // ManageSieve (STARTTLS on 4190) and runs user Sieve scripts
                // via the LMTP ingest pipeline. iCloud has neither. Downcast
                // once and reuse for the SMTP sender, the ManageSieve config,
                // and the SieveBackend so we don't pay the as_any cost twice.
                let stalwart = provider
                    .as_any()
                    .downcast_ref::<crate::provider::stalwart::StalwartProvider>();
                let (client, managesieve) = match fe.as_ref() {
                    Some(fe) => (
                        Some(fe.client().clone()),
                        Some(crate::mcp::ManageSieveConfig {
                            host: cfg.forwardemail.managesieve_host.clone(),
                            port: cfg.forwardemail.managesieve_port,
                            user: fe.user().to_string(),
                            password: fe.password().to_string(),
                        }),
                    ),
                    None => (
                        None,
                        stalwart.map(|st| {
                            let ms = st.managesieve_config();
                            crate::mcp::ManageSieveConfig {
                                host: ms.host,
                                port: ms.port,
                                user: ms.user,
                                password: ms.password,
                            }
                        }),
                    ),
                };

                // SieveBackend — the trait the MCP `list_sieve_rules` /
                // `add_sieve_rule` / `remove_sieve_rule` tools dispatch
                // through. FE wraps the REST `client` + the ManageSieve
                // config (REST for CRUD, ManageSieve for activation);
                // Stalwart is pure ManageSieve (STARTTLS). iCloud: `None`.
                let sieve_backend: Option<Arc<dyn crate::source::traits::SieveBackend>> =
                    match (client.as_ref(), managesieve.as_ref(), stalwart) {
                        (Some(c), Some(ms), _) => Some(Arc::new(
                            crate::source::sieve::ForwardemailSieveBackend::new(
                                c.clone(),
                                ms.clone(),
                            ),
                        )),
                        (None, Some(ms), _) => Some(Arc::new(
                            crate::source::sieve::StalwartSieveBackend::new(ms.clone()),
                        )),
                        _ => None,
                    };

                // SMTP-submission sender for the `send_email` tool. Built only
                // for Stalwart (which has no REST send surface) by downcasting
                // the type-erased provider and reading its inherent
                // `smtp_config()` — the SAME as_any/smtp_config pattern
                // `build_scheduling_sender` uses for the daemon's scheduling
                // send. forwardemail and iCloud get `None`: forwardemail sends
                // through the REST `client`, iCloud has no send transport.
                let smtp_sender = stalwart
                    .map(|st| Arc::new(crate::smtp::SmtpSender::new(st.smtp_config())));

                let repo =
                    Repo::open_or_init(&cfg.storage.repo_path).map_err(std::io::Error::other)?;
                let search_index = Arc::new(
                    crate::index::SearchIndex::open(repo.root())
                        .map_err(std::io::Error::other)?,
                );
                Ok(PimstewardServer::new(
                    client,
                    repo,
                    perms,
                    alias,
                    provider_name,
                    caller,
                    mail_source,
                    mail_writer,
                    contacts_source,
                    calendar_source,
                    calendar_writer,
                    managesieve,
                    sieve_backend,
                    search_index,
                    smtp_sender,
                ))
            },
            Default::default(),
            // Stateless mode: every HTTP request is a self-contained MCP call
            // with no Mcp-Session-Id affinity to a specific server-side worker.
            // Why stateless: pimsteward has no per-session server state (every
            // tool reads/writes the git-backed repo on disk), so the session
            // layer only added failure modes — a daily systemd restart or any
            // brief pimsteward bounce would invalidate every client's
            // session_id, and the Python MCP SDK then returns
            // `{"error":{"code":32600,"message":"Session terminated"}}` on
            // every subsequent tool call without reinitializing. Hermes reads
            // that as "MCP is down." Stateless mode returns plain JSON (no
            // SSE framing) and the client doesn't track a session id at all,
            // so a pimsteward restart is invisible to consumers.
            //
            // Server-initiated notifications still flow through the separate
            // `/notifications` SSE endpoint — they're independent of the MCP
            // transport, so no functionality is lost here.
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true)
                .with_cancellation_token(ct_http.child_token()),
        );

    let mail_rx = mail_tx.clone();
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .route(
            "/notifications",
            axum::routing::get(move || {
                let rx = mail_rx.subscribe();
                async move { notifications_sse(rx) }
            }),
        )
        .layer(middleware::from_fn(move |req: Request, next: Next| {
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
        }));

    let bind_addr = format!("{host}:{port}");
    let tcp_listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| Error::config(format!("binding {bind_addr}: {e}")))?;
    tracing::info!(
        addr = %bind_addr,
        profile = profile_name,
        "mcp-http server listening"
    );

    let ct_shutdown = ct.clone();
    let profile_log = profile_name.to_string();
    Ok(tokio::spawn(async move {
        if let Err(e) = axum::serve(tcp_listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled().await })
            .await
        {
            tracing::error!(profile = %profile_log, error = %e, "mcp-http server error");
        }
    }))
}

/// Pair of provider handles the daemon keeps in scope: the type-erased
/// trait object plus an optional forwardemail-specific handle for
/// resources only the forwardemail provider exposes (sieve REST client,
/// IMAP IDLE config). Aliased so `build_provider_handles` doesn't trip
/// `clippy::type_complexity`.
pub(crate) type ProviderHandles = (Arc<dyn Provider>, Option<Arc<ForwardemailProvider>>);

/// Build the daemon's two provider handles from a `Config`. Extracted
/// from [`run`] so the construction-once invariant is testable in
/// isolation (see [`tests::build_provider_handles_for_forwardemail_returns_shared_arc`]).
///
/// Returns a [`ProviderHandles`] pair:
///   * `Arc<dyn Provider>` — the type-erased trait object the MCP factory
///     and capability-gated pull spawners dispatch through.
///   * `Option<Arc<ForwardemailProvider>>` — `Some` when the active
///     provider is forwardemail and the daemon needs forwardemail-specific
///     handles (sieve pull's REST `Client`, IMAP IDLE listener,
///     ManageSieve config). `None` for iCloud and any future
///     calendar-only provider.
///
/// Construction-once is load-bearing: `ForwardemailProvider::new`
/// allocates a fresh REST `Client` each call (with its own connection
/// pool), and the IMAP variant also pre-builds a per-instance session
/// cache. The two returned Arcs MUST share the same underlying
/// `ForwardemailProvider` so capability-routed traits (calendar-writer,
/// mail-source, …) and forwardemail-only handles (sieve `Client`, IMAP
/// IDLE config) point at one connection pool. We build the typed Arc
/// first, then erase to `dyn Provider` — that also avoids
/// `Arc::downcast` gymnastics.
pub(crate) fn build_provider_handles(cfg: &Config) -> Result<ProviderHandles, Error> {
    match cfg.active_provider_kind()? {
        crate::config::ProviderKind::Forwardemail => {
            let fe = Arc::new(ForwardemailProvider::new(cfg)?);
            let dyn_provider: Arc<dyn Provider> = fe.clone();
            Ok((dyn_provider, Some(fe)))
        }
        crate::config::ProviderKind::IcloudCaldav => {
            let ic: Arc<dyn Provider> = Arc::new(IcloudCaldavProvider::new(cfg)?);
            Ok((ic, None))
        }
        // Stalwart, like iCloud, is a type-erased `dyn Provider` — it does
        // not need the forwardemail-specific `Arc<ForwardemailProvider>`
        // second handle (its sieve/SMTP surfaces are exposed via inherent
        // accessors, not the forwardemail REST `Client`), so `fe_provider`
        // is `None`.
        crate::config::ProviderKind::Stalwart => {
            let st: Arc<dyn Provider> =
                Arc::new(crate::provider::stalwart::StalwartProvider::new(cfg)?);
            Ok((st, None))
        }
    }
}

/// Run the pull daemon indefinitely, optionally serving MCP over HTTP.
/// Returns when a fatal error occurs or when a shutdown signal is received.
pub async fn run(cfg: Config, http: Option<HttpOptions>) -> Result<(), Error> {
    let (provider, fe_provider) = build_provider_handles(&cfg)?;

    // Reject configs that grant permissions on resources this provider
    // can't serve — fails the daemon at startup rather than at first
    // tool call. Pure config bug: better to scream now than to silently
    // expose a permission key the provider will always refuse.
    cfg.permissions
        .validate_against_capabilities(&provider.capabilities())?;

    let alias = provider.alias().to_string();
    let repo = Arc::new(Repo::open_or_init(&cfg.storage.repo_path)?);
    let cfg = Arc::new(cfg);

    tracing::info!(alias = %alias, repo = ?cfg.storage.repo_path, "pimsteward daemon starting");

    let mut handles = Vec::new();

    if cfg.permissions.check_read(Resource::Contacts).is_ok() {
        if let Some(contacts_source) = provider.build_contacts_source()? {
            handles.push(spawn_contacts_puller(
                Duration::from_secs(cfg.pull.contacts_interval_seconds),
                contacts_source,
                repo.clone(),
                alias.clone(),
            ));
        } else {
            tracing::warn!(
                provider = provider.name(),
                "permission grants Contacts but provider does not support it; ignoring",
            );
        }
    }

    if cfg.permissions.check_read(Resource::Sieve).is_ok() && provider.capabilities().sieve {
        // The sieve *backup pull* reads full script content, which only the
        // forwardemail REST `Client` exposes — the ManageSieve client
        // (`forwardemail/managesieve.rs`) speaks LIST/SETACTIVE, not
        // GETSCRIPT, so it can't snapshot script bodies. Route the pull
        // through the active provider: forwardemail gets the REST puller;
        // any other sieve-capable provider (Stalwart) skips the content
        // backup with a clear log rather than panicking. Sieve *activation*
        // for Stalwart still works — it's wired separately through the MCP
        // layer's ManageSieve config (see `managesieve_config`).
        match fe_provider.as_ref() {
            Some(fe) => {
                let client = fe.client().clone();
                let ms = crate::mcp::ManageSieveConfig {
                    host: cfg.forwardemail.managesieve_host.clone(),
                    port: cfg.forwardemail.managesieve_port,
                    user: fe.user().to_string(),
                    password: fe.password().to_string(),
                };
                let backend =
                    crate::source::sieve::ForwardemailSieveBackend::new(client, ms);
                let h = spawn_puller(
                    "sieve",
                    Duration::from_secs(cfg.pull.sieve_interval_seconds),
                    backend,
                    repo.clone(),
                    alias.clone(),
                    |b, r, a| {
                        Box::pin(async move {
                            pull::sieve::pull_sieve(
                                &b,
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
            None => {
                // Stalwart: pull sieve scripts via ManageSieve (STARTTLS).
                // The SieveBackend is the same one the MCP tools use, built
                // from the StalwartProvider's managesieve_config(). iCloud
                // has no sieve surface at all, so we only enter this branch
                // when the provider advertises sieve (see capabilities check
                // above) — for iCloud that check fails and we never get here.
                if let Some(st) = provider
                    .as_any()
                    .downcast_ref::<crate::provider::stalwart::StalwartProvider>()
                {
                    let ms = st.managesieve_config();
                    let backend = crate::source::sieve::StalwartSieveBackend::new(
                        crate::mcp::ManageSieveConfig {
                            host: ms.host,
                            port: ms.port,
                            user: ms.user,
                            password: ms.password,
                        },
                    );
                    let h = spawn_puller(
                        "sieve",
                        Duration::from_secs(cfg.pull.sieve_interval_seconds),
                        backend,
                        repo.clone(),
                        alias.clone(),
                        |b, r, a| {
                            Box::pin(async move {
                                pull::sieve::pull_sieve(
                                    &b,
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
                } else {
                    tracing::info!(
                        provider = provider.name(),
                        "sieve capability present but no SieveBackend available; skipping \
                         sieve content backup pull (activation may still be available via \
                         ManageSieve)",
                    );
                }
            }
        }
    }

    if cfg.permissions.check_read(Resource::Calendar).is_ok() {
        if let Some(calendar_source) = provider.build_calendar_source()? {
            // Wire scheduling only when enabled in config and the provider has
            // a forwardemail send `Client`. `[scheduling] enabled` IS the
            // authorization: it's an explicit per-provider opt-in, and the
            // scheduler can only ever email actual attendees of events this
            // alias organizes (enforced in the planner). It is deliberately
            // NOT gated on the MCP `email_send` permission — that governs what
            // AI callers may do via the send_email tool (kept denied for dan@),
            // a different and broader trust domain than this constrained,
            // daemon-internal iMIP send.
            let scheduling_ctx = if cfg.scheduling.enabled {
                match build_scheduling_sender(&provider, fe_provider.as_ref(), &alias) {
                    Some(sender) => {
                        Some(Arc::new((sender, cfg.scheduling.notify_on_send)))
                    }
                    None => {
                        tracing::warn!(
                            provider = provider.name(),
                            "scheduling.enabled but provider has no send transport; scheduling disabled"
                        );
                        None
                    }
                }
            } else {
                None
            };
            // Establish the activation watermark at the current HEAD right now,
            // so every pre-existing event is fenced off immediately and the
            // very next calendar change is eligible — rather than consuming the
            // first change as the baseline.
            if scheduling_ctx.is_some() {
                if let Err(e) = crate::scheduling::watermark::ensure_watermark_at_head(&repo) {
                    tracing::error!(error = %e, "failed to set scheduling activation watermark");
                }
            }
            handles.push(spawn_calendar_puller(
                Duration::from_secs(cfg.pull.calendar_interval_seconds),
                calendar_source,
                repo.clone(),
                alias.clone(),
                scheduling_ctx,
            ));
        } else {
            tracing::warn!(
                provider = provider.name(),
                "permission grants Calendar but provider does not support it; ignoring",
            );
        }
    }

    // Broadcast channel for mail notifications. Consumers subscribe via
    // the /notifications SSE endpoint. Buffer 64 events — slow consumers
    // that fall behind will miss events (acceptable; they'll catch up on
    // the next pull cycle).
    let (mail_tx, _) = broadcast::channel::<MailNotification>(64);

    if cfg.permissions.check_read(Resource::Email).is_ok() {
        // Startup self-heal for the search index.  Compares the row
        // count to the .eml count on disk and kicks off an incremental
        // rebuild when they diverge, so a freshly-deployed pimsteward
        // (empty DB, populated mail tree) or a long-offline alias
        // (many moves/deletes missed) wakes up with a current index
        // even if nobody has run `pimsteward index rebuild` manually.
        //
        // Driven by three knobs, each override-able via env:
        //   PIMSTEWARD_INDEX_DRIFT_THRESHOLD_PCT   (default 2.0)
        //   PIMSTEWARD_INDEX_DRIFT_THRESHOLD_ROWS  (default 100)
        //   PIMSTEWARD_INDEX_SKIP_STARTUP_REBUILD  (set non-empty to skip)
        //   PIMSTEWARD_INDEX_FORCE_STARTUP_REBUILD (set non-empty to force)
        // The drift check uses AND: both percentage and row thresholds
        // must trip before a rebuild fires, so a small mailbox doesn't
        // thrash the index every time one message moves.
        maybe_rebuild_index_on_startup(&repo);

        // Mail has a dedicated puller because it dispatches on a
        // MailSource trait object, not a concrete Client.
        if let Some(mail_source) = provider.build_mail_source()? {
            // If IMAP IDLE is enabled (only meaningful with mail_source=imap),
            // spawn a dedicated IDLE listener on its own connection and wire a
            // Notify to wake the puller when new data arrives. The periodic
            // ticker still runs as a safety net: if the IDLE connection dies
            // the puller keeps syncing on its interval.
            //
            // IDLE is forwardemail-only — gated behind the optional fe_provider.
            let idle_notify = if fe_provider
                .as_ref()
                .is_some_and(|fe| fe.imap_idle_enabled())
            {
                let fe = fe_provider.as_ref().unwrap();
                let notify = Arc::new(Notify::new());
                let idle_cfg = fe.imap_config();
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
        } else {
            tracing::warn!(
                provider = provider.name(),
                "permission grants Email but provider does not support it; ignoring",
            );
        }
    }

    // Weekly git gc timer. Runs `git gc --auto` against the backup repo
    // on a fixed cadence. Tiny cost, prevents long-term loose-object
    // fragmentation on long-running deployments.
    handles.push(spawn_gc_timer(repo.clone()));

    // ── MCP HTTP servers ─────────────────────────────────────────
    //
    // Always spawn:
    //   1. (optional) the "default" listener on `http.port` using
    //      `--bearer-token-file` and the top-level `[permissions]`
    //   2. (optional) one additional listener per entry in
    //      `config.mcp_profiles`, each with its own port, token, caller,
    //      and permission matrix
    //
    // Profiles are additive and never mutate the default listener's
    // permissions — rockycc keeps hitting `:8101/mcp` with its
    // unchanged restricted token, while spamguard gets its own
    // `:8102/mcp` with full mailbox-write access.
    let ct = CancellationToken::new();

    if let Some(http_opts) = http {
        // Default listener.
        let default_caller = std::env::var("PIMSTEWARD_CALLER")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "ai".to_string());
        let expected_token = match &http_opts.bearer_token_file {
            Some(path) => Some(read_bearer_token(path)?),
            None => {
                tracing::warn!(
                    "no --bearer-token-file set, default MCP HTTP endpoint is unauthenticated"
                );
                None
            }
        };
        if expected_token.is_some() {
            tracing::info!("bearer token auth enabled for default MCP endpoint");
        }

        let listener = spawn_mcp_http_listener(
            "default",
            &http_opts.host,
            http_opts.port,
            expected_token,
            cfg.clone(),
            provider.clone(),
            fe_provider.clone(),
            cfg.permissions.clone(),
            default_caller,
            mail_tx.clone(),
            ct.clone(),
        )
        .await?;
        handles.push(listener);
    }

    // Additional per-profile listeners.
    //
    // Each profile is independent — its token is read once at daemon
    // start and baked into a closure, its permissions are cloned into
    // the factory closure so every new MCP session gets its own
    // baked-in view. If a profile's token file is missing the daemon
    // refuses to start rather than silently falling back to the default
    // permissions (that would be a dangerous privilege surprise).
    for profile in &cfg.mcp_profiles {
        tracing::info!(
            profile = %profile.name,
            port = profile.port,
            caller = profile.caller_name(),
            "spawning per-profile MCP HTTP listener",
        );
        let token = read_bearer_token(&profile.bearer_token_file)?;
        let listener = spawn_mcp_http_listener(
            &profile.name,
            "0.0.0.0",
            profile.port,
            Some(token),
            cfg.clone(),
            provider.clone(),
            fe_provider.clone(),
            profile.permissions.clone(),
            profile.caller_name().to_string(),
            mail_tx.clone(),
            ct.clone(),
        )
        .await?;
        handles.push(listener);
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
                // Run gc through Repo::gc so it holds `commit_lock` for the
                // whole operation — otherwise gc races concurrent pull commits
                // and can corrupt the object store (zero-byte objects, broken
                // HEAD). gc is blocking, so do it on the blocking pool.
                let repo = repo.clone();
                let result = tokio::task::spawn_blocking(move || repo.gc()).await;
                match result {
                    Ok(Ok(())) => tracing::info!("git gc --auto ok"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "git gc --auto failed"),
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

/// Build the scheduling [`Sender`] for the active provider, or `None` if the
/// provider has no outbound transport.
///
/// Send routing is **provider-driven**, not forwardemail-assumed:
///   * forwardemail → [`ClientSender`] wrapping the REST `Client`
///     (`POST /v1/emails`), unchanged from before.
///   * Stalwart → [`SmtpSender`] over STARTTLS submission (`:587`, AUTH),
///     built from the provider's [`StalwartProvider::smtp_config`]. Sent
///     mail relays out through Forward Email (Stalwart's configured relay).
///   * any other provider (e.g. iCloud CalDAV, which has no send surface)
///     → `None`, so the caller disables scheduling cleanly.
///
/// Returning a boxed trait object keeps the calendar puller's scheduling
/// path identical across providers.
fn build_scheduling_sender(
    provider: &Arc<dyn Provider>,
    fe_provider: Option<&Arc<ForwardemailProvider>>,
    alias: &str,
) -> Option<Box<dyn Sender>> {
    // forwardemail: REST send via the shared `Client`.
    if let Some(fe) = fe_provider {
        return Some(Box::new(ClientSender {
            client: fe.client().clone(),
            alias: alias.to_string(),
        }));
    }
    // Stalwart: SMTP submission. Downcast the type-erased provider to read
    // its inherent `smtp_config()` accessor (the SMTP endpoint isn't part of
    // the `Provider` trait, which only covers the five `build_*` resources).
    if let Some(st) = provider
        .as_any()
        .downcast_ref::<crate::provider::stalwart::StalwartProvider>()
    {
        return Some(Box::new(crate::smtp::SmtpSender::new(st.smtp_config())));
    }
    None
}

/// Adapts the forwardemail REST `Client` to the `scheduling::Sender` trait so
/// the scheduling engine can send iMIP messages and owner notifications without
/// depending on the forwardemail module directly.
struct ClientSender {
    client: Client,
    alias: String,
}

#[async_trait]
impl Sender for ClientSender {
    async fn send_imip(
        &self, to: &str, subject: &str, text_body: &str, payload: &str, method: &str,
    ) -> Result<String, Error> {
        let v = self
            .client
            .send_imip(to, subject, text_body, payload, method)
            .await?;
        Ok(v.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string())
    }

    async fn notify(&self, subject: &str, body: &str) -> Result<(), Error> {
        let msg = NewMessage {
            folder: String::new(),
            to: vec![self.alias.clone()],
            cc: vec![],
            bcc: vec![],
            subject: subject.to_string(),
            text: Some(body.to_string()),
            html: None,
            in_reply_to: None,
            references: vec![],
        };
        self.client.send_email(&msg).await?;
        Ok(())
    }
}

fn spawn_calendar_puller(
    period: Duration,
    source: Arc<dyn CalendarSource>,
    repo: Arc<Repo>,
    alias: String,
    // (sender, notify_on_send). When `None`, scheduling is disabled and the
    // puller behaves exactly as it did before scheduling was wired in. The
    // sender is a trait object so the same puller drives forwardemail's REST
    // [`ClientSender`] and the generic [`SmtpSender`] (Stalwart) alike.
    scheduling_ctx: Option<Arc<(Box<dyn Sender>, bool)>>,
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
                // Borrow `result` so we can both log it and pull the
                // commit_sha out for scheduling without moving the summary.
                let commit_sha = match &result {
                    Ok(s) => {
                        tracing::info!(summary = %s, "pull ok");
                        s.commit_sha.clone()
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "pull failed");
                        None
                    }
                };
                // Only run scheduling when wired (ctx present) and the pull
                // actually produced a commit.
                if let (Some(ctx), Some(sha)) = (&scheduling_ctx, commit_sha) {
                    let now = chrono::Utc::now();
                    match crate::scheduling::run_scheduling(
                        &repo, &sha, ctx.0.as_ref(), &alias, ctx.1, now,
                    )
                    .await
                    {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(sent = n, "scheduling sent iMIP messages"),
                        Err(e) => tracing::error!(error = %e, "scheduling failed"),
                    }
                }
            }
        }
        .instrument(span),
    )
}

/// Count the `.eml` files under `<repo>/mail/**`.  Used for drift detection
/// against the index row count.  Best-effort: a read error on a single
/// folder is logged and the walk continues, so the counter can be slightly
/// low but never wildly wrong.
fn count_eml_files(repo_root: &std::path::Path) -> u64 {
    let mail_root = repo_root.join("mail");
    if !mail_root.is_dir() {
        return 0;
    }
    let mut n = 0u64;
    let folder_entries = match std::fs::read_dir(&mail_root) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "count_eml_files: read_dir mail/");
            return 0;
        }
    };
    for entry in folder_entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            if name.starts_with('_') || name.starts_with('.') {
                continue;
            }
        }
        let inner = match std::fs::read_dir(&p) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(folder = ?p, error = %e, "count_eml_files: read_dir folder");
                continue;
            }
        };
        for entry in inner.flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|e| e == "eml")
            {
                n += 1;
            }
        }
    }
    n
}

/// Return true iff both drift thresholds trip simultaneously.
fn drift_triggers_rebuild(idx: u64, disk: u64, pct_threshold: f64, rows_threshold: u64) -> bool {
    if disk == 0 {
        return false;
    }
    let diff = idx.abs_diff(disk);
    let pct = (diff as f64) * 100.0 / (disk as f64);
    diff > rows_threshold && pct > pct_threshold
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_bool(name: &str) -> bool {
    std::env::var(name).map(|s| !s.is_empty()).unwrap_or(false)
}

fn maybe_rebuild_index_on_startup(repo: &Arc<Repo>) {
    if env_bool("PIMSTEWARD_INDEX_SKIP_STARTUP_REBUILD") {
        tracing::info!("PIMSTEWARD_INDEX_SKIP_STARTUP_REBUILD set; skipping index startup rebuild");
        return;
    }
    let index = match crate::index::SearchIndex::open(repo.root()) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "search index open failed during startup; continuing without rebuild check");
            return;
        }
    };
    let disk = count_eml_files(repo.root());
    let idx = match index.message_count() {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "search index count failed; skipping rebuild check");
            return;
        }
    };
    let force = env_bool("PIMSTEWARD_INDEX_FORCE_STARTUP_REBUILD");
    let empty = idx == 0 && disk > 0;
    let pct = env_f64("PIMSTEWARD_INDEX_DRIFT_THRESHOLD_PCT", 2.0);
    let rows = env_u64("PIMSTEWARD_INDEX_DRIFT_THRESHOLD_ROWS", 100);
    let drift = drift_triggers_rebuild(idx, disk, pct, rows);

    if !(force || empty || drift) {
        tracing::info!(idx, disk, "index startup check: in sync");
        return;
    }
    tracing::warn!(
        idx,
        disk,
        force,
        empty,
        drift,
        pct_threshold = pct,
        rows_threshold = rows,
        "search index requires rebuild; running incremental scan"
    );
    let opts = if force {
        crate::index::RebuildOpts::force()
    } else {
        crate::index::RebuildOpts::incremental()
    };
    match index.rebuild_from_disk(repo.root(), opts) {
        Ok(stats) => tracing::info!(
            scanned = stats.scanned,
            upserted = stats.upserted,
            skipped = stats.skipped,
            orphaned_deleted = stats.orphaned_deleted,
            errors = stats.errors,
            elapsed_ms = stats.elapsed_ms,
            "index startup rebuild complete"
        ),
        Err(e) => tracing::warn!(error = %e, "index startup rebuild failed; daemon will continue"),
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod startup_rebuild_tests {
    use super::*;

    #[test]
    fn drift_requires_both_thresholds() {
        // 1 row drift on a 10-row disk is 10% — exceeds pct but below rows.
        assert!(!drift_triggers_rebuild(9, 10, 2.0, 100));
        // 1000-row drift on 100k disk is 1% — below pct, above rows.
        assert!(!drift_triggers_rebuild(99_000, 100_000, 2.0, 100));
        // 500-row drift on 10k disk is 5% — above both.
        assert!(drift_triggers_rebuild(9_500, 10_000, 2.0, 100));
        // Index vs disk symmetry: index larger than disk also triggers
        // when the drift is above thresholds.
        assert!(drift_triggers_rebuild(10_500, 10_000, 2.0, 100));
    }

    #[test]
    fn drift_empty_disk_is_never_triggering() {
        // A totally fresh deploy has disk=0 and idx=0; don't churn.
        assert!(!drift_triggers_rebuild(0, 0, 2.0, 100));
    }

    #[test]
    fn count_eml_files_walks_folders() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("mail/INBOX")).unwrap();
        std::fs::create_dir_all(root.join("mail/Archive_2026")).unwrap();
        std::fs::write(root.join("mail/INBOX/a.eml"), b"").unwrap();
        std::fs::write(root.join("mail/INBOX/a.meta.json"), b"{}").unwrap();
        std::fs::write(root.join("mail/Archive_2026/b.eml"), b"").unwrap();
        std::fs::write(root.join("mail/Archive_2026/c.eml"), b"").unwrap();
        // _attachments must be ignored (content-addressed store, not mail).
        std::fs::create_dir_all(root.join("mail/_attachments")).unwrap();
        std::fs::write(root.join("mail/_attachments/xxx"), b"blob").unwrap();
        assert_eq!(count_eml_files(root), 3);
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::config::{ForwardemailConfig, ProviderConfigs};

    /// Build a forwardemail-shaped Config with on-disk credential files
    /// so `ForwardemailProvider::new` succeeds. Mirrors the same pattern
    /// used by the IMAP-shared-Arc test in `provider/forwardemail.rs`.
    fn forwardemail_config_with_temp_creds(dir: &tempfile::TempDir) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@example.com").unwrap();
        std::fs::write(&p, "passw0rd").unwrap();
        Config {
            provider: ProviderConfigs {
                forwardemail: Some(ForwardemailConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(p),
                    ..ForwardemailConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    /// Real regression test for the daemon's construct-once invariant.
    /// Calls `build_provider_handles` (the helper extracted from
    /// `daemon::run`), then asserts the two returned `Arc`s share one
    /// allocation. If a future refactor reverts `daemon::run`'s match arm
    /// to construct via `provider::build` AND `ForwardemailProvider::new`
    /// separately, the strong_count drops to 1 here and this test fails.
    /// The previous test in `provider/mod.rs` only exercised `Arc::clone`
    /// and missed that regression class.
    #[test]
    fn build_provider_handles_for_forwardemail_returns_shared_arc() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = forwardemail_config_with_temp_creds(&dir);
        let (dyn_provider, fe) =
            build_provider_handles(&cfg).expect("forwardemail config builds cleanly");
        let fe = fe.expect("forwardemail config returns Some(fe)");

        // Two Arcs, one allocation. If the daemon ever goes back to
        // double-construction, count drops to 1.
        assert!(
            Arc::strong_count(&fe) >= 2,
            "fe Arc strong_count = {}; expected >= 2 (would be 1 if daemon \
             construction allocates twice)",
            Arc::strong_count(&fe),
        );

        // Drop the dyn_provider; strong_count must drop accordingly,
        // proving the two Arcs really did share one allocation rather
        // than independently happening to point at equal data.
        drop(dyn_provider);
        assert_eq!(
            Arc::strong_count(&fe),
            1,
            "after dropping dyn_provider, fe must be the only owner",
        );
    }

    /// iCloud path: no `fe_provider` returned, and the dyn_provider is
    /// the only handle to its allocation.
    #[test]
    fn build_provider_handles_for_icloud_returns_no_fe() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@icloud.com").unwrap();
        std::fs::write(&p, "app-spec-pass").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                icloud_caldav: Some(crate::config::IcloudCaldavConfig {
                    username_file: Some(u),
                    password_file: Some(p),
                    ..crate::config::IcloudCaldavConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let (dyn_provider, fe) = build_provider_handles(&cfg).unwrap();
        assert!(fe.is_none(), "iCloud path must not return an fe handle");
        assert_eq!(
            Arc::strong_count(&dyn_provider),
            1,
            "iCloud dyn_provider is the only handle"
        );
    }

    /// Build a `Config` with a `[provider.stalwart]` block backed by temp
    /// credential files so `StalwartProvider::new` succeeds offline.
    fn stalwart_config_with_temp_creds(dir: &tempfile::TempDir) -> Config {
        use crate::config::StalwartConfig;
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "dan@example.test").unwrap();
        std::fs::write(&p, "pw").unwrap();
        Config {
            provider: ProviderConfigs {
                stalwart: Some(StalwartConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(p),
                    caldav_base_url: "https://h:8443/dav/cal/u/default".into(),
                    carddav_base_url: "https://h:8443/dav/card/u/default".into(),
                    smtp_host: "stalwart.example.test".into(),
                    smtp_port: 587,
                    managesieve_host: "stalwart.example.test".into(),
                    managesieve_port: 4190,
                    ..StalwartConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    /// Regression: the Stalwart daemon path must build its handles WITHOUT a
    /// forwardemail handle. The old sieve gate did
    /// `fe_provider.expect(...)`, which would panic here because Stalwart
    /// reports `capabilities().sieve == true` yet returns `fe == None`. This
    /// asserts the precondition the panic depended on (sieve-capable +
    /// fe-absent) so the daemon's `match fe_provider` gate is the thing under
    /// test rather than an unreachable arm.
    #[test]
    fn stalwart_handles_have_no_fe_but_advertise_sieve() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = stalwart_config_with_temp_creds(&dir);
        let (provider, fe) = build_provider_handles(&cfg).expect("stalwart config builds");
        assert!(fe.is_none(), "stalwart must not return an fe handle");
        assert!(
            provider.capabilities().sieve,
            "stalwart advertises sieve — this is exactly the config that used to panic"
        );
    }

    /// Send dispatch is provider-driven: Stalwart selects the SMTP-submission
    /// sender (NOT the forwardemail REST `Client`), and the chosen sender's
    /// From address is the configured alias. If a regression reverted send to
    /// `fe_provider.expect(...)`/REST-only, `build_scheduling_sender` would
    /// return `None` for Stalwart (no fe handle) and this fails.
    #[test]
    fn scheduling_sender_for_stalwart_is_smtp() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = stalwart_config_with_temp_creds(&dir);
        let (provider, fe) = build_provider_handles(&cfg).unwrap();
        // The Stalwart arm must yield a sender (proves send does NOT take the
        // fe-only path, which would return None here).
        let sender = build_scheduling_sender(&provider, fe.as_ref(), provider.alias());
        assert!(sender.is_some(), "stalwart must yield an SMTP scheduling sender");
        // And it resolves the SMTP endpoint from the provider's inherent
        // smtp_config — the same construction `build_scheduling_sender` uses.
        let st = provider
            .as_any()
            .downcast_ref::<crate::provider::stalwart::StalwartProvider>()
            .expect("provider is stalwart");
        let direct = crate::smtp::SmtpSender::new(st.smtp_config());
        assert_eq!(direct.from_address(), "dan@example.test");
        assert_eq!(st.smtp_config().host, "stalwart.example.test");
        assert_eq!(st.smtp_config().port, 587);
    }

    /// forwardemail send routing is unchanged: it still yields a sender
    /// (the REST-backed `ClientSender`).
    #[test]
    fn scheduling_sender_for_forwardemail_is_some() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = forwardemail_config_with_temp_creds(&dir);
        let (provider, fe) = build_provider_handles(&cfg).unwrap();
        let sender = build_scheduling_sender(&provider, fe.as_ref(), provider.alias());
        assert!(sender.is_some(), "forwardemail must yield a scheduling sender");
    }

    /// iCloud has no outbound transport → no scheduling sender (clean
    /// disable, never a panic).
    #[test]
    fn scheduling_sender_for_icloud_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@icloud.com").unwrap();
        std::fs::write(&p, "app-spec-pass").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                icloud_caldav: Some(crate::config::IcloudCaldavConfig {
                    username_file: Some(u),
                    password_file: Some(p),
                    ..crate::config::IcloudCaldavConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let (provider, fe) = build_provider_handles(&cfg).unwrap();
        let sender = build_scheduling_sender(&provider, fe.as_ref(), provider.alias());
        assert!(sender.is_none(), "iCloud has no send transport");
    }
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

#[allow(dead_code)]
fn _pull_fn_unused(_: PullFn) {}

fn spawn_puller<T>(
    name: &'static str,
    period: Duration,
    input: T,
    repo: Arc<Repo>,
    alias: String,
    f: impl Fn(
            T,
            Arc<Repo>,
            String,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>
        + Send
        + Sync
        + 'static,
) -> tokio::task::JoinHandle<()>
where
    T: Clone + Send + Sync + 'static,
{
    let span = tracing::info_span!("puller", resource = name);
    tokio::spawn(
        async move {
            let mut ticker = interval(period);
            tracing::info!(period_secs = period.as_secs(), "puller started");
            loop {
                ticker.tick().await;
                match (f)(input.clone(), repo.clone(), alias.clone()).await {
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
