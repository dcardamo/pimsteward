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
use crate::source::traits::{FetchedMessage, MailSource};
use async_imap::Session;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

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
    /// `with_session` when no cached session is available.
    async fn connect(&self) -> Result<ImapSession, Error> {
        // TLS setup using rustls with webpki-roots. Matches pimsteward's
        // reqwest TLS story so we have one TLS stack, not two.
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_config));

        let tcp = TcpStream::connect((self.config.host.as_str(), self.config.port))
            .await
            .map_err(|e| Error::config(format!("IMAP TCP connect: {e}")))?;
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

    async fn list_messages(&self, folder: &str) -> Result<Vec<MessageSummary>, Error> {
        let mut guard = self.session_guard().await?;
        let session = guard.as_mut().expect("session present");
        let result: Result<Vec<MessageSummary>, Error> = async {
            let mailbox = session
                .examine(folder)
                .await
                .map_err(|e| Error::store(format!("IMAP EXAMINE {folder}: {e}")))?;

            if mailbox.exists == 0 {
                return Ok(Vec::new());
            }

            // FETCH 1:* UID FLAGS MODSEQ — summary fields only, no bodies.
            let messages = session
                .fetch("1:*", "(UID FLAGS MODSEQ INTERNALDATE RFC822.SIZE)")
                .await
                .map_err(|e| Error::store(format!("IMAP FETCH summaries: {e}")))?;
            let collected: Vec<async_imap::types::Fetch> = messages
                .try_collect()
                .await
                .map_err(|e| Error::store(format!("IMAP FETCH collect: {e}")))?;

            let summaries = collected
                .iter()
                .filter_map(|m| {
                    let uid = m.uid?;
                    let flags: Vec<String> = m.flags().map(|f| format!("{f:?}")).collect();
                    Some(MessageSummary {
                        // Synthetic id: `imap-<uid>`. Deployments must not
                        // mix REST and IMAP read sources against the same
                        // backup tree — filenames are in a single namespace.
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
            Ok(summaries)
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
