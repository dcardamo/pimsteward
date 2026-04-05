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

    #[serde(default)]
    pub storage: StorageConfig,

    #[serde(default)]
    pub permissions: Permissions,

    #[serde(default)]
    pub pull: PullConfig,

    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            forwardemail: ForwardemailConfig::default(),
            storage: StorageConfig::default(),
            permissions: Permissions::default(),
            pull: PullConfig::default(),
            log_level: default_log_level(),
        }
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
}

impl Default for ForwardemailConfig {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            alias_user_file: None,
            alias_password_file: None,
        }
    }
}

fn default_api_base() -> String {
    "https://api.forwardemail.net".into()
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

    /// Read alias credentials from the configured files. Returns
    /// `(alias_user, alias_password)`.
    pub fn load_credentials(&self) -> Result<(String, String), Error> {
        let user_path = self
            .forwardemail
            .alias_user_file
            .as_ref()
            .ok_or_else(|| Error::config("forwardemail.alias_user_file is required"))?;
        let pass_path = self
            .forwardemail
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
        assert_eq!(cfg.permissions.email, Access::Read);
        assert_eq!(cfg.permissions.calendar, Access::ReadWrite);
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
            },
            ..Config::default()
        };
        let (user, pass) = cfg.load_credentials().unwrap();
        assert_eq!(user, "alice@example.com");
        assert_eq!(pass, "secret123");
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
