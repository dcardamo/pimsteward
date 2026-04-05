//! Calendar pull loop.
//!
//! Pattern mirrors contacts: raw content (iCalendar text) is stored verbatim,
//! diff is etag-based (falling back to content hash if etag is missing from
//! the response). Calendars themselves are also written as `_calendar.json`
//! manifest files under each calendar's subdirectory so history tracks
//! calendar-level changes (renames, color changes, etc.) independently from
//! event changes.

use crate::error::Error;
use crate::forwardemail::calendar::{Calendar, CalendarEvent};
use crate::forwardemail::Client;
use crate::pull::{filename_safe, PullResult, PullSummary};
use crate::store::Repo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventMeta {
    id: String,
    uid: Option<String>,
    calendar_id: Option<String>,
    /// sha256 of the raw iCalendar text. Calendar events have no etag in
    /// the forwardemail REST response, so content hash is the stable diff
    /// key.
    content_sha256: Option<String>,
    updated_at: Option<String>,
}

pub async fn pull_calendar(
    client: &Client,
    repo: &Repo,
    alias: &str,
    author_name: &str,
    author_email: &str,
) -> PullResult<PullSummary> {
    let mut summary = PullSummary {
        resource: "calendar",
        ..Default::default()
    };

    // 1. List calendars; write manifest per calendar.
    let calendars = client.list_calendars().await?;
    let mut current_calendar_dirs = HashSet::new();
    for c in &calendars {
        let cal_dir = calendar_dir(alias, c);
        current_calendar_dirs.insert(cal_dir.clone());
        let manifest_path = format!("{cal_dir}/_calendar.json");
        repo.write_file(&manifest_path, serde_json::to_vec_pretty(c)?.as_slice())?;
    }

    // 2. Fetch all events (one pass — forwardemail's list endpoint supports
    // calendar_id filtering but global is simpler and fewer round-trips).
    let remote_events = client.list_calendar_events(None).await?;

    // Build: per-calendar local meta + per-calendar remote events
    let mut remote_by_cal: HashMap<String, Vec<&CalendarEvent>> = HashMap::new();
    for e in &remote_events {
        let cal_id = e.calendar_id.clone().unwrap_or_default();
        remote_by_cal.entry(cal_id).or_default().push(e);
    }

    for c in &calendars {
        let cal_dir = calendar_dir(alias, c);
        let local_meta = read_local_event_meta(repo, &cal_dir)?;
        let events_here = remote_by_cal.remove(&c.id).unwrap_or_default();

        let remote_by_key: HashMap<String, &CalendarEvent> =
            events_here.iter().map(|e| (event_key(e), *e)).collect();

        for e in &events_here {
            let key = event_key(e);
            let ical = e.ical.clone().unwrap_or_default();
            let content_hash = sha256(ical.as_bytes());
            let prev = local_meta.get(&key);

            // No CardDAV-style etag on calendar events per the forwardemail
            // API shape — diff on content hash only.
            let changed = match prev {
                None => true,
                Some(p) => p.content_sha256.as_deref() != Some(content_hash.as_str()),
            };

            if !changed {
                continue;
            }

            let ics_path = format!("{cal_dir}/events/{key}.ics");
            repo.write_file(&ics_path, ical.as_bytes())?;

            let meta = EventMeta {
                id: e.id.clone(),
                uid: e.uid.clone(),
                calendar_id: e.calendar_id.clone(),
                content_sha256: Some(content_hash),
                updated_at: e.updated_at.clone(),
            };
            let meta_path = format!("{cal_dir}/events/{key}.meta.json");
            repo.write_file(&meta_path, serde_json::to_vec_pretty(&meta)?.as_slice())?;

            if prev.is_some() {
                summary.updated += 1;
            } else {
                summary.added += 1;
            }
        }

        // Deletions within this calendar
        for local_key in local_meta.keys() {
            if !remote_by_key.contains_key(local_key) {
                let ics = repo
                    .root()
                    .join(format!("{cal_dir}/events/{local_key}.ics"));
                let meta = repo
                    .root()
                    .join(format!("{cal_dir}/events/{local_key}.meta.json"));
                let _ = std::fs::remove_file(&ics);
                let _ = std::fs::remove_file(&meta);
                summary.deleted += 1;
            }
        }
    }

    // Remove stale calendar directories (calendar was deleted remotely)
    cleanup_removed_calendars(repo, alias, &current_calendar_dirs)?;

    let msg = format!(
        "calendar: +{} ~{} -{}",
        summary.added, summary.updated, summary.deleted
    );
    summary.commit_sha = repo.commit_all(author_name, author_email, &msg)?;
    Ok(summary)
}

/// Key used as the filename for an event. Prefer the iCal UID since that's
/// stable across servers and syncs; fall back to the forwardemail id.
fn event_key(e: &CalendarEvent) -> String {
    filename_safe(e.uid.as_deref().unwrap_or(&e.id))
}

fn calendar_dir(alias: &str, c: &Calendar) -> String {
    // Use calendar id as the directory name — names can change and collide,
    // ids are stable. The _calendar.json manifest inside records the
    // human-readable name + color + etc.
    format!(
        "sources/forwardemail/{}/calendars/{}",
        alias,
        filename_safe(&c.id)
    )
}

fn read_local_event_meta(repo: &Repo, cal_dir: &str) -> Result<HashMap<String, EventMeta>, Error> {
    let mut out = HashMap::new();
    let dir: PathBuf = repo.root().join(format!("{cal_dir}/events"));
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)
        .map_err(|e| Error::store(format!("readdir {}: {}", dir.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let name = entry.file_name().into_string().unwrap_or_default();
        if !name.ends_with(".meta.json") {
            continue;
        }
        let stem = name.trim_end_matches(".meta.json").to_string();
        let bytes = std::fs::read(entry.path()).map_err(|e| Error::store(format!("read: {e}")))?;
        let meta: EventMeta = serde_json::from_slice(&bytes)?;
        out.insert(stem, meta);
    }
    Ok(out)
}

fn cleanup_removed_calendars(
    repo: &Repo,
    alias: &str,
    current_dirs: &HashSet<String>,
) -> Result<(), Error> {
    let cals_root = repo
        .root()
        .join(format!("sources/forwardemail/{alias}/calendars"));
    if !cals_root.exists() {
        return Ok(());
    }
    // Build absolute-prefixed current set to compare against actual directory paths
    let current_abs: HashSet<PathBuf> = current_dirs.iter().map(|d| repo.root().join(d)).collect();
    for entry in std::fs::read_dir(&cals_root)
        .map_err(|e| Error::store(format!("readdir {}: {}", cals_root.display(), e)))?
    {
        let entry = entry.map_err(|e| Error::store(format!("dir entry: {}", e)))?;
        let path = entry.path();
        if !current_abs.contains(&path) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    Ok(())
}

fn sha256(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}
