//! Pull loops — one module per resource type. Each `pull_<resource>` fn is a
//! pure async function that reads from a [`Client`], diffs against the
//! [`Repo`] contents, writes updated files, and commits.

pub mod calendar;
pub mod contacts;
pub mod mail;
pub mod sieve;

use crate::error::Error;

/// Summary of a single pull-loop run, returned so callers can log or alert.
#[derive(Debug, Clone, Default)]
pub struct PullSummary {
    pub resource: &'static str,
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
    pub commit_sha: Option<String>,
}

impl PullSummary {
    pub fn is_noop(&self) -> bool {
        self.added == 0 && self.updated == 0 && self.deleted == 0
    }
}

impl std::fmt::Display for PullSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: +{} ~{} -{}",
            self.resource, self.added, self.updated, self.deleted
        )?;
        if let Some(sha) = &self.commit_sha {
            write!(f, " ({})", &sha[..sha.len().min(8)])?;
        } else if self.is_noop() {
            f.write_str(" (no changes)")?;
        }
        Ok(())
    }
}

// Re-expose a tiny helper so feature modules don't each import std::collections
pub(crate) fn filename_safe(s: &str) -> String {
    // Git path-safe: strip slashes/null/cr/lf, leave everything else. UIDs
    // from forwardemail are Mongo ObjectIds or UUIDs, so this is paranoia.
    s.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | '\n' | '\r' => '_',
            c => c,
        })
        .collect()
}

// Re-exported type hints so downstream modules don't need to import Error
pub(crate) type PullResult<T> = Result<T, Error>;
