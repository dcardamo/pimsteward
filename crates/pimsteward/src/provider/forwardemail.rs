//! Forwardemail provider. Wraps the existing `src/forwardemail/` and
//! `src/source/` code paths into a `Provider` impl so the daemon and the
//! MCP factory closure can construct sources without knowing which
//! upstream they're talking to. No new behaviour here — every method
//! mirrors what `daemon.rs` was doing inline before this abstraction
//! landed.

use std::sync::Arc;

use crate::config::{CalendarSourceKind, Config, ContactsSourceKind, MailSourceKind};
use crate::error::Error;
use crate::forwardemail::Client;
use crate::provider::{Capabilities, Provider};
use crate::source::{
    imap::ImapConfig, CalendarSource, CalendarWriter, ContactsSource, DavCalendarSource,
    DavContactsSource, ImapMailSource, MailSource, MailWriter, RestCalendarSource,
    RestCalendarWriter, RestContactsSource, RestMailSource,
};

/// Provider impl for forwardemail.net. Holds a clone of the
/// `[forwardemail]` config block, the loaded credentials, and a
/// pre-constructed REST `Client` so the four `build_*` methods stay
/// allocation-light.
pub struct ForwardemailProvider {
    cfg: crate::config::ForwardemailConfig,
    user: String,
    password: String,
    alias: String,
    client: Client,
    // Cached source/writer Arcs. Constructed once in `new()` so multiple
    // `build_*` calls return clones of the SAME `Arc`. This preserves the
    // session-sharing invariant the deleted `daemon::build_mail_source`
    // helper relied on: with `mail_source = "imap"`, the source and writer
    // must point at the same `ImapMailSource` instance so they share the
    // inner `Mutex<Option<CachedSession>>` (one IMAP login, one cached
    // SELECT/EXAMINE state, one shared NOOP-probe schedule). Without this,
    // every MCP HTTP request in stateless mode would open two IMAP logins
    // instead of one.
    mail_source: Arc<dyn MailSource>,
    mail_writer: Arc<dyn MailWriter>,
    calendar_source: Arc<dyn CalendarSource>,
    calendar_writer: Arc<dyn CalendarWriter>,
    contacts_source: Arc<dyn ContactsSource>,
}

impl ForwardemailProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        // Prefer the namespaced [provider.forwardemail] block when set;
        // fall back to the legacy top-level [forwardemail] for back-compat.
        let cfg = top_cfg.effective_forwardemail();
        let (user, password) = top_cfg.load_credentials()?;
        let alias = user.replace('@', "-");
        let client = Client::new(cfg.api_base.clone(), user.clone(), password.clone())?;

        // Build mail source/writer once. For IMAP, keep the typed `Arc` and
        // coerce it twice so the two trait objects share the same inner
        // state — mirrors the deleted `daemon::build_mail_source` helper's
        // `(imap.clone(), imap)` shape. Both `RestMailSource` and
        // `ImapMailSource` implement `MailSource` and `MailWriter`, so a
        // typed `Arc<T>` cloned and coerced into both trait views points at
        // the same allocation.
        let (mail_source, mail_writer): (Arc<dyn MailSource>, Arc<dyn MailWriter>) =
            match cfg.mail_source {
                MailSourceKind::Rest => {
                    let rest = Arc::new(RestMailSource::new(client.clone()));
                    (rest.clone(), rest)
                }
                MailSourceKind::Imap => {
                    let imap = Arc::new(ImapMailSource::new(ImapConfig {
                        host: cfg.imap_host.clone(),
                        port: cfg.imap_port,
                        user: user.clone(),
                        password: password.clone(),
                    }));
                    (imap.clone(), imap)
                }
            };

        let calendar_source: Arc<dyn CalendarSource> = match cfg.calendar_source {
            CalendarSourceKind::Rest => Arc::new(RestCalendarSource::new(client.clone())),
            CalendarSourceKind::Caldav => Arc::new(DavCalendarSource::new(
                cfg.caldav_base_url.clone(),
                user.clone(),
                password.clone(),
            )?),
        };

        // Calendar writes always go through the REST API regardless of
        // `calendar_source`. Mirroring the mail-write story: writes carry
        // audit attribution and the REST surface (PUT /v1/calendar-events/:id)
        // is what forwardemail's audit log records — running write through
        // CalDAV would split the write log between two different code paths.
        let calendar_writer: Arc<dyn CalendarWriter> =
            Arc::new(RestCalendarWriter::new(client.clone()));

        let contacts_source: Arc<dyn ContactsSource> = match cfg.contacts_source {
            ContactsSourceKind::Rest => Arc::new(RestContactsSource::new(client.clone())),
            ContactsSourceKind::Carddav => Arc::new(DavContactsSource::new(
                cfg.carddav_base_url.clone(),
                user.clone(),
                password.clone(),
            )?),
        };

        Ok(Self {
            cfg,
            user,
            password,
            alias,
            client,
            mail_source,
            mail_writer,
            calendar_source,
            calendar_writer,
            contacts_source,
        })
    }

    /// REST client handle. Exposed so the daemon can keep using forwardemail-
    /// specific surfaces (sieve pull, ManageSieve, search index) that have
    /// not yet been generalised behind the provider trait.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Alias user (the email address that authenticates against the API).
    /// Exposed for the ManageSieve config and for any forwardemail-specific
    /// code path that needs the raw credentials without re-reading them
    /// from disk.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Alias password matching [`Self::user`]. See `user()` for context.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Build the `ImapConfig` the daemon's IDLE listener needs. Same
    /// host/port/credentials as the puller's `ImapMailSource`.
    pub fn imap_config(&self) -> ImapConfig {
        ImapConfig {
            host: self.cfg.imap_host.clone(),
            port: self.cfg.imap_port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }

    /// True iff the daemon should spawn a dedicated IMAP IDLE connection.
    /// Only meaningful when `mail_source = "imap"`.
    pub fn imap_idle_enabled(&self) -> bool {
        self.cfg.imap_idle && matches!(self.cfg.mail_source, MailSourceKind::Imap)
    }
}

impl Provider for ForwardemailProvider {
    fn name(&self) -> &'static str {
        "forwardemail"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::forwardemail_full()
    }

    fn alias(&self) -> &str {
        &self.alias
    }

    fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error> {
        Ok(Some(self.mail_source.clone()))
    }

    fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        // Returns the same `Arc` allocation as `build_mail_source` so the
        // two trait objects share underlying connection/session state — see
        // the cache fields on `ForwardemailProvider` for why this matters
        // for the IMAP backend.
        Ok(Some(self.mail_writer.clone()))
    }

    fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error> {
        Ok(Some(self.calendar_source.clone()))
    }

    fn build_calendar_writer(&self) -> Result<Option<Arc<dyn CalendarWriter>>, Error> {
        Ok(Some(self.calendar_writer.clone()))
    }

    fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error> {
        Ok(Some(self.contacts_source.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ForwardemailConfig;

    /// Build a `Config` that selects IMAP for mail and points
    /// `alias_user_file` / `alias_password_file` at temp files containing
    /// throwaway credentials. No network calls happen — `ImapMailSource`
    /// only opens its connection lazily on first use.
    fn imap_config_with_temp_creds(dir: &tempfile::TempDir) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "test_user@example.com").unwrap();
        std::fs::write(&p, "test_pass").unwrap();
        Config {
            forwardemail: ForwardemailConfig {
                mail_source: MailSourceKind::Imap,
                alias_user_file: Some(u),
                alias_password_file: Some(p),
                ..ForwardemailConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn imap_mail_source_and_writer_share_session() {
        // Regression guard for the Critical issue from Task 2 review:
        // with `mail_source = "imap"`, calling `build_mail_source` and
        // `build_mail_writer` must return clones of the SAME `Arc` so the
        // resulting trait objects share the inner
        // `Mutex<Option<CachedSession>>`. The MCP factory closure runs per
        // HTTP request in stateless mode — separate `ImapMailSource::new`
        // calls would open two IMAP logins per request.
        let dir = tempfile::tempdir().unwrap();
        let cfg = imap_config_with_temp_creds(&dir);
        let fe = ForwardemailProvider::new(&cfg).unwrap();

        let s = fe.build_mail_source().unwrap().unwrap();
        let w = fe.build_mail_writer().unwrap().unwrap();

        // `Arc::as_ptr` on `Arc<dyn Trait>` returns a pointer to the data
        // (vtable lives in the fat-pointer's metadata, not the data itself).
        // Both trait Arcs were coerced from the same typed `Arc<ImapMailSource>`,
        // so the data pointers must match.
        let s_ptr = Arc::as_ptr(&s) as *const ();
        let w_ptr = Arc::as_ptr(&w) as *const ();
        assert_eq!(
            s_ptr, w_ptr,
            "IMAP source and writer must share the same Arc to share session state",
        );
    }

    #[test]
    fn rest_mail_source_and_writer_share_arc() {
        // Symmetry check. REST has no shared mutable state so this isn't
        // strictly required for correctness, but verifying the same shape
        // catches accidental future regressions where someone restores the
        // double-construction pattern for one branch but not the other.
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "test_user@example.com").unwrap();
        std::fs::write(&p, "test_pass").unwrap();
        let cfg = Config {
            forwardemail: ForwardemailConfig {
                mail_source: MailSourceKind::Rest,
                alias_user_file: Some(u),
                alias_password_file: Some(p),
                ..ForwardemailConfig::default()
            },
            ..Config::default()
        };
        let fe = ForwardemailProvider::new(&cfg).unwrap();

        let s = fe.build_mail_source().unwrap().unwrap();
        let w = fe.build_mail_writer().unwrap().unwrap();

        let s_ptr = Arc::as_ptr(&s) as *const ();
        let w_ptr = Arc::as_ptr(&w) as *const ();
        assert_eq!(s_ptr, w_ptr);
    }
}
