//! Native IMAP MailSource using async-imap + tokio-rustls.
//!
//! Connects to forwardemail's IMAP server (`imap.forwardemail.net:993`
//! by default) with the alias email + generated password. Uses CONDSTORE
//! where available to surface `modseq` for per-message delta sync so the
//! pull loop can skip bodies that haven't changed.
//!
//! # What this provides vs REST
//!
//! - **Raw bytes via `FETCH BODY[]`** — byte-identical RFC822, same as
//!   REST's `raw` field.
//! - **modseq per message** — available via REST too, but IMAP's
//!   CONDSTORE lets us one day pull only messages with
//!   `modseq > <last_known>` (not yet implemented in this minimal impl).
//! - **Lower per-request overhead** at high message counts (one IMAP
//!   session does everything; REST is one HTTP round-trip per call).
//!
//! # What this doesn't do (yet)
//!
//! - CONDSTORE `CHANGEDSINCE` filtering on the FETCH command — v2.2
//!   lists all messages and diffs locally, same as REST. Adding
//!   CHANGEDSINCE is a later optimization.
//! - IDLE for push notifications.
//! - Writes — flag updates, folder moves, and deletes continue to go
//!   through the REST write path regardless of the read source.

use crate::error::Error;
use crate::forwardemail::mail::{Folder, MessageSummary};
use crate::source::traits::{FetchedMessage, ListResult, MailSource};
use async_imap::extensions::idle::IdleResponse;
use async_imap::Session;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use std::sync::Arc;
use std::time::Duration;
use socket2::SockRef;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// TCP keepalive interval. Sends a probe every 60 seconds on an idle
/// connection, which prevents NAT/firewall/load-balancer timeouts from
/// silently dropping the socket. Without this, forwardemail.net's
/// infrastructure closes idle connections after a few minutes — breaking
/// both the cached puller session and the IDLE long-poll.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Open a TcpStream with SO_KEEPALIVE enabled before handing it to TLS.
async fn tcp_connect_with_keepalive(host: &str, port: u16) -> Result<TcpStream, Error> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| Error::config(format!("IMAP TCP connect: {e}")))?;

    let sock = SockRef::from(&tcp);
    let ka = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE);
    // with_interval is Linux-only; set it when available so probes
    // repeat at the same cadence if the first goes unanswered.
    #[cfg(target_os = "linux")]
    let ka = ka.with_interval(TCP_KEEPALIVE);
    sock.set_tcp_keepalive(&ka)
        .map_err(|e| Error::config(format!("TCP keepalive: {e}")))?;

    Ok(tcp)
}

/// Configuration for connecting to an IMAP server.
#[derive(Debug, Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

impl ImapConfig {
    pub fn forwardemail(alias_user: impl Into<String>, alias_password: impl Into<String>) -> Self {
        Self {
            host: "imap.forwardemail.net".to_string(),
            port: 993,
            user: alias_user.into(),
            password: alias_password.into(),
        }
    }
}

type ImapSession = Session<TlsStream<TcpStream>>;

pub struct ImapMailSource {
    config: ImapConfig,
    // Sessions are expensive to establish; cache one and re-use across
    // calls. Mutex because async-imap's Session takes &mut self for every
    // operation.
    session: Arc<Mutex<Option<ImapSession>>>,
}

impl std::fmt::Debug for ImapMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapMailSource")
            .field("host", &self.config.host)
            .field("user", &self.config.user)
            .finish_non_exhaustive()
    }
}

impl Clone for ImapMailSource {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            session: self.session.clone(),
        }
    }
}

impl ImapMailSource {
    pub fn new(config: ImapConfig) -> Self {
        Self {
            config,
            session: Arc::new(Mutex::new(None)),
        }
    }

    /// Establish a fresh IMAP connection + login. Called lazily by
    /// `session_guard` when no cached session is available.
    async fn connect(&self) -> Result<ImapSession, Error> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let tcp = tcp_connect_with_keepalive(&self.config.host, self.config.port).await?;
        let server_name = ServerName::try_from(self.config.host.clone())
            .map_err(|e| Error::config(format!("IMAP server name: {e}")))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| Error::config(format!("IMAP TLS handshake: {e}")))?;

        let client = async_imap::Client::new(tls);
        let session = client
            .login(&self.config.user, &self.config.password)
            .await
            .map_err(|(e, _)| Error::config(format!("IMAP login: {e}")))?;

        Ok(session)
    }

    /// Ensure a session is alive and return a lock guard holding it. On
    /// subsequent error, the caller should drop the session by setting
    /// `*guard = None` — IMAP errors often leave a session in an
    /// unknown state, so reconnecting is the safe default.
    async fn session_guard(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<ImapSession>>, Error> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        Ok(guard)
    }
}

#[async_trait]
impl MailSource for ImapMailSource {
    fn tag(&self) -> &'static str {
        "imap"
    }

    async fn list_folders(&self) -> Result<Vec<Folder>, Error> {
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<Vec<Folder>, Error> = async {
            let names = session
                .list(Some(""), Some("*"))
                .await
                .map_err(|e| Error::store(format!("IMAP LIST: {e}")))?;
            let collected: Vec<async_imap::types::Name> = names
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP LIST collect: {e}")))?;

            Ok(collected
                .into_iter()
                .map(|n| {
                    let path = n.name().to_string();
                    // Map the typed NameAttribute variants to backslash-
                    // prefixed strings matching the REST source's shape
                    // (Archive, Drafts, Sent, Junk, Trash, ...).
                    let special_use = n.attributes().iter().find_map(|a| {
                        use async_imap::types::NameAttribute as NA;
                        match a {
                            NA::Archive => Some("\\Archive".to_string()),
                            NA::Drafts => Some("\\Drafts".to_string()),
                            NA::Flagged => Some("\\Flagged".to_string()),
                            NA::Junk => Some("\\Junk".to_string()),
                            NA::Sent => Some("\\Sent".to_string()),
                            NA::Trash => Some("\\Trash".to_string()),
                            NA::All => Some("\\All".to_string()),
                            NA::Extension(s) if s.starts_with('\\') => Some(s.to_string()),
                            _ => None,
                        }
                    });
                    Folder {
                        id: path.clone(),
                        path: path.clone(),
                        name: path,
                        uid_validity: None,
                        uid_next: None,
                        modify_index: None,
                        subscribed: true,
                        special_use,
                        created_at: None,
                        updated_at: None,
                    }
                })
                .collect())
        }
        .await;
        if result.is_err() {
            *guard = None; // reset session on error
        }
        result
    }

    async fn list_messages(
        &self,
        folder: &str,
        since_modseq: Option<i64>,
        uid_validity_hint: Option<i64>,
    ) -> Result<ListResult, Error> {
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<ListResult, Error> = async {
            let mailbox = session
                .examine(folder)
                .await
                .map_err(|e| Error::store(format!("IMAP EXAMINE {folder}: {e}")))?;

            let server_uid_validity = mailbox.uid_validity.map(|v| v as i64);
            let server_highest = mailbox.highest_modseq.map(|v| v as i64);

            if mailbox.exists == 0 {
                return Ok(ListResult {
                    all_ids: Vec::new(),
                    changed: Vec::new(),
                    highest_modseq: server_highest,
                    uid_validity: server_uid_validity,
                });
            }

            // CONDSTORE delta sync is only valid when the mailbox's
            // UIDVALIDITY matches what the caller stored. If it's
            // mismatched (or the caller has no stored value), fall back to
            // a full fetch and let the caller persist the new UIDVALIDITY.
            let use_changedsince = matches!(
                (since_modseq, uid_validity_hint, server_uid_validity),
                (Some(_), Some(h), Some(s)) if h == s
            );

            // Always enumerate the full UID set so the caller can detect
            // deletions. UID SEARCH ALL is cheap — server returns just a
            // list of numbers.
            let all_uids: Vec<u32> = {
                let search = session
                    .uid_search("ALL")
                    .await
                    .map_err(|e| Error::store(format!("IMAP UID SEARCH ALL: {e}")))?;
                let mut v: Vec<u32> = search.into_iter().collect();
                v.sort_unstable();
                v
            };
            let all_ids: Vec<String> = all_uids.iter().map(|u| format!("imap-{u}")).collect();

            // Build the FETCH range. With CHANGEDSINCE, FETCH 1:* returns
            // only messages whose modseq > <hint>. Without, we fetch
            // everything.
            let fetch_cmd = if use_changedsince {
                let m = since_modseq.expect("checked above");
                format!("(UID FLAGS MODSEQ INTERNALDATE RFC822.SIZE) (CHANGEDSINCE {m})")
            } else {
                "(UID FLAGS MODSEQ INTERNALDATE RFC822.SIZE)".to_string()
            };

            let messages = session
                .fetch("1:*", &fetch_cmd)
                .await
                .map_err(|e| Error::store(format!("IMAP FETCH summaries: {e}")))?;
            let collected: Vec<async_imap::types::Fetch> = messages
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP FETCH collect: {e}")))?;

            let changed: Vec<MessageSummary> = collected
                .iter()
                .filter_map(|m| {
                    let uid = m.uid?;
                    let flags: Vec<String> = m.flags().map(|f| format!("{f:?}")).collect();
                    Some(MessageSummary {
                        // Synthetic id: `imap-<uid>`. Deployments must not
                        // mix REST and IMAP read sources against the same
                        // backup tree — filenames share a namespace.
                        id: format!("imap-{uid}"),
                        folder_id: folder.to_string(),
                        folder_path: folder.to_string(),
                        subject: String::new(),
                        size: u64::from(m.size.unwrap_or(0)),
                        uid: Some(i64::from(uid)),
                        modseq: m.modseq.map(|v| v as i64),
                        updated_at: None,
                        flags,
                    })
                })
                .collect();

            tracing::debug!(
                folder,
                used_changedsince = use_changedsince,
                since = since_modseq,
                all = all_ids.len(),
                changed = changed.len(),
                "IMAP list_messages"
            );

            Ok(ListResult {
                all_ids,
                changed,
                highest_modseq: server_highest,
                uid_validity: server_uid_validity,
            })
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result
    }

    async fn fetch_message(&self, folder: &str, id: &str) -> Result<FetchedMessage, Error> {
        let uid_str = id.strip_prefix("imap-").ok_or_else(|| {
            Error::store(format!(
                "IMAP source: id {id} not in expected 'imap-<uid>' form"
            ))
        })?;
        let uid: u32 = uid_str
            .parse()
            .map_err(|e| Error::store(format!("IMAP source: parsing uid from {id}: {e}")))?;

        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<FetchedMessage, Error> = async {
            session
                .examine(folder)
                .await
                .map_err(|e| Error::store(format!("IMAP EXAMINE {folder}: {e}")))?;

            let messages = session
                .uid_fetch(
                    uid.to_string(),
                    "(UID FLAGS MODSEQ INTERNALDATE RFC822.SIZE BODY[])",
                )
                .await
                .map_err(|e| Error::store(format!("IMAP UID FETCH {uid}: {e}")))?;
            let collected: Vec<async_imap::types::Fetch> = messages
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP FETCH collect: {e}")))?;

            let msg = collected
                .into_iter()
                .next()
                .ok_or_else(|| Error::store(format!("IMAP UID {uid} returned no message")))?;

            let raw = msg
                .body()
                .ok_or_else(|| Error::store(format!("IMAP UID {uid}: no BODY[] in response")))?
                .to_vec();

            let flags: Vec<String> = msg.flags().map(|f| format!("{f:?}")).collect();
            let summary = MessageSummary {
                id: format!("imap-{uid}"),
                folder_id: folder.to_string(),
                folder_path: folder.to_string(),
                subject: String::new(),
                size: u64::from(msg.size.unwrap_or(0)),
                uid: Some(i64::from(uid)),
                modseq: msg.modseq.map(|v| v as i64),
                updated_at: None,
                flags,
            };

            Ok(FetchedMessage {
                summary,
                raw,
                extra: None,
            })
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result
    }
}

#[async_trait]
impl crate::source::traits::MailWriter for ImapMailSource {
    fn tag(&self) -> &'static str {
        "imap"
    }

    async fn update_flags(&self, folder: &str, id: &str, flags: &[String]) -> Result<(), Error> {
        let uid = parse_imap_uid(id)?;
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            // SELECT (not EXAMINE) so we can write.
            session
                .select(&folder)
                .await
                .map_err(|e| Error::store(format!("IMAP SELECT {folder}: {e}")))?;
            // STORE replaces the flag set entirely.
            let flag_str = flags.join(" ");
            let _: Vec<_> = session
                .uid_store(uid.to_string(), format!("FLAGS ({flag_str})"))
                .await
                .map_err(|e| Error::store(format!("IMAP STORE flags: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            Ok(())
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result
    }

    async fn move_message(
        &self,
        folder: &str,
        id: &str,
        target_folder: &str,
    ) -> Result<(), Error> {
        let uid = parse_imap_uid(id)?;
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            session
                .select(&folder)
                .await
                .map_err(|e| Error::store(format!("IMAP SELECT {folder}: {e}")))?;
            // UID COPY + UID STORE \Deleted + UID EXPUNGE (RFC 4315).
            // RFC 6851 MOVE exists but async-imap doesn't expose it.
            // UID EXPUNGE only removes messages matching the specified
            // UID set, unlike plain EXPUNGE which removes ALL \Deleted
            // messages — critical for safety when other clients may have
            // flagged messages for deletion concurrently.
            session
                .uid_copy(uid.to_string(), target_folder)
                .await
                .map_err(|e| Error::store(format!("IMAP COPY: {e}")))?;
            let _: Vec<_> = session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| Error::store(format!("IMAP STORE \\Deleted: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            session
                .uid_expunge(uid.to_string())
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE: {e}")))?
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE collect: {e}")))?;
            Ok(())
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result
    }

    async fn delete_message(&self, folder: &str, id: &str) -> Result<(), Error> {
        let uid = parse_imap_uid(id)?;
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            session
                .select(&folder)
                .await
                .map_err(|e| Error::store(format!("IMAP SELECT {folder}: {e}")))?;
            let _: Vec<_> = session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| Error::store(format!("IMAP STORE \\Deleted: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            // UID EXPUNGE (RFC 4315) — only removes this specific UID,
            // not other messages that may be \Deleted concurrently.
            session
                .uid_expunge(uid.to_string())
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE: {e}")))?
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE collect: {e}")))?;
            Ok(())
        }
        .await;
        if result.is_err() {
            *guard = None;
        }
        result
    }
}

/// Extract the IMAP UID from an `imap-<uid>` style message id. The
/// folder is passed separately by the caller (it's already known from
/// the backup tree or the permission-check lookup).
fn parse_imap_uid(id: &str) -> Result<u32, Error> {
    let rest = id.strip_prefix("imap-").ok_or_else(|| {
        Error::store(format!("IMAP writer: id {id} not in 'imap-<uid>' form"))
    })?;
    rest.parse::<u32>()
        .map_err(|e| Error::store(format!("IMAP writer: cannot parse uid from {id}: {e}")))
}

/// Run a standalone IMAP IDLE listener forever. Each iteration:
///
///   1. Open a fresh connection + log in (not shared with the puller's
///      session — IDLE monopolises its connection so it must be its own).
///   2. EXAMINE the chosen folder (INBOX by default; forwardemail's mail
///      volume is overwhelmingly INBOX-bound in the typical consumer use
///      case this daemon targets).
///   3. Issue IDLE and block on the server's response channel. On any
///      NewData response or server keepalive, signal the caller via
///      `notify.notify_one()` so the mail puller wakes up and does a
///      full sync pass.
///   4. RFC 2177 requires re-issuing IDLE at least every 29 minutes;
///      async-imap's default timeout respects this. On timeout we DONE
///      and loop back to a fresh IDLE.
///
/// Errors (disconnects, TLS handshake failures, login failures) are
/// logged and retried with exponential backoff capped at 5 minutes so a
/// flaky network doesn't hammer the server.
/// Optional one-shot signal fired when the IDLE connection is first
/// established and ready to receive notifications. Used by tests to
/// avoid a fragile sleep before creating trigger messages; production
/// callers pass `None`.
pub type IdleReady = Option<Arc<Notify>>;

pub async fn idle_loop(
    config: ImapConfig,
    folder: String,
    notify: Arc<Notify>,
    ready: IdleReady,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(5 * 60);

    loop {
        match run_one_idle_connection(&config, &folder, &notify, &ready).await {
            Ok(()) => {
                // Clean exit from the inner loop shouldn't happen except on
                // shutdown — reset the backoff and start a new connection.
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_secs = backoff.as_secs(), "IMAP IDLE error, reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// One IDLE lifecycle: connect, login, examine, enter the IDLE-wait loop,
/// return an error on any failure so the outer loop can back off and
/// reconnect. Returning `Ok(())` also restarts, but with backoff reset.
async fn run_one_idle_connection(
    config: &ImapConfig,
    folder: &str,
    notify: &Notify,
    ready: &IdleReady,
) -> Result<(), Error> {
    // Dedicated connection; the puller's cached session is untouched.
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));

    let tcp = tcp_connect_with_keepalive(&config.host, config.port).await?;
    let server_name = ServerName::try_from(config.host.clone())
        .map_err(|e| Error::config(format!("IDLE server name: {e}")))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| Error::config(format!("IDLE TLS handshake: {e}")))?;

    let client = async_imap::Client::new(tls);
    let mut session = client
        .login(&config.user, &config.password)
        .await
        .map_err(|(e, _)| Error::config(format!("IDLE login: {e}")))?;

    session
        .examine(folder)
        .await
        .map_err(|e| Error::store(format!("IDLE EXAMINE {folder}: {e}")))?;
    tracing::info!(folder, "IMAP IDLE connection established");

    // The inner loop: init IDLE, wait for a notification or timeout, DONE,
    // repeat. On any IMAP-layer error we bubble up so the outer loop
    // reconnects.
    let mut first_idle = true;
    loop {
        let mut handle = session.idle();
        handle
            .init()
            .await
            .map_err(|e| Error::store(format!("IDLE init: {e}")))?;

        // Signal readiness after the first successful IDLE init so tests
        // can create trigger messages without a fragile sleep.
        if first_idle {
            if let Some(r) = ready {
                r.notify_one();
            }
            first_idle = false;
        }

        let (idle_fut, _stop) = handle.wait();
        let result = idle_fut.await;

        // Whatever the outcome, we must DONE to return the session to a
        // usable state before the next iteration.
        session = handle
            .done()
            .await
            .map_err(|e| Error::store(format!("IDLE done: {e}")))?;

        match result {
            Ok(IdleResponse::NewData(_)) => {
                tracing::debug!(folder, "IMAP IDLE: new data, waking puller");
                notify.notify_one();
            }
            Ok(IdleResponse::Timeout) => {
                // 29-minute keepalive — server expects us to DONE and
                // re-IDLE. No wake signal; nothing changed.
                tracing::trace!(folder, "IMAP IDLE: keepalive timeout, re-idling");
            }
            Ok(IdleResponse::ManualInterrupt) => {
                // We don't drive a manual interrupt anywhere, but if the
                // stream closes cleanly, surface it as an error so the
                // outer loop reconnects.
                return Err(Error::store("IDLE stream closed unexpectedly"));
            }
            Err(e) => {
                return Err(Error::store(format!("IDLE wait: {e}")));
            }
        }
    }
}
