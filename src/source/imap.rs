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
use socket2::SockRef;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// The IMAP "open mailbox" modes. `Examine` is read-only, `Select` is
/// read-write. A folder that has been `Select`-ed can be read from, so
/// a subsequent EXAMINE of the same folder is redundant. An `Examine`-d
/// folder cannot be written to, so wanting write access forces a real
/// SELECT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailboxMode {
    Examine,
    Select,
}

/// Whether a SELECT/EXAMINE command can be skipped given the session's
/// cached position. Pure function, tested directly — see
/// [`mailbox_switch_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailboxSwitch {
    /// Session is already positioned on this folder with at least the
    /// required mode; caller can skip the SELECT/EXAMINE entirely. This
    /// is the optimization that keeps us under forwardemail's per-session
    /// EXAMINE limit.
    Skip,
    /// Caller must issue the SELECT/EXAMINE before operating on the
    /// folder — either a different folder is currently positioned, or
    /// the current mode is too weak for what's requested.
    Execute,
}

/// Decide whether to issue an IMAP SELECT/EXAMINE or reuse the cached
/// mailbox state. Extracted as a pure function so it can be unit-tested
/// without a real IMAP session.
///
/// The state machine:
/// - No current mailbox → always Execute.
/// - Different folder → always Execute.
/// - Same folder, current mode is Select → always Skip (Select can do
///   everything Examine can).
/// - Same folder, current mode is Examine, want Examine → Skip.
/// - Same folder, current mode is Examine, want Select → Execute (we
///   must upgrade to read-write).
pub(crate) fn mailbox_switch_decision(
    current: Option<&(String, MailboxMode)>,
    target_folder: &str,
    target_mode: MailboxMode,
) -> MailboxSwitch {
    match current {
        Some((f, mode)) if f == target_folder => match (mode, target_mode) {
            (MailboxMode::Select, _) => MailboxSwitch::Skip,
            (MailboxMode::Examine, MailboxMode::Examine) => MailboxSwitch::Skip,
            (MailboxMode::Examine, MailboxMode::Select) => MailboxSwitch::Execute,
        },
        _ => MailboxSwitch::Execute,
    }
}

/// IMAP session + position cache + mailbox-switch counter.
///
/// Bundles the raw `async-imap` Session with the bookkeeping we need to
/// avoid hammering forwardemail with redundant EXAMINE/SELECT commands.
/// Forwardemail (Wildduck under the hood) caps SELECT/EXAMINE per
/// session to prevent abusive clients, and the initial-sync path for a
/// 5000+ message folder used to issue one EXAMINE per fetched message,
/// blowing that cap and killing the connection before any meaningful
/// progress. See `ensure_positioned` below for the fix.
struct CachedSession {
    session: ImapSession,
    /// The folder + mode the session is currently positioned on, or
    /// `None` immediately after login. Cleared implicitly when the
    /// whole `CachedSession` is dropped on error.
    current: Option<(String, MailboxMode)>,
    /// Count of mailbox-open commands (EXAMINE + SELECT) issued since
    /// the TCP connect. Used by `session_guard` as a belt-and-suspenders
    /// check: if we somehow accumulate too many mailbox switches
    /// despite the position caching, drop the session proactively
    /// rather than waiting for the server to BYE us.
    ///
    /// Only EXAMINE/SELECT count here — they're the commands that
    /// Wildduck caps. FETCH, STORE, SEARCH, COPY, EXPUNGE don't count,
    /// so an initial-sync fetch of thousands of messages from one
    /// folder does not churn this counter and does not cause
    /// reconnects. See `MAX_MAILBOX_SWITCHES_PER_SESSION`.
    mailbox_switches_since_connect: usize,
    /// When the session was last successfully used for a command.
    /// `session_guard` uses this to decide whether to send a NOOP
    /// liveness probe before returning the session: a session that has
    /// been idle longer than `SESSION_IDLE_PROBE` may have had its TCP
    /// connection silently closed by forwardemail's infrastructure, and
    /// probing with a NOOP forces any such breakage to surface as a
    /// reconnect *before* a mutating operation runs against the dead
    /// socket and half-completes.
    last_used_at: Instant,
}

/// Proactively recycle the puller session after this many
/// EXAMINE+SELECT commands. Forwardemail's per-session SELECT/EXAMINE
/// cap is somewhere in the few-hundreds; staying well under it keeps
/// long-lived daemons happy. Because the position cache folds
/// consecutive same-folder operations to a single EXAMINE/SELECT, a
/// "switch" in practice means actually changing folders — so in
/// steady-state Dan's puller hits this many switches after roughly
/// `MAX_MAILBOX_SWITCHES_PER_SESSION / num_folders` pull cycles.
const MAX_MAILBOX_SWITCHES_PER_SESSION: usize = 200;

/// If a cached session has been idle at least this long, `session_guard`
/// sends a NOOP to verify the TCP connection is still alive before
/// returning it. The probe cost is one round-trip; the benefit is that
/// dead-socket failures surface as a clean reconnect instead of as a
/// broken-pipe error in the middle of a multi-step operation like
/// `move_message` (UID COPY + STORE + EXPUNGE), where a mid-sequence
/// failure can leave a message partially moved. Forwardemail aggressively
/// closes idle connections — anything more than a few seconds without
/// traffic is a plausible candidate for server-side reaping.
const SESSION_IDLE_PROBE: Duration = Duration::from_secs(5);

impl CachedSession {
    fn new(session: ImapSession) -> Self {
        Self {
            session,
            current: None,
            mailbox_switches_since_connect: 0,
            last_used_at: Instant::now(),
        }
    }

    /// Refresh the idle timer. Call this after every successful command
    /// so `session_guard` knows the socket was alive as of recently.
    fn mark_used(&mut self) {
        self.last_used_at = Instant::now();
    }

    /// How long has this session been idle since its last successful
    /// command? Used by `session_guard` to decide whether a NOOP probe
    /// is warranted.
    fn idle_for(&self) -> Duration {
        self.last_used_at.elapsed()
    }

    /// Record that a mailbox-open command (EXAMINE/SELECT) was issued.
    /// Only called by the code paths that actually speak those
    /// commands to the server — `ensure_positioned` on its Execute
    /// branch and `examine_for_metadata` unconditionally.
    fn note_mailbox_switch(&mut self) {
        self.mailbox_switches_since_connect = self.mailbox_switches_since_connect.saturating_add(1);
    }

    /// Should the caller recycle this session before doing more work?
    /// See [`MAX_MAILBOX_SWITCHES_PER_SESSION`].
    fn should_recycle(&self) -> bool {
        self.mailbox_switches_since_connect >= MAX_MAILBOX_SWITCHES_PER_SESSION
    }

    /// Ensure the session is positioned on `folder` with at least
    /// `mode` access. Issues EXAMINE/SELECT only when the cache says
    /// we must. On success, updates the cached position. On error,
    /// the caller's outer error handler drops the whole CachedSession
    /// so the cache can't go stale.
    ///
    /// This is the hot path: on initial sync of a big folder, the
    /// puller calls `fetch_message` N times in a row for the same
    /// folder, and this method turns N EXAMINEs into 1.
    async fn ensure_positioned(&mut self, folder: &str, mode: MailboxMode) -> Result<(), Error> {
        match mailbox_switch_decision(self.current.as_ref(), folder, mode) {
            MailboxSwitch::Skip => Ok(()),
            MailboxSwitch::Execute => {
                self.note_mailbox_switch();
                match mode {
                    MailboxMode::Examine => {
                        self.session
                            .examine(folder)
                            .await
                            .map_err(|e| Error::store(format!("IMAP EXAMINE {folder}: {e}")))?;
                    }
                    MailboxMode::Select => {
                        self.session
                            .select(&folder)
                            .await
                            .map_err(|e| Error::store(format!("IMAP SELECT {folder}: {e}")))?;
                    }
                }
                self.current = Some((folder.to_string(), mode));
                Ok(())
            }
        }
    }

    /// EXAMINE `folder` unconditionally and return the fresh mailbox
    /// metadata. Used by `list_messages` which needs current
    /// `uid_validity` / `highest_modseq` / `exists` every call — those
    /// change as new mail arrives and can't be safely cached across
    /// pull cycles. Still updates the position cache so a subsequent
    /// `ensure_positioned(folder, Examine)` is a no-op.
    async fn examine_for_metadata(
        &mut self,
        folder: &str,
    ) -> Result<async_imap::types::Mailbox, Error> {
        self.note_mailbox_switch();
        let mailbox = self
            .session
            .examine(folder)
            .await
            .map_err(|e| Error::store(format!("IMAP EXAMINE {folder}: {e}")))?;
        self.current = Some((folder.to_string(), MailboxMode::Examine));
        Ok(mailbox)
    }
}

pub struct ImapMailSource {
    config: ImapConfig,
    // Sessions are expensive to establish; cache one and re-use across
    // calls. Mutex because async-imap's Session takes &mut self for every
    // operation. The `CachedSession` wrapper adds position + command
    // bookkeeping on top of the raw async-imap Session.
    session: Arc<Mutex<Option<CachedSession>>>,
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
    /// unknown state, so reconnecting is the safe default. This also
    /// clears the `CachedSession.current` position cache automatically:
    /// the whole struct goes away on drop, so the next `session_guard`
    /// call starts with `current = None`.
    ///
    /// Recycles the session proactively when its command counter
    /// exceeds [`MAX_MAILBOX_SWITCHES_PER_SESSION`] — belt-and-suspenders
    /// protection against forwardemail's per-session SELECT/EXAMINE cap.
    ///
    /// Sends a NOOP liveness probe to sessions that have been idle
    /// longer than [`SESSION_IDLE_PROBE`]. Forwardemail silently closes
    /// idle connections, and without this a broken pipe shows up
    /// mid-operation — e.g. after `UID COPY` but before `UID STORE`
    /// during a `move_message`, leaving the message partially moved.
    /// Probing up-front converts dead-socket errors into clean
    /// reconnects before any mutation happens. If the probe fails, the
    /// session is dropped and re-established transparently, so the
    /// caller sees a healthy connection or a hard error — never a
    /// half-dead one.
    async fn session_guard(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<CachedSession>>, Error> {
        let mut guard = self.session.lock().await;
        if let Some(ref cached) = *guard {
            if cached.should_recycle() {
                tracing::debug!(
                    commands = cached.mailbox_switches_since_connect,
                    "recycling IMAP session after hitting command threshold"
                );
                *guard = None;
            }
        }
        // Liveness probe for idle sessions. Only probes if we have a
        // cached session that has been idle long enough to plausibly be
        // dead on the server side. Fresh connections skip this.
        if let Some(cached) = guard.as_mut() {
            if cached.idle_for() >= SESSION_IDLE_PROBE {
                match cached.session.noop().await {
                    Ok(()) => {
                        cached.mark_used();
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            idle_secs = cached.idle_for().as_secs(),
                            "IMAP NOOP probe failed, reconnecting before next operation"
                        );
                        *guard = None;
                    }
                }
            }
        }
        if guard.is_none() {
            *guard = Some(CachedSession::new(self.connect().await?));
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
        let cached = guard.as_mut().expect("session present");
        // LIST doesn't touch a mailbox, so it doesn't count against
        // the SELECT/EXAMINE cap — no note_mailbox_switch here.
        let result: Result<Vec<Folder>, Error> = async {
            let names = cached
                .session
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
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None; // reset session on error
            }
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
        let cached = guard.as_mut().expect("session present");
        let result: Result<ListResult, Error> = async {
            // Always issue a real EXAMINE here — we need fresh mailbox
            // metadata (exists, uid_validity, highest_modseq) every pull
            // because new mail arrives between calls. The position cache
            // is still updated so any immediately-following
            // fetch_message calls on the same folder can skip their
            // EXAMINE.
            let mailbox = cached.examine_for_metadata(folder).await?;

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
            // list of numbers. Neither SEARCH nor FETCH counts against
            // the SELECT/EXAMINE cap.
            let all_uids: Vec<u32> = {
                let search = cached
                    .session
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

            let messages = cached
                .session
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
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None;
            }
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
        let cached = guard.as_mut().expect("session present");
        let result: Result<FetchedMessage, Error> = async {
            // Skip EXAMINE if the session is already positioned here —
            // this is the optimization that saves ~N commands per
            // N-message fetch batch. Before this, pulling a 5922-message
            // archive folder issued 5923 EXAMINEs (1 for list_messages
            // + 1 per fetch) and blew forwardemail's per-session cap;
            // now it issues 1.
            cached
                .ensure_positioned(folder, MailboxMode::Examine)
                .await?;

            // UID FETCH doesn't count against the SELECT/EXAMINE cap.
            let messages = cached
                .session
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
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None;
            }
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
        let cached = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            // Must be Select (not Examine) so we can write. The cache
            // will upgrade Examine → Select if needed.
            cached
                .ensure_positioned(folder, MailboxMode::Select)
                .await?;
            // STORE replaces the flag set entirely. UID STORE does not
            // count against the SELECT/EXAMINE cap.
            let flag_str = flags.join(" ");
            let _: Vec<_> = cached
                .session
                .uid_store(uid.to_string(), format!("FLAGS ({flag_str})"))
                .await
                .map_err(|e| Error::store(format!("IMAP STORE flags: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            Ok(())
        }
        .await;
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None;
            }
        }
        result
    }

    async fn move_message(&self, folder: &str, id: &str, target_folder: &str) -> Result<(), Error> {
        let uid = parse_imap_uid(id)?;
        let mut guard = self.session_guard().await?;
        let cached = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            cached
                .ensure_positioned(folder, MailboxMode::Select)
                .await?;
            // UID COPY + UID STORE \Deleted + UID EXPUNGE (RFC 4315).
            // RFC 6851 MOVE exists but async-imap doesn't expose it.
            // UID EXPUNGE only removes messages matching the specified
            // UID set, unlike plain EXPUNGE which removes ALL \Deleted
            // messages — critical for safety when other clients may have
            // flagged messages for deletion concurrently. None of these
            // count against the SELECT/EXAMINE cap.
            cached
                .session
                .uid_copy(uid.to_string(), target_folder)
                .await
                .map_err(|e| Error::store(format!("IMAP COPY: {e}")))?;
            let _: Vec<_> = cached
                .session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| Error::store(format!("IMAP STORE \\Deleted: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            cached
                .session
                .uid_expunge(uid.to_string())
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE: {e}")))?
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE collect: {e}")))?;
            Ok(())
        }
        .await;
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None;
            }
        }
        result
    }

    async fn delete_message(&self, folder: &str, id: &str) -> Result<(), Error> {
        let uid = parse_imap_uid(id)?;
        let mut guard = self.session_guard().await?;
        let cached = guard.as_mut().expect("session present");
        let result: Result<(), Error> = async {
            cached
                .ensure_positioned(folder, MailboxMode::Select)
                .await?;
            // UID STORE + UID EXPUNGE don't count against the
            // SELECT/EXAMINE cap.
            let _: Vec<_> = cached
                .session
                .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
                .await
                .map_err(|e| Error::store(format!("IMAP STORE \\Deleted: {e}")))?
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP STORE collect: {e}")))?;
            // UID EXPUNGE (RFC 4315) — only removes this specific UID,
            // not other messages that may be \Deleted concurrently.
            cached
                .session
                .uid_expunge(uid.to_string())
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE: {e}")))?
                .try_collect::<Vec<_>>()
                .await
                .map_err(|e| Error::store(format!("IMAP UID EXPUNGE collect: {e}")))?;
            Ok(())
        }
        .await;
        match &result {
            Ok(_) => {
                if let Some(c) = guard.as_mut() {
                    c.mark_used();
                }
            }
            Err(_) => {
                *guard = None;
            }
        }
        result
    }
}

/// Extract the IMAP UID from an `imap-<uid>` style message id. The
/// folder is passed separately by the caller (it's already known from
/// the backup tree or the permission-check lookup).
fn parse_imap_uid(id: &str) -> Result<u32, Error> {
    let rest = id
        .strip_prefix("imap-")
        .ok_or_else(|| Error::store(format!("IMAP writer: id {id} not in 'imap-<uid>' form")))?;
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

pub async fn idle_loop(config: ImapConfig, folder: String, notify: Arc<Notify>, ready: IdleReady) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── mailbox_switch_decision ──────────────────────────────────────
    //
    // These tests nail down the state machine that keeps us under
    // forwardemail's per-session EXAMINE cap. The bug they guard
    // against: if `fetch_message` ever stops skipping its EXAMINE when
    // the session is already positioned on the right folder, an
    // initial sync of a large archive folder will issue thousands of
    // EXAMINE commands in a row and blow the server's limit.

    #[test]
    fn switch_decision_no_current_always_executes() {
        assert_eq!(
            mailbox_switch_decision(None, "INBOX", MailboxMode::Examine),
            MailboxSwitch::Execute
        );
        assert_eq!(
            mailbox_switch_decision(None, "INBOX", MailboxMode::Select),
            MailboxSwitch::Execute
        );
    }

    #[test]
    fn switch_decision_different_folder_always_executes() {
        let current = Some(("Archive/2010".to_string(), MailboxMode::Examine));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "INBOX", MailboxMode::Examine),
            MailboxSwitch::Execute
        );
        let current = Some(("Archive/2010".to_string(), MailboxMode::Select));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "INBOX", MailboxMode::Select),
            MailboxSwitch::Execute
        );
    }

    #[test]
    fn switch_decision_same_folder_same_mode_skips() {
        // This is THE optimization: N consecutive fetch_message calls
        // on the same folder all skip the redundant EXAMINE after the
        // first one.
        let current = Some(("Archive/2010".to_string(), MailboxMode::Examine));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "Archive/2010", MailboxMode::Examine),
            MailboxSwitch::Skip
        );
    }

    #[test]
    fn switch_decision_select_satisfies_examine_request() {
        // If we already SELECT-ed a folder for writing, we can also
        // read from it without re-examining.
        let current = Some(("Spam".to_string(), MailboxMode::Select));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "Spam", MailboxMode::Examine),
            MailboxSwitch::Skip
        );
    }

    #[test]
    fn switch_decision_examine_does_not_satisfy_select_request() {
        // EXAMINE is read-only; writes need SELECT. This forces an
        // upgrade even though the folder is the same. Without this, a
        // sequence like list_messages(INBOX) → move_message(INBOX, …)
        // would try to STORE/EXPUNGE against a read-only mailbox and
        // error out.
        let current = Some(("INBOX".to_string(), MailboxMode::Examine));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "INBOX", MailboxMode::Select),
            MailboxSwitch::Execute
        );
    }

    #[test]
    fn switch_decision_select_same_folder_skips() {
        let current = Some(("INBOX".to_string(), MailboxMode::Select));
        assert_eq!(
            mailbox_switch_decision(current.as_ref(), "INBOX", MailboxMode::Select),
            MailboxSwitch::Skip
        );
    }

    // ── Command counter / recycle threshold ──────────────────────────
    //
    // The recycle path is the belt-and-suspenders guard: even if a
    // future change accidentally defeats the position cache, we still
    // won't exceed the server's per-session command limit because
    // `should_recycle` will tell `session_guard` to drop the session
    // before it gets that far. These tests bolt down the threshold
    // semantics without needing a real IMAP session.

    /// Mirror of CachedSession's counter logic without the session
    /// field. We can't build a real `CachedSession` in a unit test
    /// because its `session: ImapSession` requires a live TLS
    /// connection — but the counter logic is pure and can be tested
    /// in isolation. If you add counter fields or change the
    /// increment/recycle semantics on `CachedSession`, mirror them
    /// here too.
    struct CounterOnly {
        mailbox_switches_since_connect: usize,
    }

    impl CounterOnly {
        fn new() -> Self {
            Self {
                mailbox_switches_since_connect: 0,
            }
        }
        fn note_mailbox_switch(&mut self) {
            self.mailbox_switches_since_connect =
                self.mailbox_switches_since_connect.saturating_add(1);
        }
        fn should_recycle(&self) -> bool {
            self.mailbox_switches_since_connect >= MAX_MAILBOX_SWITCHES_PER_SESSION
        }
    }

    #[test]
    fn counter_starts_at_zero_and_does_not_recycle() {
        let c = CounterOnly::new();
        assert_eq!(c.mailbox_switches_since_connect, 0);
        assert!(!c.should_recycle());
    }

    #[test]
    fn counter_increments_monotonically() {
        let mut c = CounterOnly::new();
        for _ in 0..5 {
            c.note_mailbox_switch();
        }
        assert_eq!(c.mailbox_switches_since_connect, 5);
    }

    #[test]
    fn recycle_triggers_at_or_above_threshold() {
        let mut c = CounterOnly::new();
        for _ in 0..(MAX_MAILBOX_SWITCHES_PER_SESSION - 1) {
            c.note_mailbox_switch();
        }
        assert!(
            !c.should_recycle(),
            "just under threshold should not recycle"
        );
        c.note_mailbox_switch();
        assert!(c.should_recycle(), "at threshold must recycle");
    }

    #[test]
    fn recycle_threshold_is_comfortably_below_typical_imap_cap() {
        // Sanity check: Wildduck (forwardemail's backend) caps
        // SELECT/EXAMINE somewhere in the low hundreds. Keep our
        // recycle threshold comfortably below any plausible server cap
        // so we always recycle before the server BYE's us. If this
        // ever fails, the threshold was raised carelessly.
        assert!(
            MAX_MAILBOX_SWITCHES_PER_SESSION <= 250,
            "MAX_MAILBOX_SWITCHES_PER_SESSION ({}) is too close to the typical Wildduck limit",
            MAX_MAILBOX_SWITCHES_PER_SESSION
        );
        assert!(
            MAX_MAILBOX_SWITCHES_PER_SESSION >= 50,
            "MAX_MAILBOX_SWITCHES_PER_SESSION ({}) is so low the reconnect cost will dominate",
            MAX_MAILBOX_SWITCHES_PER_SESSION
        );
    }

    #[test]
    fn counter_saturates_at_usize_max_instead_of_overflowing() {
        // Paranoia: note_mailbox_switch uses saturating_add so a wedged
        // session that somehow iterates billions of times without
        // hitting an error won't panic. Not reachable in practice but
        // keeps the counter incrementation path branch-free of
        // overflow checks.
        let mut c = CounterOnly {
            mailbox_switches_since_connect: usize::MAX,
        };
        c.note_mailbox_switch();
        assert_eq!(c.mailbox_switches_since_connect, usize::MAX);
        assert!(c.should_recycle());
    }
}
