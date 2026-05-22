//! iCloud CalDAV provider. Calendar-only — the rest of pimsteward's
//! resource surface (mail, contacts, sieve, send) is intentionally out of
//! scope for iCloud and the corresponding `build_*` methods return
//! `Ok(None)`. The MCP layer translates `None` into a structured
//! "not supported by this provider" error at runtime.
//!
//! Credentials are loaded from the file paths in the
//! `[provider.icloud_caldav]` config block (`username_file` /
//! `password_file`); both are required and must be non-empty after a
//! whitespace trim.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use crate::error::Error;
use crate::icloud::caldav::{IcloudCalendarSource, IcloudCalendarWriter};
use crate::provider::{Capabilities, Provider};
use crate::source::{CalendarSource, CalendarWriter, ContactsSource, MailSource, MailWriter};

/// Provider impl for iCloud CalDAV. Reads username + app-specific password
/// from the configured files and pre-builds a shared `IcloudCalendarSource`
/// (which holds the discovery cache) plus an `IcloudCalendarWriter` so
/// subsequent `build_*` calls return clones of the same `Arc`s — matching
/// the cache-once invariant `ForwardemailProvider` relies on.
pub struct IcloudCaldavProvider {
    /// Apple ID (CalDAV basic-auth username). Stored for diagnostics; the
    /// active source/writer have their own copies.
    user: String,
    /// `user` with `@` replaced by `-` so it's safe to embed in
    /// filesystem paths and git ref names. This is the alias used
    /// throughout the rest of pimsteward (storage layout, audit attribution).
    alias: String,
    calendar_source: Arc<dyn CalendarSource>,
    calendar_writer: Arc<dyn CalendarWriter>,
}

impl std::fmt::Debug for IcloudCaldavProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IcloudCaldavProvider")
            .field("user", &self.user)
            .field("alias", &self.alias)
            .finish_non_exhaustive()
    }
}

impl IcloudCaldavProvider {
    pub fn new(top_cfg: &Config) -> Result<Self, Error> {
        let cfg = top_cfg
            .provider
            .icloud_caldav
            .as_ref()
            .ok_or_else(|| Error::config("[provider.icloud_caldav] not configured"))?;
        let user = read_required_credential_file(
            cfg.username_file.as_ref(),
            "provider.icloud_caldav.username_file",
        )?;
        let password = read_required_credential_file(
            cfg.password_file.as_ref(),
            "provider.icloud_caldav.password_file",
        )?;
        let alias = user.replace('@', "-");

        let source = Arc::new(IcloudCalendarSource::new(
            cfg.discovery_url.clone(),
            cfg.user_agent.clone(),
            user.clone(),
            password.clone(),
        )?);
        let writer = Arc::new(IcloudCalendarWriter::new(
            cfg.discovery_url.clone(),
            cfg.user_agent.clone(),
            user.clone(),
            password.clone(),
        )?);

        Ok(Self {
            user,
            alias,
            calendar_source: source,
            calendar_writer: writer,
        })
    }

}

/// Read a required credential file, trim trailing whitespace, and reject
/// empty values. Identical shape to `Config::load_credentials` so the
/// error messages line up across providers.
fn read_required_credential_file(p: Option<&PathBuf>, name: &str) -> Result<String, Error> {
    let path = p.ok_or_else(|| Error::config(format!("{name} is required")))?;
    let s = std::fs::read_to_string(path)
        .map_err(|e| Error::config(format!("reading {name} ({}): {e}", path.display())))?
        .trim()
        .to_string();
    if s.is_empty() {
        return Err(Error::config(format!(
            "{name} ({}) is empty",
            path.display()
        )));
    }
    Ok(s)
}

impl Provider for IcloudCaldavProvider {
    fn name(&self) -> &'static str {
        "icloud_caldav"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::calendar_only()
    }

    fn alias(&self) -> &str {
        &self.alias
    }

    fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error> {
        Ok(None)
    }

    fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error> {
        Ok(None)
    }

    fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error> {
        Ok(Some(self.calendar_source.clone()))
    }

    fn build_calendar_writer(&self) -> Result<Option<Arc<dyn CalendarWriter>>, Error> {
        Ok(Some(self.calendar_writer.clone()))
    }

    fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IcloudCaldavConfig, ProviderConfigs};

    fn config_with_temp_creds(dir: &tempfile::TempDir, user: &str, pass: &str) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, user).unwrap();
        std::fs::write(&p, pass).unwrap();
        Config {
            provider: ProviderConfigs {
                icloud_caldav: Some(IcloudCaldavConfig {
                    username_file: Some(u),
                    password_file: Some(p),
                    ..IcloudCaldavConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn provider_basic_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "alice@icloud.com", "app-spec-pass");
        let provider = IcloudCaldavProvider::new(&cfg).expect("provider should build");
        assert_eq!(provider.name(), "icloud_caldav");
        assert_eq!(provider.capabilities(), Capabilities::calendar_only());
        // `@` becomes `-` so the alias is filesystem-safe.
        assert_eq!(provider.alias(), "alice-icloud.com");
    }

    #[test]
    fn provider_calendar_only_capability() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let provider = IcloudCaldavProvider::new(&cfg).unwrap();
        let caps = provider.capabilities();
        assert!(caps.calendar);
        assert!(!caps.mail);
        assert!(!caps.contacts);
        assert!(!caps.sieve);
        assert!(!caps.email_send);
    }

    #[test]
    fn build_mail_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let provider = IcloudCaldavProvider::new(&cfg).unwrap();
        assert!(provider.build_mail_source().unwrap().is_none());
        assert!(provider.build_mail_writer().unwrap().is_none());
        assert!(provider.build_contacts_source().unwrap().is_none());
    }

    #[test]
    fn build_calendar_source_and_writer_present() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with_temp_creds(&dir, "u@example.com", "p");
        let provider = IcloudCaldavProvider::new(&cfg).unwrap();
        assert!(provider.build_calendar_source().unwrap().is_some());
        assert!(provider.build_calendar_writer().unwrap().is_some());
    }

    #[test]
    fn missing_username_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p");
        std::fs::write(&p, "x").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                icloud_caldav: Some(IcloudCaldavConfig {
                    username_file: None,
                    password_file: Some(p),
                    ..IcloudCaldavConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let err = IcloudCaldavProvider::new(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("username_file"),
            "{err}"
        );
    }

    #[test]
    fn empty_password_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@icloud.com").unwrap();
        // Just a newline — trims to empty.
        std::fs::write(&p, "\n").unwrap();
        let cfg = Config {
            provider: ProviderConfigs {
                icloud_caldav: Some(IcloudCaldavConfig {
                    username_file: Some(u),
                    password_file: Some(p),
                    ..IcloudCaldavConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        let err = IcloudCaldavProvider::new(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("password_file"), "{msg}");
        assert!(msg.contains("empty"), "{msg}");
    }
}
