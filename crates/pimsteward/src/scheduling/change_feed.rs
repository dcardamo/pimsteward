//! Turn a pull commit into a list of event-file changes by diffing it
//! against its parent with the `git` CLI (same backend as `Repo`).

use crate::error::Error;
use crate::scheduling::model::{ChangeKind, EventChange};
use crate::store::Repo;
use std::process::Command;

fn run_git(repo: &Repo, args: &[&str]) -> Result<String, Error> {
    let out = Command::new("git")
        .current_dir(repo.root())
        .args(args)
        .output()
        .map_err(|e| Error::config(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(Error::config(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read a blob at `<rev>:<path>`; returns None if the path didn't exist there.
fn blob(repo: &Repo, rev: &str, path: &str) -> Option<String> {
    Command::new("git")
        .current_dir(repo.root())
        .args(["show", &format!("{rev}:{path}")])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn uid_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".ics"))
        .unwrap_or(path)
        .to_string()
}

pub fn change_feed(repo: &Repo, commit_sha: &str) -> Result<Vec<EventChange>, Error> {
    let parent = run_git(repo, &["rev-parse", &format!("{commit_sha}^")])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string());

    let diff = run_git(
        repo,
        &["diff", "--name-status", "--no-renames", &parent, commit_sha],
    )?;

    let mut changes = Vec::new();
    for line in diff.lines() {
        let mut cols = line.split('\t');
        let status = cols.next().unwrap_or("");
        let path = cols.next().unwrap_or("").to_string();
        if !path.ends_with(".ics") {
            continue;
        }
        let kind = match status.chars().next() {
            Some('A') => ChangeKind::Added,
            Some('M') => ChangeKind::Modified,
            Some('D') => ChangeKind::Deleted,
            _ => continue,
        };
        let new_ics = match kind {
            ChangeKind::Deleted => None,
            _ => blob(repo, commit_sha, &path),
        };
        let old_ics = match kind {
            ChangeKind::Added => None,
            _ => blob(repo, &parent, &path),
        };
        changes.push(EventChange {
            uid: uid_from_path(&path),
            rel_path: path,
            kind,
            new_ics,
            old_ics,
        });
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &std::path::Path, args: &[&str]) {
        let ok = Command::new("git").current_dir(repo).args(args)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap().success();
        assert!(ok, "git {:?}", args);
    }

    #[test]
    fn added_and_modified_and_deleted_are_detected() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let p = dir.path();

        std::fs::create_dir_all(p.join("cal/events")).unwrap();
        std::fs::write(p.join("cal/events/a.ics"), "BEGIN:VEVENT\nUID:a\nEND:VEVENT\n").unwrap();
        std::fs::write(p.join("cal/events/b.ics"), "BEGIN:VEVENT\nUID:b\nEND:VEVENT\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "c1"]);

        std::fs::write(p.join("cal/events/a.ics"), "BEGIN:VEVENT\nUID:a\nSUMMARY:x\nEND:VEVENT\n").unwrap();
        std::fs::remove_file(p.join("cal/events/b.ics")).unwrap();
        std::fs::write(p.join("cal/events/c.ics"), "BEGIN:VEVENT\nUID:c\nEND:VEVENT\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "c2"]);

        let sha = String::from_utf8(
            Command::new("git").current_dir(p).args(["rev-parse", "HEAD"]).output().unwrap().stdout
        ).unwrap().trim().to_string();

        let mut changes = change_feed(&repo, &sha).unwrap();
        changes.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0].kind, ChangeKind::Modified)); // a.ics
        assert_eq!(changes[0].uid, "a");
        assert!(changes[0].old_ics.as_deref().unwrap().contains("UID:a"));
        assert!(matches!(changes[1].kind, ChangeKind::Deleted));  // b.ics
        assert!(matches!(changes[2].kind, ChangeKind::Added));    // c.ics
    }
}
