//! Contacts endpoint wrapper (`/v1/contacts`).
//!
//! Shape is driven by the smoke test findings in `docs/api-findings.md`.
//! Key points: `content` holds the raw vCard text, `etag` is the CardDAV
//! ETag, and If-Match is honored on updates.

use crate::error::Error;
use crate::forwardemail::Client;
use serde::{Deserialize, Serialize};

/// A single contact as returned by forwardemail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub id: String,
    pub uid: String,
    #[serde(default)]
    pub full_name: String,
    /// Raw vCard 3.0 text. Store this verbatim.
    pub content: String,
    /// CardDAV etag, including the surrounding quotes as forwardemail returns
    /// them (e.g. `"\"1f6b9549224f62b9f0d4f613c57b16f6\""`).
    pub etag: String,
    #[serde(default)]
    pub is_group: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Client {
    /// GET /v1/contacts — list all contacts for the authenticated alias.
    ///
    /// Default pagination returns up to 10 per page. This helper iterates
    /// through all pages and concatenates results. Fine for the reasonable
    /// contact-count regime (<10k) this tool targets; larger datasets would
    /// want streaming.
    pub async fn list_contacts(&self) -> Result<Vec<Contact>, Error> {
        let mut out = Vec::new();
        let mut page = 1usize;
        loop {
            let path = format!("/v1/contacts?page={page}&limit=50");
            let chunk: Vec<Contact> = self.get_json(&path).await?;
            if chunk.is_empty() {
                break;
            }
            let got = chunk.len();
            out.extend(chunk);
            if got < 50 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// GET /v1/contacts/:id
    pub async fn get_contact(&self, id: &str) -> Result<Contact, Error> {
        self.get_json(&format!("/v1/contacts/{id}")).await
    }
}
