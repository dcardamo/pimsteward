//! Shared e2e test helpers. Included by each e2e test file with
//! `#[path = "common/mod.rs"] mod common;`.
//!
//! **Every e2e test must construct its client via [`E2eContext::from_env`].**
//! That function calls the safety guards in [`pimsteward::safety`] before
//! returning anything. No direct `Client::new` calls in test code.

#![allow(dead_code)] // not every test uses every helper

use pimsteward::forwardemail::Client;
use pimsteward::source::{
    CalendarSource, CalendarWriter, RestCalendarSource, RestCalendarWriter,
};
use pimsteward::store::Repo;
use std::path::PathBuf;
use std::sync::Arc;

pub struct E2eContext {
    pub client: Client,
    pub repo: Repo,
    pub alias: String,
    /// Tempdir holding the git repo — dropped at the end of the test.
    pub _repo_dir: tempfile::TempDir,
}

impl E2eContext {
    /// Build an e2e context from environment variables with safety guards.
    ///
    /// Panics on any of:
    /// - `PIMSTEWARD_RUN_E2E` not set to `1`
    /// - credential files missing / empty
    /// - alias not containing `_test` (via [`pimsteward::safety::assert_test_alias`])
    /// - repo path under a production directory (via
    ///   [`pimsteward::safety::assert_test_environment`])
    pub fn from_env() -> Self {
        if std::env::var("PIMSTEWARD_RUN_E2E").ok().as_deref() != Some("1") {
            panic!(
                "e2e tests require PIMSTEWARD_RUN_E2E=1. These tests hit the real \
                 forwardemail API — refusing to run without explicit opt-in."
            );
        }

        let user_file = env_path(
            "PIMSTEWARD_TEST_ALIAS_USER_FILE",
            "/home/dan/.config/secrets/pimsteward-test-alias-user",
        );
        let pass_file = env_path(
            "PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE",
            "/home/dan/.config/secrets/pimsteward-test-alias-password",
        );

        let alias = std::fs::read_to_string(&user_file)
            .unwrap_or_else(|e| panic!("reading {user_file:?}: {e}"))
            .trim()
            .to_string();
        let password = std::fs::read_to_string(&pass_file)
            .unwrap_or_else(|e| panic!("reading {pass_file:?}: {e}"))
            .trim()
            .to_string();

        // SAFETY GUARD — no workaround.
        pimsteward::safety::assert_test_alias(&alias);

        let repo_dir = tempfile::tempdir().expect("creating temp repo dir");
        pimsteward::safety::assert_test_environment(&alias, repo_dir.path());

        let api_base = std::env::var("PIMSTEWARD_TEST_API_BASE")
            .unwrap_or_else(|_| "https://api.forwardemail.net".to_string());
        let client =
            Client::new(api_base, alias.clone(), password).expect("building reqwest client");
        let repo = Repo::open_or_init(repo_dir.path()).expect("init test repo");

        Self {
            client,
            repo,
            alias,
            _repo_dir: repo_dir,
        }
    }

    /// Alias with '@' replaced by '-', used as the path segment in the
    /// backup tree.
    pub fn alias_slug(&self) -> String {
        self.alias.replace('@', "-")
    }

    /// Standard attribution for e2e writes.
    pub fn attribution(&self, reason: &str) -> pimsteward::write::audit::Attribution {
        pimsteward::write::audit::Attribution::new("e2e-test", Some(reason.to_string()))
    }

    /// REST-backed `CalendarSource` over the test alias's client. Used by
    /// e2e tests that exercise the trait-based calendar plumbing.
    pub fn calendar_source(&self) -> Arc<dyn CalendarSource> {
        Arc::new(RestCalendarSource::new(self.client.clone()))
    }

    /// REST-backed `CalendarWriter` over the test alias's client.
    pub fn calendar_writer(&self) -> Arc<dyn CalendarWriter> {
        Arc::new(RestCalendarWriter::new(self.client.clone()))
    }

    /// ManageSieve config for the test alias. Reads the same credentials
    /// as the REST client and uses forwardemail's standard ManageSieve
    /// host/port (imap.forwardemail.net:4190).
    pub fn managesieve(&self) -> pimsteward::mcp::ManageSieveConfig {
        let pass_file = env_path(
            "PIMSTEWARD_TEST_ALIAS_PASSWORD_FILE",
            "/home/dan/.config/secrets/pimsteward-test-alias-password",
        );
        let password = std::fs::read_to_string(&pass_file)
            .unwrap_or_else(|e| panic!("reading {pass_file:?}: {e}"))
            .trim()
            .to_string();
        pimsteward::mcp::ManageSieveConfig {
            host: "imap.forwardemail.net".to_string(),
            port: 4190,
            user: self.alias.clone(),
            password,
        }
    }
}

fn env_path(key: &str, default: &str) -> PathBuf {
    std::env::var(key)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(default))
}
