//! Calendar event write operations.
//!
//! Dispatches mutations through the [`CalendarWriter`] trait so the same
//! audit-and-refresh wrapper works for both forwardemail (REST) and iCloud
//! (CalDAV) backends. The post-write refresh runs through the matching
//! [`CalendarSource`] so the local backup tree picks up the new state
//! before the next pull cycle.
//!
//! Identifier note: forwardemail's REST API addresses events by its
//! global eventId (which the trait surface calls `uid`), while iCloud
//! addresses by `(calendar_url, ical_uid)`. The MCP layer resolves both
//! into the same trait shape — see `src/source/rest.rs` and
//! `src/icloud/caldav.rs` for the per-backend mapping.

use crate::error::Error;
use crate::forwardemail::calendar::CalendarEvent;
use crate::pull::calendar::pull_calendar;
use crate::source::traits::{CalendarSource, CalendarWriter};
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};

/// Create a new calendar event via the writer trait, then refresh the
/// backup tree from the source trait so the new event lands in git.
///
/// `event_id` is the optional caller-supplied event identifier. For
/// forwardemail it becomes the REST API's `event_id` body field (so the
/// returned `id` matches); for iCloud it MUST be the iCalendar UID — the
/// writer uses it as the `.ics` filename.
///
/// Returns the writer's `CalendarEvent` directly so MCP callers see the
/// derived fields (`summary`, `start_date`, `end_date`, …) populated. For
/// forwardemail those come from the REST server's normalised response; for
/// iCloud the writer synthesizes them from the request's iCal text (CalDAV
/// PUT bodies are empty, so the request is the canonical text).
#[allow(clippy::too_many_arguments)]
pub async fn create_event(
    writer: &dyn CalendarWriter,
    source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    calendar_id: &str,
    ical: &str,
    event_id: Option<&str>,
) -> Result<CalendarEvent, Error> {
    // For forwardemail, an empty `uid` lets the writer fall back to
    // server-side id allocation. For iCloud we extract the UID from the
    // iCal payload up-front so the .ics filename matches the VEVENT UID.
    let uid_string;
    let uid: &str = match event_id {
        Some(e) => e,
        None => {
            uid_string = extract_ical_uid(ical).unwrap_or_default();
            &uid_string
        }
    };
    let created = writer.create_event(calendar_id, uid, ical).await?;

    // Pull the title for the audit commit message. Prefer the server-
    // normalised summary on the returned event; fall back to the request
    // iCal so we never lose the title even if the writer can't supply it.
    let title = created
        .summary
        .clone()
        .or_else(|| extract_ical_field(ical, "SUMMARY"))
        .unwrap_or_else(|| "(no title)".to_string());
    let audit = WriteAudit {
        attribution,
        tool: "create_event",
        resource: "calendar",
        resource_id: created.id.clone(),
        args: serde_json::json!({
            "calendar_id": calendar_id,
            "event_id": event_id,
            "ical_bytes": ical.len(),
        }),
        summary: format!("calendar: create event {title} in {calendar_id}"),
    };
    refresh(source, repo, alias, attribution, &audit).await?;
    Ok(created)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_event(
    writer: &dyn CalendarWriter,
    source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    calendar_id: &str,
    uid: &str,
    ical: &str,
    if_match: &str,
) -> Result<CalendarEvent, Error> {
    let updated = writer
        .update_event(calendar_id, uid, ical, if_match)
        .await?;
    let title = updated
        .summary
        .clone()
        .or_else(|| extract_ical_field(ical, "SUMMARY"))
        .unwrap_or_else(|| "(no title)".to_string());
    let audit = WriteAudit {
        attribution,
        tool: "update_event",
        resource: "calendar",
        resource_id: uid.to_string(),
        args: serde_json::json!({
            "calendar_id": calendar_id,
            "ical_bytes": ical.len(),
            "if_match_present": !if_match.is_empty(),
        }),
        summary: format!("calendar: update event {title} ({uid})"),
    };
    refresh(source, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

#[allow(clippy::too_many_arguments)]
pub async fn delete_event(
    writer: &dyn CalendarWriter,
    source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    calendar_id: &str,
    uid: &str,
    if_match: &str,
) -> Result<(), Error> {
    writer.delete_event(calendar_id, uid, if_match).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_event",
        resource: "calendar",
        resource_id: uid.to_string(),
        args: serde_json::json!({
            "calendar_id": calendar_id,
            "if_match_present": !if_match.is_empty(),
        }),
        summary: format!("calendar: delete event {uid}"),
    };
    refresh(source, repo, alias, attribution, &audit).await
}

async fn refresh(
    source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    let _ = pull_calendar(
        source,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}

/// Extract the iCalendar `UID:` property from an iCal payload. Returns
/// `None` if the payload is malformed or has no UID.
fn extract_ical_uid(ical: &str) -> Option<String> {
    extract_ical_field(ical, "UID")
}

/// Best-effort iCalendar property extractor — finds the first matching
/// line and returns its value. Doesn't handle parameter folding (the
/// caller is expected to pass tightly-formed iCal here, since this is
/// post-write metadata and not the parsing code path).
fn extract_ical_field(ical: &str, name: &str) -> Option<String> {
    let upper = name.to_ascii_uppercase();
    for line in ical.lines() {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let head = &line[..colon];
        let prop = head.split(';').next().unwrap_or(head);
        if prop.eq_ignore_ascii_case(&upper) {
            return Some(line[colon + 1..].trim_end_matches('\r').to_string());
        }
    }
    None
}
