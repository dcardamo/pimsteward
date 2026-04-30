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
    imap::ImapConfig, CalendarSource, ContactsSource, DavCalendarSource, DavContactsSource,
    ImapMailSource, MailSource, MailWriter, RestCalendarSource, RestContactsSource, RestMailSource,
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
}

impl ForwardemailProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        let cfg = top_cfg.forwardemail.clone();
        let (user, password) = top_cfg.load_credentials()?;
        let alias = user.replace('@', "-");
        let client = Client::new(cfg.api_base.clone(), user.clone(), password.clone())?;
        Ok(Self {
            cfg,
            user,
            password,
            alias,
            client,
        })
    }

    /// REST client handle. Exposed so the daemon can keep using forwardemail-
    /// specific surfaces (sieve pull, ManageSieve, search index) that have
    /// not yet been generalised behind the provider trait.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Alias user (the email address that authenticates against the API)
    /// and the matching password. Exposed for the ManageSieve config and
    /// for any forwardemail-specific code path that needs the raw
    /// credentials without re-reading them from disk.
    pub fn credentials(&self) -> (&str, &str) {
        (&self.user, &self.password)
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
        Ok(Some(match self.cfg.mail_source {
            MailSourceKind::Rest => Arc::new(RestMailSource::new(self.client.clone())),
            MailSourceKind::Imap => Arc::new(ImapMailSource::new(self.imap_config())),
        }))
    }

    fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        // Writes always go through REST regardless of read source — see
        // the docs on `[forwardemail].mail_source` for why.
        Ok(Some(match self.cfg.mail_source {
            MailSourceKind::Rest => Arc::new(RestMailSource::new(self.client.clone())),
            MailSourceKind::Imap => Arc::new(ImapMailSource::new(self.imap_config())),
        }))
    }

    fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error> {
        Ok(Some(match self.cfg.calendar_source {
            CalendarSourceKind::Rest => Arc::new(RestCalendarSource::new(self.client.clone())),
            CalendarSourceKind::Caldav => Arc::new(DavCalendarSource::new(
                self.cfg.caldav_base_url.clone(),
                self.user.clone(),
                self.password.clone(),
            )?),
        }))
    }

    fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error> {
        Ok(Some(match self.cfg.contacts_source {
            ContactsSourceKind::Rest => Arc::new(RestContactsSource::new(self.client.clone())),
            ContactsSourceKind::Carddav => Arc::new(DavContactsSource::new(
                self.cfg.carddav_base_url.clone(),
                self.user.clone(),
                self.password.clone(),
            )?),
        }))
    }
}
