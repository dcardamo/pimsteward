//! Configuration loading.
//!
//! Layering: [`Config::default`] → TOML file → environment (prefix
//! `PIMSTEWARD_`, nested with `__`, e.g. `PIMSTEWARD_LOG_LEVEL=debug`).

use crate::error::Error;
use crate::permission::Permissions;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub forwardemail: ForwardemailConfig,

    /// Namespaced provider configs. Replaces the legacy top-level
    /// `[forwardemail]` block — exactly one variant inside this struct
    /// must be set, validated by [`Config::active_provider_kind`]. The
    /// legacy `[forwardemail]` block above is still accepted as a
    /// back-compat shim, and configs that set it count as
    /// [`ProviderKind::Forwardemail`].
    #[serde(default)]
    pub provider: ProviderConfigs,

    #[serde(default)]
    pub storage: StorageConfig,

    #[serde(default)]
    pub permissions: Permissions,

    #[serde(default)]
    pub pull: PullConfig,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Additional MCP HTTP listeners with their own bearer token and
    /// permission profile. Each entry spawns an extra HTTP listener on
    /// the configured `port`, allowing multiple MCP clients to share the
    /// same daemon (and the same underlying mail/calendar/contact data)
    /// with *different* permission matrices.
    ///
    /// Why: rockycc (AI assistant) and spamguard (mail filter) both need
    /// access to `dan@hld.ca`, but want very different capabilities.
    /// Rockycc is limited to read-only + Drafts write. Spamguard needs
    /// read_write on the whole mailbox so it can move scored messages to
    /// Spam. A single-token daemon forces both callers into the same
    /// permission matrix; profiles break that tie.
    ///
    /// The default `--bearer-token-file` + `[permissions]` combination
    /// is always served on the CLI-provided `--port`, regardless of what
    /// is configured here — profiles are strictly additive so back-compat
    /// is preserved.
    #[serde(default)]
    pub mcp_profiles: Vec<McpProfile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            forwardemail: ForwardemailConfig::default(),
            provider: ProviderConfigs::default(),
            storage: StorageConfig::default(),
            permissions: Permissions::default(),
            pull: PullConfig::default(),
            log_level: default_log_level(),
            mcp_profiles: Vec::new(),
        }
    }
}

/// One named MCP HTTP profile. Serves a separate `axum::serve` listener
/// on its own `port`, authenticated by its own `bearer_token_file`, with
/// tool calls gated by its own `permissions` matrix. Lets a single
/// pimsteward daemon mediate the same alias to multiple callers with
/// different access levels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpProfile {
    /// Profile name — used in log lines and the caller-attribution git
    /// trailer on any mutation originating from this endpoint.
    pub name: String,
    /// TCP port for this profile's MCP HTTP listener. Must differ from
    /// the CLI `--port` and from every other profile's `port`.
    pub port: u16,
    /// Path to a file containing this profile's bearer token. Required
    /// — profiles without auth are a design smell.
    pub bearer_token_file: PathBuf,
    /// Caller name recorded in git commit attribution for every write
    /// initiated through this profile. Defaults to the profile `name`.
    #[serde(default)]
    pub caller: Option<String>,
    /// Permission matrix for this profile. Independent of the top-level
    /// `[permissions]`; must be set explicitly (the default is
    /// `Permissions::default()` which denies everything).
    #[serde(default)]
    pub permissions: Permissions,
}

impl McpProfile {
    /// The caller name to attribute writes to. Falls back to the profile
    /// `name` when no explicit `caller` is set.
    pub fn caller_name(&self) -> &str {
        self.caller.as_deref().unwrap_or(&self.name)
    }
}

/// Per-resource pull intervals used by the daemon. Non-daemon subcommands
/// (probe, pull-contacts, etc.) ignore these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullConfig {
    #[serde(default = "default_contacts_interval")]
    pub contacts_interval_seconds: u64,
    #[serde(default = "default_sieve_interval")]
    pub sieve_interval_seconds: u64,
    #[serde(default = "default_calendar_interval")]
    pub calendar_interval_seconds: u64,
    #[serde(default = "default_mail_interval")]
    pub mail_interval_seconds: u64,
}

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            contacts_interval_seconds: default_contacts_interval(),
            sieve_interval_seconds: default_sieve_interval(),
            calendar_interval_seconds: default_calendar_interval(),
            mail_interval_seconds: default_mail_interval(),
        }
    }
}

fn default_contacts_interval() -> u64 {
    900 // 15 min
}
fn default_sieve_interval() -> u64 {
    3600 // 1 hour
}
fn default_calendar_interval() -> u64 {
    300 // 5 min
}
fn default_mail_interval() -> u64 {
    300 // 5 min
}

fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardemailConfig {
    #[serde(default = "default_api_base")]
    pub api_base: String,

    /// File containing the alias email (Basic auth username).
    pub alias_user_file: Option<PathBuf>,

    /// File containing the generated alias password (Basic auth password).
    pub alias_password_file: Option<PathBuf>,

    /// Which backend to use for reading mail. `rest` (default) uses the
    /// forwardemail REST API with per-message JSON fetch. `imap` uses
    /// native IMAP with `FETCH BODY[]` and CONDSTORE modseq delta hints.
    ///
    /// Writes always go through REST regardless of this setting — mixing
    /// write backends complicates audit attribution.
    ///
    /// Safe to switch between `rest` and `imap` against the same backup
    /// tree: canonical message IDs (hash of the RFC822 Message-ID header)
    /// are source-agnostic, so files keep their identity across backends.
    #[serde(default)]
    pub mail_source: MailSourceKind,

    /// Backend for reading calendar state. `rest` (default) uses the
    /// REST API. `caldav` uses native CalDAV against
    /// `caldav.forwardemail.net` — more efficient at high event counts
    /// because a single REPORT returns all events with iCal bodies and
    /// etags in one round trip.
    #[serde(default)]
    pub calendar_source: CalendarSourceKind,

    /// Backend for reading contacts. `rest` (default) or `carddav`.
    /// CardDAV is more efficient with large address books for the same
    /// reason CalDAV is for calendars.
    #[serde(default)]
    pub contacts_source: ContactsSourceKind,

    /// IMAP host — used only when `mail_source = "imap"`.
    #[serde(default = "default_imap_host")]
    pub imap_host: String,

    /// IMAP port — used only when `mail_source = "imap"`.
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,

    /// Use IMAP IDLE to push-notify the mail puller when new messages
    /// arrive, instead of only waking on the periodic `mail_interval_seconds`
    /// ticker. Only applies when `mail_source = "imap"`. When enabled, a
    /// dedicated IDLE connection runs alongside the puller — on any mailbox
    /// change the puller is signalled to run immediately, and the periodic
    /// ticker still acts as a safety net in case the IDLE connection drops.
    #[serde(default)]
    pub imap_idle: bool,

    /// CalDAV base URL (no trailing slash) — used only when
    /// `calendar_source = "caldav"`.
    #[serde(default = "default_caldav_base_url")]
    pub caldav_base_url: String,

    /// CardDAV base URL (no trailing slash) — used only when
    /// `contacts_source = "carddav"`.
    #[serde(default = "default_carddav_base_url")]
    pub carddav_base_url: String,

    /// ManageSieve host for sieve script activation. Forwardemail's REST
    /// API treats `is_active` as read-only; activation requires the
    /// ManageSieve protocol (RFC 5804).
    #[serde(default = "default_managesieve_host")]
    pub managesieve_host: String,

    /// ManageSieve port (implicit TLS).
    #[serde(default = "default_managesieve_port")]
    pub managesieve_port: u16,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailSourceKind {
    #[default]
    Rest,
    Imap,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSourceKind {
    #[default]
    Rest,
    Caldav,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactsSourceKind {
    #[default]
    Rest,
    Carddav,
}

fn default_imap_host() -> String {
    "imap.forwardemail.net".into()
}

fn default_imap_port() -> u16 {
    993
}

fn default_caldav_base_url() -> String {
    "https://caldav.forwardemail.net".into()
}

fn default_carddav_base_url() -> String {
    "https://carddav.forwardemail.net".into()
}

fn default_managesieve_host() -> String {
    "imap.forwardemail.net".into()
}

fn default_managesieve_port() -> u16 {
    4190
}

impl Default for ForwardemailConfig {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            alias_user_file: None,
            alias_password_file: None,
            mail_source: MailSourceKind::default(),
            calendar_source: CalendarSourceKind::default(),
            contacts_source: ContactsSourceKind::default(),
            imap_host: default_imap_host(),
            imap_port: default_imap_port(),
            imap_idle: false,
            caldav_base_url: default_caldav_base_url(),
            carddav_base_url: default_carddav_base_url(),
            managesieve_host: default_managesieve_host(),
            managesieve_port: default_managesieve_port(),
        }
    }
}

fn default_api_base() -> String {
    "https://api.forwardemail.net".into()
}

/// iCloud CalDAV provider configuration. Calendar-only — iCloud's other
/// resources (mail, contacts) are out of scope for pimsteward today.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IcloudCaldavConfig {
    /// CalDAV discovery root. Walks `.well-known/caldav` then PROPFINDs
    /// for the principal URL and `calendar-home-set` per RFC 6764.
    #[serde(default = "default_icloud_discovery_url")]
    pub discovery_url: String,

    /// File containing the Apple ID email (CalDAV basic-auth username).
    pub username_file: Option<PathBuf>,

    /// File containing an Apple-ID app-specific password (CalDAV basic-auth
    /// password). Generate at appleid.apple.com; rotate on suspected compromise.
    pub password_file: Option<PathBuf>,

    /// HTTP User-Agent for CalDAV requests. iCloud rejects empty UAs with 403.
    #[serde(default = "default_icloud_user_agent")]
    pub user_agent: String,
}

impl Default for IcloudCaldavConfig {
    fn default() -> Self {
        Self {
            discovery_url: default_icloud_discovery_url(),
            username_file: None,
            password_file: None,
            user_agent: default_icloud_user_agent(),
        }
    }
}

fn default_icloud_discovery_url() -> String {
    "https://caldav.icloud.com/".into()
}

fn default_icloud_user_agent() -> String {
    "pimsteward (iCloud CalDAV)".into()
}

/// Namespaced provider config holder. Exactly one variant must be set —
/// validated by [`Config::active_provider_kind`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfigs {
    pub forwardemail: Option<ForwardemailConfig>,
    pub icloud_caldav: Option<IcloudCaldavConfig>,
}

/// Which provider this config selects. Determined at startup by
/// [`Config::active_provider_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Forwardemail,
    IcloudCaldav,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_repo_path")]
    pub repo_path: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            repo_path: default_repo_path(),
        }
    }
}

fn default_repo_path() -> PathBuf {
    PathBuf::from("/var/lib/pimsteward")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Error> {
        let figment = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("PIMSTEWARD_").split("__"));
        figment.extract().map_err(|e| Error::config(e.to_string()))
    }

    /// Determine which provider this config configures. Errors if zero or
    /// more than one is set, or if both legacy and namespaced forwardemail
    /// credentials are set simultaneously (mid-migration footgun).
    ///
    /// "Set" here means: credential file paths are populated, either in
    /// the namespaced `[provider.*]` section or (for forwardemail) in the
    /// legacy top-level `[forwardemail]` shim. An empty `[provider.*]`
    /// section with no credentials is treated as "not set" so a stub
    /// section doesn't accidentally trip detection — that previously
    /// returned `Ok` here and then failed with a worse error inside
    /// `load_credentials`.
    pub fn active_provider_kind(&self) -> Result<ProviderKind, Error> {
        // Namespaced providers count only when credential files are set;
        // an empty `[provider.forwardemail]` / `[provider.icloud_caldav]`
        // header with no fields is "not configured".
        let has_namespaced_fe = self
            .provider
            .forwardemail
            .as_ref()
            .map(|fe| fe.alias_user_file.is_some() || fe.alias_password_file.is_some())
            .unwrap_or(false);
        let has_namespaced_ic = self
            .provider
            .icloud_caldav
            .as_ref()
            .map(|ic| ic.username_file.is_some() || ic.password_file.is_some())
            .unwrap_or(false);
        // Legacy top-level [forwardemail] counts only if the user actually
        // configured credentials; the all-default ForwardemailConfig from
        // `#[serde(default)]` shouldn't impersonate a real provider.
        let has_legacy_fe = self.forwardemail.alias_user_file.is_some()
            || self.forwardemail.alias_password_file.is_some();

        match (has_namespaced_fe, has_legacy_fe, has_namespaced_ic) {
            (true, true, _) => Err(Error::config(
                "config has both legacy [forwardemail] and namespaced \
                 [provider.forwardemail] credentials; pick one form (the \
                 namespaced form is preferred for new configs)",
            )),
            (true, false, true) | (false, true, true) => Err(Error::config(
                "config has both a forwardemail credential and an \
                 icloud_caldav config; pick one — pimsteward runs one \
                 provider per daemon",
            )),
            (false, false, false) => Err(Error::config(
                "no provider configured: set [provider.forwardemail] or \
                 [provider.icloud_caldav] (and provide credential files)",
            )),
            (true, false, false) | (false, true, false) => Ok(ProviderKind::Forwardemail),
            (false, false, true) => Ok(ProviderKind::IcloudCaldav),
        }
    }

    /// Effective ForwardemailConfig — namespaced [provider.forwardemail]
    /// wins over legacy top-level [forwardemail] when both present, but
    /// `active_provider_kind` will already have errored in that case.
    pub fn effective_forwardemail(&self) -> ForwardemailConfig {
        self.provider
            .forwardemail
            .clone()
            .unwrap_or_else(|| self.forwardemail.clone())
    }

    /// Read alias credentials from the configured files. Returns
    /// `(alias_user, alias_password)`. Uses [`Self::effective_forwardemail`]
    /// so a config with only `[provider.forwardemail]` (no legacy top-level
    /// block) still resolves the right file paths.
    pub fn load_credentials(&self) -> Result<(String, String), Error> {
        let fe = self.effective_forwardemail();
        let user_path = fe
            .alias_user_file
            .as_ref()
            .ok_or_else(|| Error::config("forwardemail.alias_user_file is required"))?;
        let pass_path = fe
            .alias_password_file
            .as_ref()
            .ok_or_else(|| Error::config("forwardemail.alias_password_file is required"))?;

        let user = std::fs::read_to_string(user_path)
            .map_err(|e| {
                Error::config(format!(
                    "reading alias_user_file {}: {}",
                    user_path.display(),
                    e
                ))
            })?
            .trim()
            .to_string();
        let pass = std::fs::read_to_string(pass_path)
            .map_err(|e| {
                Error::config(format!(
                    "reading alias_password_file {}: {}",
                    pass_path.display(),
                    e
                ))
            })?
            .trim()
            .to_string();

        if user.is_empty() {
            return Err(Error::config(format!(
                "alias_user_file {} is empty",
                user_path.display()
            )));
        }
        if pass.is_empty() {
            return Err(Error::config(format!(
                "alias_password_file {} is empty",
                pass_path.display()
            )));
        }
        Ok((user, pass))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::Access;

    #[test]
    fn defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("none.toml")).unwrap();
        assert_eq!(cfg.forwardemail.api_base, "https://api.forwardemail.net");
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn toml_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
log_level = "debug"

[forwardemail]
api_base = "https://test.example"
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"

[storage]
repo_path = "/tmp/repo"

[permissions]
email = "read"
calendar = "read_write"
contacts = "read_write"
sieve = "read_write"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.forwardemail.api_base, "https://test.example");
        assert_eq!(cfg.storage.repo_path, PathBuf::from("/tmp/repo"));
        assert_eq!(cfg.permissions.email.default_access(), Access::Read);
        assert_eq!(cfg.permissions.calendar.default_access(), Access::ReadWrite);
    }

    #[test]
    fn imap_idle_defaults_off_and_respects_toml() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("none.toml")).unwrap();
        assert!(!cfg.forwardemail.imap_idle);

        let p = dir.path().join("idle.toml");
        std::fs::write(
            &p,
            r#"
[forwardemail]
mail_source = "imap"
imap_idle = true

[storage]
repo_path = "/tmp/repo"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.forwardemail.imap_idle);
    }

    #[test]
    fn load_credentials_reads_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("user");
        let p = dir.path().join("pass");
        std::fs::write(&u, "alice@example.com\n").unwrap();
        std::fs::write(&p, "secret123\n").unwrap();

        let cfg = Config {
            forwardemail: ForwardemailConfig {
                api_base: "https://x".into(),
                alias_user_file: Some(u),
                alias_password_file: Some(p),
                ..ForwardemailConfig::default()
            },
            ..Config::default()
        };
        let (user, pass) = cfg.load_credentials().unwrap();
        assert_eq!(user, "alice@example.com");
        assert_eq!(pass, "secret123");
    }

    #[test]
    fn mcp_profiles_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&dir.path().join("none.toml")).unwrap();
        assert!(cfg.mcp_profiles.is_empty());
    }

    #[test]
    fn mcp_profiles_parse_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"

[storage]
repo_path = "/tmp/repo"

# Top-level (default) permissions — used by the primary MCP listener on
# the CLI --port. Restricted, so read-only for rockycc.
[permissions]
email_send = "denied"
calendar   = "read_write"

[permissions.email]
default = "read"
[permissions.email.folders]
Drafts = "read_write"

# Profile used by spamguard — needs full mailbox write to move spam.
[[mcp_profiles]]
name = "spamguard"
port = 8102
bearer_token_file = "/run/secrets/spamguard-token"
caller = "spamguard"

[mcp_profiles.permissions]
email_send = "denied"

[mcp_profiles.permissions.email]
default = "read_write"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();

        // Default top-level permissions unchanged — rockycc still read-only.
        assert_eq!(cfg.permissions.email.default_access(), Access::Read);
        assert_eq!(
            cfg.permissions.email.for_folder(Some("Drafts")),
            Access::ReadWrite
        );
        assert_eq!(
            cfg.permissions.email.for_folder(Some("INBOX")),
            Access::Read,
            "rockycc must NOT be granted write on INBOX",
        );

        // Profile is additive.
        assert_eq!(cfg.mcp_profiles.len(), 1);
        let p = &cfg.mcp_profiles[0];
        assert_eq!(p.name, "spamguard");
        assert_eq!(p.port, 8102);
        assert_eq!(
            p.bearer_token_file,
            PathBuf::from("/run/secrets/spamguard-token")
        );
        assert_eq!(p.caller_name(), "spamguard");
        assert_eq!(p.permissions.email.default_access(), Access::ReadWrite);
        // Confirms the two permission matrices are independent.
        assert_eq!(
            p.permissions.email.for_folder(Some("INBOX")),
            Access::ReadWrite,
            "spamguard profile must grant INBOX write",
        );
    }

    #[test]
    fn mcp_profile_caller_defaults_to_name() {
        // caller = None → caller_name() returns the profile name so git
        // trailers are still attributed to something meaningful.
        let p = McpProfile {
            name: "spamguard".into(),
            port: 8102,
            bearer_token_file: PathBuf::from("/tmp/t"),
            caller: None,
            permissions: Permissions::default(),
        };
        assert_eq!(p.caller_name(), "spamguard");

        let p = McpProfile {
            caller: Some("filter-bot".into()),
            ..p
        };
        assert_eq!(p.caller_name(), "filter-bot");
    }

    #[test]
    fn provider_kind_forwardemail_legacy_top_level() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(
            cfg.active_provider_kind().unwrap(),
            ProviderKind::Forwardemail
        );
    }

    #[test]
    fn provider_kind_forwardemail_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[provider.forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(
            cfg.active_provider_kind().unwrap(),
            ProviderKind::Forwardemail
        );
    }

    #[test]
    fn provider_kind_icloud_namespaced() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[provider.icloud_caldav]
username_file = "/tmp/u"
password_file = "/tmp/p"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(
            cfg.active_provider_kind().unwrap(),
            ProviderKind::IcloudCaldav
        );
    }

    #[test]
    fn provider_kind_both_set_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[forwardemail]
alias_user_file = "/tmp/u"
alias_password_file = "/tmp/p"

[provider.icloud_caldav]
username_file = "/tmp/u"
password_file = "/tmp/p"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        let err = cfg.active_provider_kind().unwrap_err();
        assert!(err.to_string().contains("both"), "{}", err);
    }

    #[test]
    fn provider_kind_none_set_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, r#"log_level = "info""#).unwrap();
        let cfg = Config::load(&p).unwrap();
        let err = cfg.active_provider_kind().unwrap_err();
        assert!(err.to_string().contains("no provider"), "{}", err);
    }

    #[test]
    fn provider_kind_empty_namespaced_fe_section_is_unconfigured() {
        // An empty `[provider.forwardemail]` header with no fields used to
        // count as "configured" and led to a confusing error inside
        // load_credentials. Now it correctly reports "no provider".
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[provider.forwardemail]
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        let err = cfg.active_provider_kind().unwrap_err();
        assert!(err.to_string().contains("no provider"), "{}", err);
    }

    #[test]
    fn provider_kind_empty_namespaced_icloud_section_is_unconfigured() {
        // Symmetric guard for the icloud_caldav side.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[provider.icloud_caldav]
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        let err = cfg.active_provider_kind().unwrap_err();
        assert!(err.to_string().contains("no provider"), "{}", err);
    }

    #[test]
    fn provider_kind_legacy_and_namespaced_forwardemail_errors() {
        // Mid-migration footgun: both legacy [forwardemail] and namespaced
        // [provider.forwardemail] credentials set. We refuse to guess
        // which one wins — error and tell the user to pick.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            r#"
[forwardemail]
alias_user_file = "/tmp/u-legacy"
alias_password_file = "/tmp/p-legacy"

[provider.forwardemail]
alias_user_file = "/tmp/u-ns"
alias_password_file = "/tmp/p-ns"
"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        let err = cfg.active_provider_kind().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("both legacy"), "{}", msg);
        assert!(msg.contains("namespaced"), "{}", msg);
    }

    #[test]
    fn effective_forwardemail_prefers_namespaced() {
        // Note: in production active_provider_kind would error on this dual
        // config. The function is called on ProviderKind::Forwardemail paths
        // only — but assert the precedence anyway as a regression catch.
        let cfg = Config {
            forwardemail: ForwardemailConfig {
                api_base: "https://legacy.example".into(),
                ..ForwardemailConfig::default()
            },
            provider: ProviderConfigs {
                forwardemail: Some(ForwardemailConfig {
                    api_base: "https://namespaced.example".into(),
                    ..ForwardemailConfig::default()
                }),
                ..ProviderConfigs::default()
            },
            ..Config::default()
        };
        assert_eq!(
            cfg.effective_forwardemail().api_base,
            "https://namespaced.example"
        );
    }

    #[test]
    fn example_icloud_config_parses() {
        // The shipped example must always parse cleanly so users can copy
        // it as a starting point. Asserts the provider selection and the
        // default permission shape so a future edit that silently changes
        // either is caught here.
        let cfg = Config::load(std::path::Path::new("examples/config-icloud-caldav.toml"))
            .expect("example iCloud config should parse");
        assert_eq!(
            cfg.active_provider_kind().unwrap(),
            ProviderKind::IcloudCaldav,
            "example must declare iCloud as the active provider",
        );
        assert_eq!(
            cfg.permissions.calendar.default_access(),
            Access::Read,
            "example permissions are flat-read by default; if this changes, update the comment in the example",
        );
    }

    #[test]
    fn load_credentials_empty_errors() {
        let dir = tempfile::tempdir().unwrap();
        let u = dir.path().join("user");
        let p = dir.path().join("pass");
        std::fs::write(&u, "").unwrap();
        std::fs::write(&p, "secret").unwrap();
        let cfg = Config {
            forwardemail: ForwardemailConfig {
                api_base: "https://x".into(),
                alias_user_file: Some(u),
                alias_password_file: Some(p),
                ..ForwardemailConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg
            .load_credentials()
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }
}
