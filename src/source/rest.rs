//! REST-backed MailSource — the v1 behaviour, now behind the trait.
//!
//! This is a thin adapter over [`crate::forwardemail::Client`]. It does
//! no work the existing Client didn't already do; the point is to let
//! other backends slot in without rewriting the pull loop.

use crate::error::Error;
use crate::forwardemail::mail::{Folder, MessageSummary};
use crate::forwardemail::Client;
use crate::source::traits::{FetchedMessage, MailSource};
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct RestMailSource {
    client: Client,
}

impl RestMailSource {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MailSource for RestMailSource {
    fn tag(&self) -> &'static str {
        "rest"
    }

    async fn list_folders(&self) -> Result<Vec<Folder>, Error> {
        self.client.list_folders().await
    }

    async fn list_messages(&self, folder: &str) -> Result<Vec<MessageSummary>, Error> {
        self.client.list_messages_in_folder(folder).await
    }

    async fn fetch_message(&self, _folder: &str, id: &str) -> Result<FetchedMessage, Error> {
        let full = self.client.get_message(id).await?;
        let raw = full
            .get("raw")
            .and_then(|v| v.as_str())
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| {
                Error::store(format!(
                    "forwardemail REST response for message {id} missing `raw` field"
                ))
            })?;

        // Build a summary from the full response. For the REST path the
        // caller already has a summary from list_messages but we rebuild
        // here so FetchedMessage is self-contained for the trait.
        let summary = MessageSummary {
            id: id.to_string(),
            folder_id: full
                .get("folder_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            folder_path: full
                .get("folder_path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            subject: full
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            size: full.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            uid: full.get("uid").and_then(|v| v.as_i64()),
            modseq: full.get("modseq").and_then(|v| v.as_i64()),
            updated_at: full
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            flags: full
                .get("flags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        };

        Ok(FetchedMessage {
            summary,
            raw,
            extra: Some(full),
        })
    }
}

// ── Calendar via REST ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RestCalendarSource {
    client: Client,
}

impl RestCalendarSource {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl crate::source::traits::CalendarSource for RestCalendarSource {
    fn tag(&self) -> &'static str {
        "rest"
    }

    async fn list_calendars(&self) -> Result<Vec<crate::forwardemail::calendar::Calendar>, Error> {
        self.client.list_calendars().await
    }

    async fn list_events(
        &self,
        calendar_id: Option<&str>,
    ) -> Result<Vec<crate::forwardemail::calendar::CalendarEvent>, Error> {
        self.client.list_calendar_events(calendar_id).await
    }
}

// ── Contacts via REST ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RestContactsSource {
    client: Client,
}

impl RestContactsSource {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl crate::source::traits::ContactsSource for RestContactsSource {
    fn tag(&self) -> &'static str {
        "rest"
    }

    async fn list_contacts(&self) -> Result<Vec<crate::forwardemail::contacts::Contact>, Error> {
        self.client.list_contacts().await
    }
}
