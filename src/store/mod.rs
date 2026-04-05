//! Git-backed time-series store for pimsteward.
//!
//! v1 deliberately does NOT use gix's in-memory tree API for writes — that's
//! a nice optimisation to do later. Instead we treat the repo as a working
//! directory: write files with `std::fs`, then shell out to `git` via the
//! `std::process::Command` path provided by gix's discovered git binary.
//! Simpler to get right, and git's own binary is the authoritative
//! committer for now.
//!
//! This trades a little startup cost per commit for a lot of obvious-ness
//! in the write path. When pimsteward is humming and we want to commit every
//! 5 minutes, we'll revisit.

use crate::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Open an existing repo at `root`. Initializes one if it doesn't exist
    /// (with `git init -b main`).
    pub fn open_or_init(root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .map_err(|e| Error::store(format!("mkdir {}: {}", root.display(), e)))?;
        let dot_git = root.join(".git");
        if !dot_git.exists() {
            let status = Command::new("git")
                .arg("init")
                .arg("-q")
                .arg("-b")
                .arg("main")
                .current_dir(&root)
                .status()
                .map_err(|e| Error::store(format!("git init: {}", e)))?;
            if !status.success() {
                return Err(Error::store(format!("git init failed: {status}")));
            }
            // Set a default identity if not configured, so commits always succeed.
            // This is local to the repo, not global.
            let _ = Command::new("git")
                .args(["config", "user.email", "pimsteward@localhost"])
                .current_dir(&root)
                .status();
            let _ = Command::new("git")
                .args(["config", "user.name", "pimsteward"])
                .current_dir(&root)
                .status();
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write a file at `rel` (relative to repo root), creating parent dirs.
    /// Content is a byte slice. Returns the absolute path.
    pub fn write_file(&self, rel: impl AsRef<Path>, content: &[u8]) -> Result<PathBuf, Error> {
        let abs = self.root.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::store(format!("mkdir {}: {}", parent.display(), e)))?;
        }
        std::fs::write(&abs, content)
            .map_err(|e| Error::store(format!("write {}: {}", abs.display(), e)))?;
        Ok(abs)
    }

    /// Read a file at `rel` relative to repo root.
    pub fn read_file(&self, rel: impl AsRef<Path>) -> Result<Vec<u8>, Error> {
        let abs = self.root.join(rel);
        std::fs::read(&abs).map_err(|e| Error::store(format!("read {}: {}", abs.display(), e)))
    }

    /// True if the working tree has uncommitted changes.
    pub fn has_changes(&self) -> Result<bool, Error> {
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&self.root)
            .output()
            .map_err(|e| Error::store(format!("git status: {}", e)))?;
        if !out.status.success() {
            return Err(Error::store(format!(
                "git status failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(!out.stdout.is_empty())
    }

    /// Stage all changes and commit with the given author + message. No-op
    /// (returns `Ok(None)`) if there are no changes. Returns the new commit
    /// SHA on success.
    pub fn commit_all(
        &self,
        author_name: &str,
        author_email: &str,
        message: &str,
    ) -> Result<Option<String>, Error> {
        if !self.has_changes()? {
            return Ok(None);
        }
        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.root)
            .status()
            .map_err(|e| Error::store(format!("git add: {}", e)))?;
        if !add.success() {
            return Err(Error::store("git add failed"));
        }

        let commit = Command::new("git")
            .args(["commit", "-q", "--allow-empty-message", "-m", message])
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", author_name)
            .env("GIT_COMMITTER_EMAIL", author_email)
            .current_dir(&self.root)
            .status()
            .map_err(|e| Error::store(format!("git commit: {}", e)))?;
        if !commit.success() {
            return Err(Error::store("git commit failed"));
        }

        let sha = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.root)
            .output()
            .map_err(|e| Error::store(format!("git rev-parse: {}", e)))?;
        Ok(Some(
            String::from_utf8_lossy(&sha.stdout).trim().to_string(),
        ))
    }

    /// Make an empty commit. Used by the write path to record an attributed
    /// audit entry even when the resource state on disk is identical to
    /// what's already committed (e.g. because an earlier pull cycle already
    /// captured the new state).
    pub fn empty_commit(
        &self,
        author_name: &str,
        author_email: &str,
        message: &str,
    ) -> Result<String, Error> {
        let status = Command::new("git")
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .env("GIT_AUTHOR_NAME", author_name)
            .env("GIT_AUTHOR_EMAIL", author_email)
            .env("GIT_COMMITTER_NAME", author_name)
            .env("GIT_COMMITTER_EMAIL", author_email)
            .current_dir(&self.root)
            .status()
            .map_err(|e| Error::store(format!("git commit --allow-empty: {}", e)))?;
        if !status.success() {
            return Err(Error::store("git empty commit failed"));
        }
        let sha = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.root)
            .output()
            .map_err(|e| Error::store(format!("git rev-parse: {}", e)))?;
        Ok(String::from_utf8_lossy(&sha.stdout).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_open_write_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        assert!(dir.path().join(".git").exists());

        repo.write_file("a/b.txt", b"hello").unwrap();
        let read_back = repo.read_file("a/b.txt").unwrap();
        assert_eq!(read_back, b"hello");

        assert!(repo.has_changes().unwrap());
        let sha = repo
            .commit_all("test", "test@example.com", "first commit")
            .unwrap();
        assert!(sha.is_some());
        assert_eq!(sha.as_ref().unwrap().len(), 40);

        // No changes now
        assert!(!repo.has_changes().unwrap());

        // Commit-all with no changes returns None, not an error
        let sha2 = repo
            .commit_all("test", "test@example.com", "should be noop")
            .unwrap();
        assert_eq!(sha2, None);
    }

    #[test]
    fn reopen_existing_repo() {
        let dir = tempfile::tempdir().unwrap();
        let _ = Repo::open_or_init(dir.path()).unwrap();
        let r = Repo::open_or_init(dir.path()).unwrap();
        assert_eq!(r.root(), dir.path());
    }
}
