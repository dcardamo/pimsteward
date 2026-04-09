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

/// Structured input for `Client::create_message`. Bundles all the fields
/// the forwardemail POST /v1/messages endpoint accepts.
pub struct NewMessage {
    pub folder: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
}

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

    /// POST /v1/contacts with raw vCard content — re-creates a contact
    /// preserving all fields (emails, phones, addresses, notes, org, etc.)
    /// without needing a vCard parser. Forwardemail parses the vCard
    /// server-side and populates all structured fields from it.
    pub async fn create_contact_from_vcard(&self, vcard: &str) -> Result<Contact, Error> {
        let body = json!({"content": vcard});
        self.post_json("/v1/contacts", &body).await
    }

    /// PUT /v1/contacts/:id with raw vCard content + full_name extracted
    /// from the vCard. forwardemail's PUT endpoint reliably accepts
    /// `full_name` but may silently ignore `content` (it only fully
    /// supports `content` on POST). Sending both hedges: if `content`
    /// is honored, all fields restore; if not, at least the name is
    /// correct.
    pub async fn update_contact_vcard(
        &self,
        id: &str,
        vcard: &str,
        full_name: &str,
        if_match: Option<&str>,
    ) -> Result<Contact, Error> {
        let body = json!({"full_name": full_name, "content": vcard});
        self.put_json(&format!("/v1/contacts/{id}"), &body, if_match)
            .await
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
        // Note: is_active is read-only in forwardemail's REST API — the
        // API silently ignores it on both POST and PUT. Scripts must be
        // activated via the forwardemail dashboard or ManageSieve protocol.
        let body = json!({"name": name, "content": content});
        self.post_json("/v1/sieve-scripts", &body).await
    }

    /// PUT /v1/sieve-scripts/:id — update script content.
    ///
    /// Note: `is_active` is read-only in forwardemail's REST API. The API
    /// silently ignores it on both POST and PUT (returns 200 but the field
    /// stays unchanged). Scripts must be activated through forwardemail's
    /// dashboard or ManageSieve protocol (port 4190).
    pub async fn update_sieve_script(&self, id: &str, content: &str) -> Result<SieveScript, Error> {
        let body = json!({"content": content});
        self.put_json(&format!("/v1/sieve-scripts/{id}"), &body, None)
            .await
    }

    /// DELETE /v1/sieve-scripts/:id
    pub async fn delete_sieve_script(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/sieve-scripts/{id}")).await
    }

    // ── Mail flags/folder ────────────────────────────────────────────

    /// POST /v1/messages — append raw RFC822 bytes to a folder (IMAP
    /// APPEND equivalent). Used by the mail restore flow to re-append a
    /// message that was hard-deleted from the server. Forwardemail
    /// generates a new message id; the restored message is byte-identical
    /// to the historical version but has a different backend id.
    pub async fn append_raw_message(
        &self,
        folder: &str,
        raw_rfc822: &[u8],
    ) -> Result<serde_json::Value, Error> {
        // The API accepts `{folder, raw}` with raw as a string. We parse
        // the bytes as UTF-8 — forwardemail will round-trip 8-bit MIME
        // without us needing to encode, because the `raw` field is what
        // it stores verbatim.
        let raw_str = std::str::from_utf8(raw_rfc822).map_err(|e| {
            Error::config(format!(
                "append_raw_message: stored raw bytes are not valid UTF-8: {e}"
            ))
        })?;
        let body = json!({"folder": folder, "raw": raw_str});
        self.post_json("/v1/messages", &body).await
    }

    /// POST /v1/emails — send an outgoing email via forwardemail's SMTP
    /// bridge. Same alias credentials as every other REST call (Basic
    /// Auth), but this is a distinct capability: delivery to a third
    /// party over the public internet, not a mailbox mutation.
    ///
    /// Forwardemail handles the SMTP handshake, envelope construction,
    /// DKIM signing (if the sending domain has DKIM configured), and
    /// persists a copy to the alias's `Sent` folder on success — so the
    /// next pull will capture the full outgoing message into git
    /// automatically.
    ///
    /// Returns the JSON response body verbatim (contains the new message
    /// `id` plus metadata). Callers SHOULD compute and log a hash of the
    /// body bytes at the pimsteward write layer rather than trusting the
    /// returned record to stay byte-stable under later API revisions.
    ///
    /// The `from` field is set to the authenticated alias — forwardemail
    /// requires an explicit envelope sender on POST /v1/emails and
    /// rejects the request with HTTP 403 "Envelope MAIL FROM header
    /// could not be parsed or was missing." if it's absent. SMTP auth
    /// would also reject a mismatched from, so we just use the alias
    /// we're already authenticated as.
    pub async fn send_email(
        &self,
        msg: &NewMessage,
    ) -> Result<serde_json::Value, Error> {
        let mut body = json!({
            "from": self.alias_user(),
            "to": msg.to,
            "subject": msg.subject,
        });
        let obj = body.as_object_mut().expect("just created");
        if !msg.cc.is_empty() {
            obj.insert("cc".into(), json!(msg.cc));
        }
        if !msg.bcc.is_empty() {
            obj.insert("bcc".into(), json!(msg.bcc));
        }
        if let Some(ref t) = msg.text {
            obj.insert("text".into(), json!(t));
        }
        if let Some(ref h) = msg.html {
            obj.insert("html".into(), json!(h));
        }
        self.post_json("/v1/emails", &body).await
    }

    /// POST /v1/messages with structured fields — creates a new message in
    /// the specified folder. Forwardemail constructs the RFC822 envelope and
    /// body server-side from the structured fields (to, cc, bcc, subject,
    /// text, html). The `\Draft` flag is set automatically by forwardemail
    /// for messages placed into the Drafts folder.
    pub async fn create_message(
        &self,
        msg: &NewMessage,
    ) -> Result<serde_json::Value, Error> {
        let mut body = json!({
            "folder": msg.folder,
            "to": msg.to,
            "subject": msg.subject,
        });
        let obj = body.as_object_mut().expect("just created");
        if !msg.cc.is_empty() {
            obj.insert("cc".into(), json!(msg.cc));
        }
        if !msg.bcc.is_empty() {
            obj.insert("bcc".into(), json!(msg.bcc));
        }
        if let Some(ref t) = msg.text {
            obj.insert("text".into(), json!(t));
        }
        if let Some(ref h) = msg.html {
            obj.insert("html".into(), json!(h));
        }
        self.post_json("/v1/messages", &body).await
    }

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
    pub async fn move_message(&self, id: &str, folder: &str) -> Result<serde_json::Value, Error> {
        let body = json!({"folder": folder});
        self.put_json(&format!("/v1/messages/{id}"), &body, None)
            .await
    }

    /// DELETE /v1/messages/:id
    pub async fn delete_message(&self, id: &str) -> Result<(), Error> {
        self.delete_path(&format!("/v1/messages/{id}")).await
    }
}
