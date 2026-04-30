//! Calendar event restore.
//!
//! The historical state is the `.ics` file in the backup tree at a given
//! commit. The operation is either:
//! - `Recreate` — event was deleted; POST a new event with the historical
//!   iCalendar payload
//! - `UpdateIcal` — event exists but its ical differs; PUT the historical
//!   iCalendar payload
//! - `NoOp` — live ical already matches historical
//!
//! Caveat: the event is identified by its VEVENT UID (stable iCalendar UID).
//! When recreating a deleted event, we preserve the UID so any CalDAV
//! client that cached it will see the familiar identifier.

use crate::error::Error;
use crate::pull::calendar::pull_calendar;
use crate::restore::read_git_blob;
use crate::source::traits::{CalendarSource, CalendarWriter};
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarRestorePlan {
    pub path: String,
    pub at_sha: String,
    pub calendar_id: String,
    pub event_uid: String,
    pub operation: CalendarOperation,
    /// Forwardemail eventId (different from the iCalendar UID) of the live
    /// event, or None if it doesn't currently exist.
    pub live_event_id: Option<String>,
    pub human_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CalendarOperation {
    UpdateIcal { target_ical: String },
    Recreate { ical: String },
    NoOp,
}

pub async fn plan_calendar(
    source: &dyn CalendarSource,
    repo: &Repo,
    _alias: &str,
    calendar_id: &str,
    event_uid: &str,
    at_sha: &str,
) -> Result<(CalendarRestorePlan, String), Error> {
    let rel_path = format!("calendars/{calendar_id}/events/{event_uid}.ics");
    let historical_ical =
        String::from_utf8_lossy(&read_git_blob(repo, at_sha, &rel_path)?).into_owned();

    // Find live event by iCalendar uid (not forwardemail's id field — uid
    // is what the VEVENT UID: property contains and what we use for
    // filenames).
    let live_events = source.list_events(Some(calendar_id)).await?;
    let live_event = live_events
        .iter()
        .find(|e| e.uid.as_deref() == Some(event_uid));

    let (operation, human_summary, live_event_id) = match live_event {
        None => (
            CalendarOperation::Recreate {
                ical: historical_ical.clone(),
            },
            format!(
                "Calendar event uid={event_uid} was deleted from forwardemail. \
                 Restore will re-create it in calendar {calendar_id} ({} bytes of iCal).",
                historical_ical.len()
            ),
            None,
        ),
        Some(e) if e.ical.as_deref() == Some(historical_ical.as_str()) => (
            CalendarOperation::NoOp,
            format!("Calendar event uid={event_uid} already matches — nothing to do."),
            Some(e.id.clone()),
        ),
        Some(e) => (
            CalendarOperation::UpdateIcal {
                target_ical: historical_ical.clone(),
            },
            format!(
                "Calendar event uid={event_uid} ical differs. Restore will replace \
                 the live ical with the historical version."
            ),
            Some(e.id.clone()),
        ),
    };

    let plan = CalendarRestorePlan {
        path: rel_path,
        at_sha: at_sha.to_string(),
        calendar_id: calendar_id.to_string(),
        event_uid: event_uid.to_string(),
        operation,
        live_event_id,
        human_summary,
    };
    let token = crate::restore::plan_token(&plan)?;
    Ok((plan, token))
}

#[allow(clippy::too_many_arguments)]
pub async fn apply_calendar(
    writer: &dyn CalendarWriter,
    source: &dyn CalendarSource,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    plan: &CalendarRestorePlan,
    supplied_token: &str,
) -> Result<(), Error> {
    let computed = crate::restore::plan_token(plan)?;
    if computed != supplied_token {
        return Err(Error::config(format!(
            "restore plan_token mismatch (calendar): expected {computed}, got {supplied_token}"
        )));
    }

    match &plan.operation {
        CalendarOperation::NoOp => return Ok(()),
        CalendarOperation::UpdateIcal { target_ical } => {
            // For forwardemail, the trait's `uid` is forwardemail's
            // global eventId — that's what `live_event_id` carries. For
            // CalDAV-style writers, `live_event_id` should match the
            // iCalendar UID; the bulk-restore path stores them as the
            // same string. Restore deliberately overwrites without an
            // etag (`""`) — the user explicitly asked to roll back.
            let id = plan
                .live_event_id
                .as_ref()
                .ok_or_else(|| Error::config("UpdateIcal op requires live_event_id in plan"))?;
            writer
                .update_event(&plan.calendar_id, id, target_ical, "")
                .await?;
        }
        CalendarOperation::Recreate { ical } => {
            // Preserve the iCalendar UID — the writer addresses the new
            // resource by uid, so passing the original UID keeps clients
            // that cached it pointed at the same identifier.
            writer
                .create_event(&plan.calendar_id, &plan.event_uid, ical)
                .await?;
        }
    }

    let _ = pull_calendar(
        source,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let audit = WriteAudit {
        attribution,
        tool: "restore_calendar_event",
        resource: "calendar",
        resource_id: plan.event_uid.clone(),
        args: serde_json::to_value(plan)?,
        summary: format!(
            "restore: calendar/{}/{} from {}",
            plan.calendar_id,
            plan.event_uid,
            &plan.at_sha[..8.min(plan.at_sha.len())]
        ),
    };
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}
