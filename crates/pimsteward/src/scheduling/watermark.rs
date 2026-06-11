//! Activation watermark: the commit SHA at the moment scheduling was first
//! enabled. Only events introduced in commits strictly after it are eligible,
//! so pre-existing events never trigger invites.

use crate::error::Error;
use crate::store::Repo;
use chrono::{DateTime, Utc};
use std::process::Command;

const WATERMARK_PATH: &str = "scheduling/watermark";

pub fn read_watermark(repo: &Repo) -> Option<String> {
    repo.read_file(WATERMARK_PATH)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn ensure_watermark(repo: &Repo, head_sha: &str) -> Result<(), Error> {
    if read_watermark(repo).is_some() {
        return Ok(());
    }
    repo.write_file(WATERMARK_PATH, head_sha.trim().as_bytes())?;
    repo.commit_all(
        "pimsteward-scheduling",
        "scheduling@pimsteward.local",
        "scheduling: set activation watermark",
    )?;
    Ok(())
}

/// True iff `commit_sha` is strictly newer than `watermark` (a descendant).
pub fn commit_is_after(repo: &Repo, watermark: &str, commit_sha: &str) -> bool {
    if watermark == commit_sha {
        return false;
    }
    Command::new("git")
        .current_dir(repo.root())
        .args(["merge-base", "--is-ancestor", watermark, commit_sha])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when the event's start is strictly before `now` and it does not
/// recur. Recurring events (RRULE present) are always eligible.
pub fn is_past_start(ics: &str, now: DateTime<Utc>) -> bool {
    use pimsteward_ical::ical::{vevent_field, vevent_field_all};
    if vevent_field(ics, "RRULE").is_some() {
        return false;
    }
    let Some(dt) = vevent_field_all(ics, "DTSTART").into_iter().last() else {
        return false;
    };
    match parse_ical_dt(&dt) {
        Some(start) => start < now,
        None => false,
    }
}

/// Parse a DTSTART value (basic UTC `...Z`, or local/floating basic form).
/// Floating/local values are treated as UTC for the coarse past/future guard.
fn parse_ical_dt(v: &str) -> Option<DateTime<Utc>> {
    use chrono::NaiveDateTime;
    let v = v.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(v) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S") {
        return Some(DateTime::from_naive_utc_and_offset(ndt, Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y%m%d") {
        return Some(DateTime::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0)?, Utc));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        repo.empty_commit("t", "t@t", "root").unwrap();
        let head = repo.empty_commit("t", "t@t", "head1").unwrap();
        ensure_watermark(&repo, &head).unwrap();
        assert_eq!(read_watermark(&repo).as_deref(), Some(head.as_str()));
        let head2 = repo.empty_commit("t", "t@t", "head2").unwrap();
        ensure_watermark(&repo, &head2).unwrap();
        assert_eq!(read_watermark(&repo).as_deref(), Some(head.as_str()));
    }

    #[test]
    fn past_nonrecurring_is_filtered_recurring_is_not() {
        let now: DateTime<Utc> = "2026-06-11T00:00:00Z".parse().unwrap();
        let past = "BEGIN:VEVENT\nUID:p\nDTSTART:20200101T120000Z\nEND:VEVENT\n";
        let future = "BEGIN:VEVENT\nUID:f\nDTSTART:20990101T120000Z\nEND:VEVENT\n";
        let recurring = "BEGIN:VEVENT\nUID:r\nDTSTART:20200101T120000Z\nRRULE:FREQ=WEEKLY\nEND:VEVENT\n";
        assert!(is_past_start(past, now));
        assert!(!is_past_start(future, now));
        assert!(!is_past_start(recurring, now));
    }

    #[test]
    fn commit_is_after_is_directional() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(dir.path()).unwrap();
        let a = repo.empty_commit("t", "t@t", "a").unwrap();
        let b = repo.empty_commit("t", "t@t", "b").unwrap();
        assert!(commit_is_after(&repo, &a, &b));   // b is newer than a
        assert!(!commit_is_after(&repo, &b, &a));  // a is not after b
        assert!(!commit_is_after(&repo, &a, &a));  // equal is not "after"
    }
}
