//! Write operations against forwardemail.
//!
//! Shape-relevant findings from `docs/api-findings.md`:
//! - Contacts: `If-Match` is honored (412 on stale etag). Accepts
//!   `full_name`, `emails`, and/or raw `content` vCard on POST/PUT.
//! - Sieve: create/update use the `content` field (not `script`).
//!   Server parses and returns `is_valid` + `required_capabilities`.
//! - Mail: `PUT {flags: [...]}` and `{folder: "..."}` work; `{raw: ...}`
//!   is silently ignored (body is effectively immutable).

use crate::error::Error;
use crate::forwardemail::contacts::Contact;
use crate::forwardemail::sieve::SieveScript;
use crate::forwardemail::Client;
use serde_json::json;

impl Client {
    // ── Contacts ──────────────────────────────────────────────────────

    /// POST /v1/contacts — create a contact. Provides the minimum viable
    /// input: full_name plus a list of (type, address) email pairs.
    pub async fn create_contact(
        &self,
        full_name: &str,
        emails: &[(&str, &str)],
    ) -> Result<Contact, Error> {
        let emails_json: Vec<_> = emails
            .iter()
            .map(|(t, v)| json!({"type": t, "value": v}))
            .collect();
        let body = json!({"full_name": full_name, "emails": emails_json});
        self.post_json("/v1/contacts", &body).await
    }

    /// PUT /v1/contacts/:id — update a contact. Pass the current etag in
    /// `if_match` for optimistic concurrency; a stale etag returns 412.
    pub async fn update_contact_name(
        &self,
        id: &str,
        full_name: &str,
        if_match: Option<&str>,
    ) -> Result<Contact, Error> {
        let body = json!({"full_name": full_name});
        self.put_json(&format!("/v1/contacts/{id}"), &body, if_match)
            .await
    }

    /// DELETE /v1/contacts/:id
    pub async fn delete_contact(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/contacts/{id}")).await
    }

    // ── Sieve ────────────────────────────────────────────────────────

    /// POST /v1/sieve-scripts — install a new sieve script. Forwardemail
    /// parses the script server-side and returns `is_valid`,
    /// `required_capabilities`, and `security_warnings`, giving us dry-run
    /// validation for free.
    pub async fn create_sieve_script(
        &self,
        name: &str,
        content: &str,
    ) -> Result<SieveScript, Error> {
        let body = json!({"name": name, "content": content});
        self.post_json("/v1/sieve-scripts", &body).await
    }

    /// PUT /v1/sieve-scripts/:id — update an existing script.
    pub async fn update_sieve_script(
        &self,
        id: &str,
        content: &str,
    ) -> Result<SieveScript, Error> {
        let body = json!({"content": content});
        self.put_json(&format!("/v1/sieve-scripts/{id}"), &body, None)
            .await
    }

    /// DELETE /v1/sieve-scripts/:id
    pub async fn delete_sieve_script(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/sieve-scripts/{id}")).await
    }

    // ── Mail flags/folder ────────────────────────────────────────────

    /// PUT /v1/messages/:id with `{flags: [...]}` — replace the flag set.
    pub async fn update_message_flags(
        &self,
        id: &str,
        flags: &[String],
    ) -> Result<serde_json::Value, Error> {
        let body = json!({"flags": flags});
        self.put_json(&format!("/v1/messages/{id}"), &body, None)
            .await
    }

    /// PUT /v1/messages/:id with `{folder: "..."}` — move a message to a
    /// different folder by path (e.g. "Archive").
    pub async fn move_message(
        &self,
        id: &str,
        folder: &str,
    ) -> Result<serde_json::Value, Error> {
        let body = json!({"folder": folder});
        self.put_json(&format!("/v1/messages/{id}"), &body, None)
            .await
    }

    /// DELETE /v1/messages/:id
    pub async fn delete_message(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/messages/{id}")).await
    }
}
