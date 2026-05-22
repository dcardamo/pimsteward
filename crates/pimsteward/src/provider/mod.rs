//! Provider abstraction. A "provider" is the bundle of (capabilities,
//! sources, writers, MCP tool list, credentials) that pimsteward uses to
//! talk to one specific upstream service. Exactly one provider is active
//! per daemon — selected at startup by which `[provider.*]` section is
//! present in config.
//!
//! Provider trait — concrete impls in [`forwardemail::ForwardemailProvider`]
//! and [`icloud_caldav::IcloudCaldavProvider`]. The daemon and MCP server
//! dispatch through `&dyn Provider`; capability-gated tools that the active
//! provider doesn't support return a structured "unsupported by provider"
//! error at call time (see `mcp/server.rs::unsupported_by_provider`).

use std::sync::Arc;

use crate::error::Error;
use crate::source::{CalendarSource, CalendarWriter, ContactsSource, MailSource, MailWriter};

pub mod forwardemail;
pub mod icloud_caldav;

/// Resource axes a provider may support. Distinct from
/// [`crate::permission::Resource`] — that enum gates user-granted access on
/// the existing forwardemail-shaped resources, while this enum describes
/// the capability surface a provider exposes (notably splitting `Mail`
/// fetch/store from `EmailSend`, which permissions also treat as separate
/// but encode out-of-band).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Mail,
    Calendar,
    Contacts,
    Sieve,
    EmailSend,
}

impl Resource {
    /// All resource variants in declaration order. Used by capability
    /// helpers and tests that need to walk every axis.
    pub fn all() -> &'static [Resource] {
        &[
            Resource::Mail,
            Resource::Calendar,
            Resource::Contacts,
            Resource::Sieve,
            Resource::EmailSend,
        ]
    }
}

/// Capability flags advertised by a provider. The daemon checks these
/// before dispatching to a `build_*` method — a provider that returns
/// `false` for a resource is allowed to also return `Ok(None)` from the
/// matching builder, but the daemon should never reach that path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub mail: bool,
    pub calendar: bool,
    pub contacts: bool,
    pub sieve: bool,
    pub email_send: bool,
}

impl Capabilities {
    /// True if this capability set advertises support for `r`.
    pub fn supports(&self, r: Resource) -> bool {
        match r {
            Resource::Mail => self.mail,
            Resource::Calendar => self.calendar,
            Resource::Contacts => self.contacts,
            Resource::Sieve => self.sieve,
            Resource::EmailSend => self.email_send,
        }
    }

    /// Calendar-only provider (e.g. iCloud CalDAV).
    pub fn calendar_only() -> Self {
        Self { calendar: true, ..Self::default() }
    }

    /// Today's forwardemail provider — every resource supported.
    pub fn forwardemail_full() -> Self {
        Self {
            mail: true,
            calendar: true,
            contacts: true,
            sieve: true,
            email_send: true,
        }
    }
}

/// One concrete upstream provider. Implementations bundle the capability
/// set, the source/writer constructors, and the per-provider configuration
/// glue. The `build_*` methods return `Option` so that providers which
/// don't support a resource can return `Ok(None)` — the daemon is expected
/// to consult [`Provider::capabilities`] first, but the `Option` makes the
/// invariant typecheck-able rather than panic-checkable.
///
/// `build_*` is sync on purpose: the MCP `StreamableHttpService` factory
/// closure that calls these methods runs on a tokio worker thread but is
/// itself non-async, so an `async fn` here would force every caller into
/// `block_on`, which can deadlock. All of pimsteward's source/writer
/// constructors (`RestMailSource::new`, `ImapMailSource::new`,
/// `DavCalendarSource::new`, etc.) are already synchronous — they only
/// open the network when the trait methods are first awaited — so the
/// constraint costs nothing.
pub trait Provider: Send + Sync {
    /// Stable, lowercase identifier — `"forwardemail"`, `"icloud"`, etc.
    /// Used in logs and metric labels, not user-facing.
    fn name(&self) -> &'static str;
    /// Capability flags for this provider instance.
    fn capabilities(&self) -> Capabilities;
    /// The configured account alias (e.g. `"dan"` or an Apple ID). Used
    /// by audit attribution and the git store layout.
    fn alias(&self) -> &str;

    fn build_mail_source(&self) -> Result<Option<Arc<dyn MailSource>>, Error>;
    fn build_mail_writer(&self) -> Result<Option<Arc<dyn MailWriter>>, Error>;
    fn build_calendar_source(&self) -> Result<Option<Arc<dyn CalendarSource>>, Error>;
    fn build_calendar_writer(&self) -> Result<Option<Arc<dyn CalendarWriter>>, Error>;
    fn build_contacts_source(&self) -> Result<Option<Arc<dyn ContactsSource>>, Error>;
}

// Construction lives in `daemon::build_provider_handles`. The previous
// `provider::build` helper was deleted as dead code (Task 6 review minor):
// only this file's own tests called it, and the daemon needs both the
// typed `Arc<ForwardemailProvider>` and the `Arc<dyn Provider>` handles
// — a single-arm helper here couldn't return both without down-casting.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn default_capabilities_all_false() {
        let c = Capabilities::default();
        for r in Resource::all() {
            assert!(!c.supports(*r), "{:?} should be unsupported by default", r);
        }
    }

    #[test]
    fn calendar_only_supports_only_calendar() {
        let c = Capabilities::calendar_only();
        for r in Resource::all() {
            let expected = matches!(r, Resource::Calendar);
            assert_eq!(c.supports(*r), expected, "{:?}: expected {}", r, expected);
        }
    }

    #[test]
    fn forwardemail_full_supports_all() {
        let c = Capabilities::forwardemail_full();
        for r in Resource::all() {
            assert!(c.supports(*r), "{:?} should be supported", r);
        }
    }

    /// Helper: write throwaway credentials and return a Config that
    /// configures `[provider.forwardemail]` with those file paths.
    fn forwardemail_config_with_temp_creds(dir: &tempfile::TempDir) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@example.com").unwrap();
        std::fs::write(&p, "passw0rd").unwrap();
        Config {
            provider: crate::config::ProviderConfigs {
                forwardemail: Some(crate::config::ForwardemailConfig {
                    alias_user_file: Some(u),
                    alias_password_file: Some(p),
                    ..crate::config::ForwardemailConfig::default()
                }),
                ..crate::config::ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    fn icloud_config_with_temp_creds(dir: &tempfile::TempDir) -> Config {
        let u = dir.path().join("u");
        let p = dir.path().join("p");
        std::fs::write(&u, "alice@icloud.com").unwrap();
        std::fs::write(&p, "app-spec-pass").unwrap();
        Config {
            provider: crate::config::ProviderConfigs {
                icloud_caldav: Some(crate::config::IcloudCaldavConfig {
                    username_file: Some(u),
                    password_file: Some(p),
                    ..crate::config::IcloudCaldavConfig::default()
                }),
                ..crate::config::ProviderConfigs::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn forwardemail_provider_constructs_with_full_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = forwardemail_config_with_temp_creds(&dir);
        let provider = forwardemail::ForwardemailProvider::new(&cfg)
            .expect("forwardemail provider should build");
        assert_eq!(provider.name(), "forwardemail");
        assert_eq!(provider.capabilities(), Capabilities::forwardemail_full());
    }

    #[test]
    fn icloud_provider_constructs_with_calendar_only_capability() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = icloud_config_with_temp_creds(&dir);
        let provider = icloud_caldav::IcloudCaldavProvider::new(&cfg)
            .expect("iCloud provider should build");
        assert_eq!(provider.name(), "icloud_caldav");
        assert_eq!(provider.capabilities(), Capabilities::calendar_only());
    }

    /// iCloud provider must return `Some` for calendar source/writer and
    /// `None` for mail/contacts. This is the contract the MCP factory's
    /// require_*/unwrap-or-error gates rely on — flipping any of these
    /// would either crash the daemon or silently expose unsupported tools.
    #[test]
    fn icloud_provider_calendar_present_mail_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = icloud_config_with_temp_creds(&dir);
        let provider = icloud_caldav::IcloudCaldavProvider::new(&cfg).unwrap();
        assert!(provider.build_calendar_source().unwrap().is_some());
        assert!(provider.build_calendar_writer().unwrap().is_some());
        assert!(provider.build_mail_source().unwrap().is_none());
        assert!(provider.build_mail_writer().unwrap().is_none());
        assert!(provider.build_contacts_source().unwrap().is_none());
    }
}
