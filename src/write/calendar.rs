//! Calendar event write operations.
//!
//! Creation payload shape: `{calendar_id, ical, event_id?}`. See
//! `docs/api-findings.md` for the history of figuring this out — the field
//! is `ical`, not `content`, unlike contacts. Forwardemail's REST API is
//! inconsistent between resource types.

use crate::error::Error;
use crate::forwardemail::calendar::CalendarEvent;
use crate::forwardemail::Client;
use crate::pull::calendar::pull_calendar;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};

pub async fn create_event(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    calendar_id: &str,
    ical: &str,
    event_id: Option<&str>,
) -> Result<CalendarEvent, Error> {
    let created = client
        .create_calendar_event(calendar_id, ical, event_id)
        .await?;
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
        summary: format!(
            "calendar: create event {} in {calendar_id}",
            created
                .summary
                .clone()
                .unwrap_or_else(|| "<no summary>".into())
        ),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(created)
}

pub async fn update_event(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    ical: Option<&str>,
    target_calendar_id: Option<&str>,
) -> Result<CalendarEvent, Error> {
    let updated = client
        .update_calendar_event(id, ical, target_calendar_id)
        .await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_event",
        resource: "calendar",
        resource_id: id.to_string(),
        args: serde_json::json!({
            "target_calendar_id": target_calendar_id,
            "ical_bytes": ical.map(str::len),
        }),
        summary: format!("calendar: update event {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

pub async fn delete_event(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
) -> Result<(), Error> {
    client.delete_calendar_event(id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_event",
        resource: "calendar",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("calendar: delete event {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await
}

async fn refresh(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    let _ = pull_calendar(
        client,
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
