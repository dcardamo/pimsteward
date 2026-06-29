//! Stalwart provider. Stalwart is a self-hosted, standards-compliant mail
//! (IMAP/SMTP) + CalDAV + CardDAV + ManageSieve server, so unlike the
//! calendar-only iCloud provider it advertises the full capability surface.
//!
//! Composition (no new protocol code — every piece is an existing generic
//! source/writer):
//!
//! - **mail (r/w):** [`ImapMailSource`] over implicit TLS. It implements
//!   both [`MailSource`] and [`MailWriter`], so — exactly like the
//!   forwardemail IMAP path — one `Arc<ImapMailSource>` is coerced into
//!   both trait objects to share a single login/session.
//! - **calendar (r/w):** [`DavCalendarSource::with_collection_url`] +
//!   [`DavCalendarWriter`] (Task 2). Both are pointed at the configured
//!   `caldav_base_url`, which for Stalwart is the full collection URL
//!   (`https://host:8443/dav/cal/<user>/default`). The source's
//!   collection-URL mode skips the forwardemail `/dav/<user>/` discovery
//!   that 404s on Stalwart.
//! - **contacts (read):** [`DavContactsSource::with_collection_url`] against
//!   `carddav_base_url`. Writes go through [`DavContactsWriter`]'s inherent
//!   methods, which the MCP layer calls directly (there is no
//!   `ContactsWriter` trait), mirroring the forwardemail contacts shape.
//!
//! Sieve (ManageSieve) and outbound SMTP submission are advertised in the
//! capability set and exposed via inherent accessors
//! ([`Self::managesieve_config`], [`Self::smtp_config`]) for the daemon /
//! MCP layer to consume — they are not part of the [`Provider`] trait,
//! which only covers the five `build_*` resource constructors.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{Config, StalwartConfig};
use crate::error::Error;
use crate::provider::{Capabilities, Provider};
use crate::source::imap::ImapConfig;
use crate::source::{
    CalendarSource, CalendarWriter, ContactsSource, DavCalendarSource, DavCalendarWriter,
    DavContactsSource, DavContactsWriter, ImapMailSource, MailSource, MailWriter,
};

/// Host + port + credentials for a ManageSieve / SMTP-submission endpoint.
/// Plain data so the daemon can build a ManageSieve session or an SMTP
/// client without reaching back into the provider's private fields.
#[derive(Debug, Clone)]
pub struct StalwartEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

/// Provider impl for a Stalwart server. Pre-builds every source/writer once
/// in `new()` (matching the cache-once invariant the other providers rely
/// on) so repeated `build_*` calls return clones of the same `Arc`.
pub struct StalwartProvider {
    cfg: StalwartConfig,
    user: String,
    password: String,
    alias: String,
    // Mail source and writer are the SAME `ImapMailSource` allocation,
    // coerced into both trait views so they share one IMAP session — see
    // `ForwardemailProvider` for why this matters under stateless MCP.
    mail_source: Arc<dyn MailSource>,
    mail_writer: Arc<dyn MailWriter>,
    calendar_source: Arc<dyn CalendarSource>,
    calendar_writer: Arc<dyn CalendarWriter>,
    contacts_source: Arc<dyn ContactsSource>,
    contacts_writer: Arc<DavContactsWriter>,
}

impl std::fmt::Debug for StalwartProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StalwartProvider")
            .field("user", &self.user)
            .field("alias", &self.alias)
            .finish_non_exhaustive()
    }
}

impl StalwartProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        let cfg = top_cfg
            .provider
            .stalwart
            .clone()
            .ok_or_else(|| Error::config("[provider.stalwart] not configured"))?;
        let user = read_required_credential_file(
            cfg.alias_user_file.as_ref(),
            "provider.stalwart.alias_user_file",
        )?;
        let password = read_required_credential_file(
            cfg.alias_password_file.as_ref(),
            "provider.stalwart.alias_password_file",
        )?;
        let alias = user.replace('@', "-");

        // Mail: one IMAP allocation, both trait views.
        let imap = Arc::new(ImapMailSource::new(ImapConfig {
            host: cfg.imap_host.clone(),
            port: cfg.imap_port,
            user: user.clone(),
            password: password.clone(),
        }));
        let mail_source: Arc<dyn MailSource> = imap.clone();
        let mail_writer: Arc<dyn MailWriter> = imap;

        // Calendar: read + write against the configured collection URL.
        let calendar_source: Arc<dyn CalendarSource> = Arc::new(
            DavCalendarSource::with_collection_url(
                cfg.caldav_base_url.clone(),
                user.clone(),
                password.clone(),
            )?,
        );
        let calendar_writer: Arc<dyn CalendarWriter> = Arc::new(DavCalendarWriter::new(
            cfg.caldav_base_url.clone(),
            user.clone(),
            password.clone(),
        )?);

        // Contacts: read source + write helper against the addressbook URL.
        let contacts_source: Arc<dyn ContactsSource> = Arc::new(
            DavContactsSource::with_collection_url(
                cfg.carddav_base_url.clone(),
                user.clone(),
                password.clone(),
            )?,
        );
        let contacts_writer = Arc::new(DavContactsWriter::new(
            cfg.carddav_base_url.clone(),
            user.clone(),
            password.clone(),
        )?);

        Ok(Self {
            cfg,
            user,
            password,
            alias,
            mail_source,
            mail_writer,
            calendar_source,
            calendar_writer,
            contacts_source,
            contacts_writer,
        })
    }

    /// Alias user (basic-auth / IMAP login).
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Alias password matching [`Self::user`].
    pub fn password(&self) -> &str {
        &self.password
    }

    /// The shared CardDAV write helper (inherent create/update/delete —
    /// there is no `ContactsWriter` trait), for the MCP contacts-write path.
    pub fn contacts_writer(&self) -> Arc<DavContactsWriter> {
        self.contacts_writer.clone()
    }

    /// The configured CardDAV addressbook collection URL — the `addressbook`
    /// argument the [`DavContactsWriter`] methods take.
    pub fn carddav_collection_url(&self) -> &str {
        &self.cfg.carddav_base_url
    }

    /// The configured CalDAV calendar collection URL — the `calendar_id` the
    /// [`CalendarWriter`] methods take for this provider.
    pub fn caldav_collection_url(&self) -> &str {
        &self.cfg.caldav_base_url
    }

    /// ManageSieve endpoint for sieve script activation.
    pub fn managesieve_config(&self) -> StalwartEndpoint {
        StalwartEndpoint {
            host: self.cfg.managesieve_host.clone(),
            port: self.cfg.managesieve_port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }

    /// SMTP submission endpoint for outbound mail.
    pub fn smtp_config(&self) -> StalwartEndpoint {
        StalwartEndpoint {
            host: self.cfg.smtp_host.clone(),
            port: self.cfg.smtp_port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }

    /// `ImapConfig` for any daemon component that needs a fresh IMAP
    /// connection (e.g. an IDLE listener), mirroring `ForwardemailProvider`.
    pub fn imap_config(&self) -> ImapConfig {
        ImapConfig {
            host: self.cfg.imap_host.clone(),
            port: self.cfg.imap_port,
            user: self.user.clone(),
            password: self.password.clone(),
        }
    }
}

/// Read a required credential file, trim, and reject empty values. Same
/// shape/messages as the other providers' credential loaders.
fn read_required_credential_file(p: Option<&PathBuf>, name: &str) -> Result<String, Error> {
    let path = p.ok_or_else(|| Error::config(format!("{name} is required")))?;
    let s = std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("reading {name} ({}): {e}", path.display())))?
        .trim()
        .to_string();
    if s.is_empty() {
        return Err(Error::config(format!("{name} ({}) is empty", path.display())));
    }
    Ok(s)
}

impl Provider for StalwartProvider {
    fn name(&self) -> &'static str {
        "stalwart"
    }

    fn capabilities(&self) -> Capabilities {
        // Stalwart speaks every protocol pimsteward needs, so the surface
        // matches forwardemail's full set.
        Capabilities::forwardemail_full()
    }

    fn alias(&self) -> &str {
        &self.alias
    }

    fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error> {
        Ok(Some(self.mail_source.clone()))
    }

    fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        // Same `Arc` allocation as `build_mail_source` so the two trait
        // objects share the IMAP session.
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
    use crate::config::ProviderConfigs;

    /// Build a `Config` with a `[provider.stalwart]` block whose credential
    /// files point at throwaway temp files. No network happens — every
    /// source/writer opens its connection lazily on first use.
    fn config_with_temp_creds(dir: &tempfile::TempDir, user: &str, pass: &str) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, user).unwrap();
        std::fs::write(&p, pass).unwrap();
        Config {
            provider: ProviderConfigs {
                stalwart: Some(StalwartConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(p),
                    caldav_base_url: "https://h:8443/dav/cal/u/default".into(),
                    carddav_base_url: "https://h:8443/dav/card/u/default".into(),
                    ..StalwartConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn provider_basic_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "dan@example.test", "pw");
        let p = StalwartProvider::new(&cfg).expect("provider should build");
        assert_eq!(p.name(), "stalwart");
        assert_eq!(p.capabilities(), Capabilities::forwardemail_full());
        assert_eq!(p.alias(), "dan-example.test");
    }

    #[test]
    fn provider_full_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let p = StalwartProvider::new(&cfg).unwrap();
        let caps = p.capabilities();
        assert!(caps.mail);
        assert!(caps.calendar);
        assert!(caps.contacts);
        assert!(caps.sieve);
        assert!(caps.email_send);
    }

    #[test]
    fn all_builders_present() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let p = StalwartProvider::new(&cfg).unwrap();
        assert!(p.build_mail_source().unwrap().is_some());
        assert!(p.build_mail_writer().unwrap().is_some());
        assert!(p.build_calendar_source().unwrap().is_some());
        assert!(p.build_calendar_writer().unwrap().is_some());
        assert!(p.build_contacts_source().unwrap().is_some());
    }

    #[test]
    fn mail_source_and_writer_share_session() {
        // IMAP source + writer must be the same `Arc` allocation so they
        // share one login (see ForwardemailProvider for the rationale).
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let p = StalwartProvider::new(&cfg).unwrap();
        let s = p.build_mail_source().unwrap().unwrap();
        let w = p.build_mail_writer().unwrap().unwrap();
        let s_ptr = Arc::as_ptr(&s) as *const ();
        let w_ptr = Arc::as_ptr(&w) as *const ();
        assert_eq!(
            s_ptr, w_ptr,
            "IMAP source and writer must share one Arc/session"
        );
    }

    #[test]
    fn endpoints_carry_configured_host_port_and_creds() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let pw = dir.path().join("p");
        std::fs::write(&u, "dan@example.test").unwrap();
        std::fs::write(&pw, "secret").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                stalwart: Some(StalwartConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(pw),
                    managesieve_host: "sieve.host".into(),
                    managesieve_port: 4190,
                    smtp_host: "smtp.host".into(),
                    smtp_port: 587,
                    ..StalwartConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let p = StalwartProvider::new(&cfg).unwrap();
        let sieve = p.managesieve_config();
        assert_eq!(sieve.host, "sieve.host");
        assert_eq!(sieve.port, 4190);
        assert_eq!(sieve.user, "dan@example.test");
        assert_eq!(sieve.password, "secret");
        let smtp = p.smtp_config();
        assert_eq!(smtp.host, "smtp.host");
        assert_eq!(smtp.port, 587);
    }

    #[test]
    fn missing_user_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p");
        std::fs::write(&p, "x").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                stalwart: Some(StalwartConfig {
                    alias_user_file: None,
                    alias_password_file: Some(p),
                    ..StalwartConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let err = StalwartProvider::new(&cfg).unwrap_err();
        assert!(err.to_string().contains("alias_user_file"), "{err}");
    }

    #[test]
    fn empty_password_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "dan@example.test").unwrap();
        std::fs::write(&p, "\n").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                stalwart: Some(StalwartConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(p),
                    ..StalwartConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let err = StalwartProvider::new(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("alias_password_file"), "{msg}");
        assert!(msg.contains("empty"), "{msg}");
    }
}
