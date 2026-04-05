//! Mail endpoints wrapper (`/v1/folders`, `/v1/messages`).
//!
//! Shape matches the live API behaviour documented in `docs/api-findings.md`.
//! The message list endpoint returns summary objects (no body), so the pull
//! loop fetches full messages one-by-one for anything whose `modseq` or
//! `updated_at` differs from the local cache.
//!
//! v1 storage strategy: serialize the whole JSON response per message. This
//! is slightly lossy compared to true RFC822 `.eml` (forwardemail may or may
//! not expose a raw-body endpoint — TBD in phase H) but every field a future
//! tool might need is present: parsed nodemailer structure, flags, folder,
//! modseq, etag, etc.

use crate::error::Error;
use crate::forwardemail::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub uid_validity: Option<i64>,
    #[serde(default)]
    pub uid_next: Option<i64>,
    #[serde(default)]
    pub modify_index: Option<i64>,
    #[serde(default)]
    pub subscribed: bool,
    #[serde(default)]
    pub special_use: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Message summary as returned by the list endpoint. We intentionally only
/// capture the fields we actually diff on plus ids — the full content lands
/// in `MessageDetail` via per-id GET.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: String,
    #[serde(default)]
    pub folder_id: String,
    #[serde(default)]
    pub folder_path: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub uid: Option<i64>,
    #[serde(default)]
    pub modseq: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub flags: Vec<String>,
}

impl Client {
    /// GET /v1/folders — all folders for the alias. Usually <20 items so no
    /// pagination needed in practice.
    pub async fn list_folders(&self) -> Result<Vec<Folder>, Error> {
        self.get_json("/v1/folders?limit=50").await
    }

    /// GET /v1/messages?folder=<path_or_id>&limit=…&page=… — paginated.
    /// Returns message *summaries* (without body).
    pub async fn list_messages_in_folder(
        &self,
        folder: &str,
    ) -> Result<Vec<MessageSummary>, Error> {
        let mut out = Vec::new();
        let mut page = 1usize;
        loop {
            let path = format!(
                "/v1/messages?folder={}&page={page}&limit=50",
                urlencoding(folder)
            );
            let chunk: Vec<MessageSummary> = self.get_json(&path).await?;
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

    /// GET /v1/messages/:id — full message JSON including nodemailer parse.
    /// Returned as `serde_json::Value` to avoid lossy typing; the store
    /// writes this verbatim.
    pub async fn get_message(&self, id: &str) -> Result<serde_json::Value, Error> {
        self.get_json(&format!("/v1/messages/{id}")).await
    }
}

/// Minimal URL component encoder — only the characters that actually appear
/// in forwardemail folder paths need to be escaped. Avoids pulling in a full
/// urlencoding crate for a single use-site.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            ' ' => out.push_str("%20"),
            c => {
                for b in c.to_string().as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_basics() {
        assert_eq!(urlencoding("INBOX"), "INBOX");
        assert_eq!(urlencoding("Sent Mail"), "Sent%20Mail");
        assert_eq!(urlencoding("A/B"), "A%2FB");
    }
}
