//! rmcp-based MCP server. The tool implementations live here; most are thin
//! wrappers around `crate::forwardemail::Client` methods with a permission
//! check on the front and a JSON-ready return value on the back.

use crate::forwardemail::Client;
use crate::source::{MailSource, MailWriter};
use crate::permission::{Permissions, Resource, Scope};
use crate::store::Repo;
use crate::write::audit::Attribution;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use std::process::Command;
use std::sync::Arc;

/// Shared state held by every tool handler.
#[derive(Clone)]
pub struct PimstewardServer {
    inner: Arc<Inner>,
    tool_router: ToolRouter<PimstewardServer>,
}

struct Inner {
    client: Client,
    repo: Repo,
    permissions: Permissions,
    alias: String,
    /// Mail read source for post-write refresh. Matches the daemon's read
    /// source so IDs stay consistent.
    mail_source: Arc<dyn MailSource>,
    /// Mail write backend: REST or IMAP, matching the read source.
    mail_writer: Arc<dyn MailWriter>,
}

impl std::fmt::Debug for PimstewardServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PimstewardServer")
            .field("alias", &self.inner.alias)
            .finish_non_exhaustive()
    }
}

impl PimstewardServer {
    pub fn new(
        client: Client,
        repo: Repo,
        permissions: Permissions,
        alias: String,
        mail_source: Arc<dyn MailSource>,
        mail_writer: Arc<dyn MailWriter>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                repo,
                permissions,
                alias,
                mail_source,
                mail_writer,
            }),
            tool_router: Self::tool_router(),
        }
    }

    fn check(&self, resource: Resource) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_read(resource)
            .map_err(|e| McpError::invalid_params(format!("permission denied: {e}"), None))
    }

    fn check_write(&self, resource: Resource) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_write(resource)
            .map_err(|e| McpError::invalid_params(format!("permission denied: {e}"), None))
    }

    /// Scoped read check — per-folder for email, per-calendar for calendar.
    /// Passing `None` for the scope target behaves identically to
    /// [`Self::check`].
    fn check_scoped(&self, scope: Scope<'_>) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_read_scoped(&scope)
            .map_err(|e| McpError::invalid_params(format!("permission denied: {e}"), None))
    }

    /// Scoped write check.
    fn check_write_scoped(&self, scope: Scope<'_>) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_write_scoped(&scope)
            .map_err(|e| McpError::invalid_params(format!("permission denied: {e}"), None))
    }

    /// Look up a message's current folder and source-specific id from
    /// the backup tree's meta.json. The `id` parameter is the canonical
    /// id (filename stem) as seen in the backup. Returns (folder, source_id).
    fn lookup_message_meta(
        &self,
        canonical_id: &str,
    ) -> Result<crate::pull::mail::MessageMeta, McpError> {
        // Scan all folder directories for a meta.json matching this canonical id.
        let mail_root = self
            .inner
            .repo
            .root()
            .join(format!("sources/forwardemail/{}/mail", self.inner.alias));
        if !mail_root.exists() {
            return Err(McpError::invalid_params(
                format!("no mail tree for alias {}", self.inner.alias),
                None,
            ));
        }
        for folder_entry in std::fs::read_dir(&mail_root).map_err(|e| {
            McpError::internal_error(format!("readdir {}: {e}", mail_root.display()), None)
        })? {
            let folder_entry = folder_entry.map_err(|e| {
                McpError::internal_error(format!("dir entry: {e}"), None)
            })?;
            let name = folder_entry.file_name().into_string().unwrap_or_default();
            if name == "_attachments" {
                continue;
            }
            let meta_path = folder_entry
                .path()
                .join(format!("{canonical_id}.meta.json"));
            if meta_path.exists() {
                let bytes = std::fs::read(&meta_path).map_err(|e| {
                    McpError::internal_error(format!("read {}: {e}", meta_path.display()), None)
                })?;
                let meta: crate::pull::mail::MessageMeta =
                    serde_json::from_slice(&bytes).map_err(|e| {
                        McpError::internal_error(
                            format!("parse {}: {e}", meta_path.display()),
                            None,
                        )
                    })?;
                return Ok(meta);
            }
        }
        Err(McpError::invalid_params(
            format!("message {canonical_id} not found in backup tree"),
            None,
        ))
    }


    fn api_error(&self, e: crate::Error) -> McpError {
        McpError::internal_error(format!("forwardemail: {e}"), None)
    }

    fn attribution(&self, caller: Option<String>, reason: Option<String>) -> Attribution {
        Attribution::new(caller.unwrap_or_else(|| "ai".into()), reason)
    }
}

// Parameter structs for each tool. Derive schemars::JsonSchema so rmcp can
// surface argument schemas to the MCP client.

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchEmailParams {
    /// Free-text search across all fields. Forwardemail's `?q=` parameter.
    #[serde(default)]
    pub q: Option<String>,
    /// Restrict to a folder path (e.g. "INBOX", "Sent Mail").
    #[serde(default)]
    pub folder: Option<String>,
    /// Only messages with header_date >= this ISO-8601 timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Only messages with header_date <= this ISO-8601 timestamp.
    #[serde(default)]
    pub before: Option<String>,
    /// Substring match on subject.
    #[serde(default)]
    pub subject: Option<String>,
    /// Substring match on From address.
    #[serde(default)]
    pub from: Option<String>,
    /// Page of results (default 1).
    #[serde(default)]
    pub page: Option<u32>,
    /// Results per page, 1-50 (default 10).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EmptyParams {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// Path within the backup tree, e.g.
    /// `sources/forwardemail/dan-hld.ca/calendars/` or
    /// `sources/forwardemail/dan-hld.ca/mail/INBOX/abc.json`.
    pub path: String,
    /// Max number of commits to return (default 20, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
}

// ── Write tool params ───────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateContactParams {
    /// Full name as it should appear in the contact.
    pub full_name: String,
    /// Email addresses, each with a type ("home", "work", etc.) and the address.
    pub emails: Vec<ContactEmail>,
    /// Free-text reason why you're making this change. Ends up in the git
    /// commit message for audit purposes. Be specific: "user asked me to
    /// add Alice to contacts" is better than "new contact".
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ContactEmail {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateContactParams {
    /// Forwardemail contact id.
    pub id: String,
    /// New full name.
    pub full_name: String,
    /// Optional etag from a previous get_contact call. If provided and
    /// stale, the update will fail with 412 Precondition Failed — use this
    /// to prevent clobbering concurrent edits.
    #[serde(default)]
    pub if_match: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteContactParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct InstallSieveParams {
    pub name: String,
    /// Sieve script source. Must be valid RFC 5228 sieve; forwardemail
    /// will parse and reject invalid scripts server-side.
    pub content: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateSieveParams {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteSieveParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetEmailParams {
    /// Canonical message id (the filename stem from the backup tree,
    /// e.g. as returned in search_email results or history).
    pub id: String,
    /// If true, include the raw RFC822 .eml bytes in the response
    /// (base64-encoded). Default false — returns parsed headers + meta
    /// only, which is usually enough for an AI agent.
    #[serde(default)]
    pub include_raw: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateDraftParams {
    /// Recipient email addresses (To:).
    pub to: Vec<String>,
    /// CC recipients. Optional.
    #[serde(default)]
    pub cc: Vec<String>,
    /// BCC recipients. Optional.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Email subject line.
    pub subject: String,
    /// Plain-text body. At least one of `text` or `html` should be
    /// provided.
    #[serde(default)]
    pub text: Option<String>,
    /// HTML body. Optional — if only `text` is provided, forwardemail
    /// uses it as the sole body part.
    #[serde(default)]
    pub html: Option<String>,
    /// Free-text reason why you're creating this draft. Ends up in the
    /// git commit message for audit purposes.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateFlagsParams {
    /// Forwardemail message id.
    pub id: String,
    /// New flag set — replaces existing flags entirely. Use IMAP flag names
    /// with backslashes: "\\Seen", "\\Flagged", "\\Answered", "\\Draft".
    pub flags: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MoveMessageParams {
    pub id: String,
    /// Target folder path, e.g. "Archive" or "INBOX".
    pub folder: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteMessageParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateEventParams {
    /// Calendar id (from list_calendars).
    pub calendar_id: String,
    /// Full iCalendar text, e.g.
    /// "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//foo//bar\nBEGIN:VEVENT\n
    ///  UID:abc@example\nDTSTAMP:20260101T000000Z\nSUMMARY:Lunch
    ///  DTSTART:20260115T120000Z\nDTEND:20260115T130000Z\n
    ///  END:VEVENT\nEND:VCALENDAR".
    /// Forwardemail parses and normalizes the ics server-side.
    pub ical: String,
    /// Optional client-provided event id. Forwardemail generates one if not
    /// provided. Useful for ensuring idempotence across retries.
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateEventParams {
    /// Forwardemail event id from list_events or create_event.
    pub id: String,
    /// New iCalendar text. Replaces the previous ics entirely.
    #[serde(default)]
    pub ical: Option<String>,
    /// Optionally move the event to a different calendar.
    #[serde(default)]
    pub target_calendar_id: Option<String>,
    /// Optimistic concurrency: pass the event's last-known etag (from a
    /// CalDAV-sourced pull) to get a 412 Precondition Failed if the
    /// server version has changed. Harmlessly ignored on REST backends
    /// that don't support If-Match for calendar events.
    #[serde(default)]
    pub if_match: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteEventParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

// ── Restore tool params ─────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreContactDryRunParams {
    /// iCard UID of the contact (the filename stem in the backup tree).
    /// Find it via list_contacts (uid field) or history (path like
    /// `sources/.../contacts/default/<uid>.vcf`).
    pub contact_uid: String,
    /// Git commit SHA to restore from. Use the `history` tool to find
    /// candidate commits.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreContactApplyParams {
    /// The RestorePlan object returned by restore_contact_dry_run. Must
    /// be passed back verbatim — any modification changes the plan_token
    /// and the apply will be refused.
    pub plan: serde_json::Value,
    /// The plan_token returned alongside the plan in the dry-run response.
    pub plan_token: String,
    /// Free-text reason, embedded in the audit commit.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreSieveDryRunParams {
    /// Sieve script name (the filename stem in the backup tree).
    pub script_name: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreSieveApplyParams {
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreCalendarDryRunParams {
    /// Calendar id (the directory name under the backup tree).
    pub calendar_id: String,
    /// Event UID from the VEVENT component (the .ics filename stem).
    pub event_uid: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreCalendarApplyParams {
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreMailDryRunParams {
    /// Folder path, e.g. "INBOX" or "Sent Mail".
    pub folder: String,
    /// Forwardemail message id.
    pub message_id: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreMailApplyParams {
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestorePathDryRunParams {
    /// Backup-tree path prefix to restore, e.g.
    /// `sources/forwardemail/<alias>/contacts/` to restore all contacts,
    /// or `sources/forwardemail/<alias>/calendars/<id>/events/` for one
    /// calendar.
    pub path_prefix: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestorePathApplyParams {
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[tool_router]
impl PimstewardServer {
    #[tool(
        name = "search_email",
        description = "Search email messages via forwardemail's native search. Filter by folder, date range, subject, from, or free-text. Returns message summaries without bodies — use get_email for full content."
    )]
    async fn search_email(
        &self,
        Parameters(p): Parameters<SearchEmailParams>,
    ) -> Result<String, McpError> {
        // If folder is specified, honor per-folder scoped permission.
        // If not, fall back to email resource-level default.
        self.check_scoped(Scope::Email {
            folder: p.folder.as_deref(),
        })?;

        // Build a query string from the optional params. The pass-through is
        // intentionally simple so the AI can learn the parameter set.
        let mut parts = Vec::new();
        if let Some(q) = p.q {
            parts.push(format!("q={}", urlenc(&q)));
        }
        if let Some(f) = p.folder {
            parts.push(format!("folder={}", urlenc(&f)));
        }
        if let Some(s) = p.since {
            parts.push(format!("since={}", urlenc(&s)));
        }
        if let Some(b) = p.before {
            parts.push(format!("before={}", urlenc(&b)));
        }
        if let Some(s) = p.subject {
            parts.push(format!("subject={}", urlenc(&s)));
        }
        if let Some(f) = p.from {
            parts.push(format!("from={}", urlenc(&f)));
        }
        parts.push(format!("page={}", p.page.unwrap_or(1)));
        parts.push(format!("limit={}", p.limit.unwrap_or(10).clamp(1, 50)));
        let path = format!("/v1/messages?{}", parts.join("&"));

        let v: serde_json::Value = self
            .inner
            .client
            .raw_get_json(&path)
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&v).map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "get_email",
        description = "Get a single email message by canonical id. Returns parsed headers, metadata (flags, folder, modseq), and optionally the raw RFC822 bytes. The canonical id is the filename stem from the backup tree (16 hex chars)."
    )]
    async fn get_email(
        &self,
        Parameters(p): Parameters<GetEmailParams>,
    ) -> Result<String, McpError> {
        let meta = self.lookup_message_meta(&p.id)?;
        let folder = meta.folder_path.as_deref().unwrap_or("INBOX");
        self.check_scoped(Scope::Email {
            folder: Some(folder),
        })?;

        // Find the .eml in the backup tree.
        let mail_root = self
            .inner
            .repo
            .root()
            .join(format!("sources/forwardemail/{}/mail", self.inner.alias));
        let folder_safe = folder.replace('/', "_");
        let eml_path = mail_root.join(&folder_safe).join(format!("{}.eml", p.id));

        let mut result = serde_json::json!({
            "canonical_id": p.id,
            "source_id": meta.id,
            "folder": folder,
            "flags": meta.flags,
            "modseq": meta.modseq,
            "uid": meta.uid,
        });
        let obj = result.as_object_mut().expect("just created");

        // Parse key headers from the .eml for the AI without needing
        // the full raw bytes.
        if let Ok(raw) = std::fs::read(&eml_path) {
            if let Ok(text) = std::str::from_utf8(&raw) {
                let mut headers = serde_json::Map::new();
                for line in text.lines() {
                    if line.is_empty() {
                        break; // end of headers
                    }
                    if let Some((key, val)) = line.split_once(':') {
                        let k = key.trim().to_lowercase();
                        if matches!(
                            k.as_str(),
                            "from" | "to" | "cc" | "subject" | "date" | "message-id" | "in-reply-to" | "references"
                        ) {
                            headers.insert(k, serde_json::Value::String(val.trim().to_string()));
                        }
                    }
                }
                obj.insert("headers".into(), serde_json::Value::Object(headers));

                // Extract plain-text body (everything after the blank line
                // separating headers from body). For MIME messages this is
                // a simplification — the full .eml is available via
                // include_raw for proper MIME parsing.
                if let Some(body_start) = text.find("\r\n\r\n").or_else(|| text.find("\n\n")) {
                    let offset = if text[body_start..].starts_with("\r\n\r\n") { 4 } else { 2 };
                    let body = &text[body_start + offset..];
                    // Truncate to ~4k chars to avoid blowing up the MCP response.
                    let truncated = if body.len() > 4096 {
                        format!("{}…[truncated, {} bytes total]", &body[..4096], body.len())
                    } else {
                        body.to_string()
                    };
                    obj.insert("body_preview".into(), serde_json::Value::String(truncated));
                }

                if p.include_raw {
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
                    obj.insert("raw_base64".into(), serde_json::Value::String(b64));
                }
            }
        }

        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_folders",
        description = "List mailbox folders for the authenticated alias, including special_use markers (Inbox, Drafts, Sent, Junk, Trash, Archive) and uid_validity."
    )]
    async fn list_folders(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Email)?;
        let folders = self
            .inner
            .client
            .list_folders()
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&folders)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_calendars",
        description = "List calendars for the authenticated alias with name, color, timezone."
    )]
    async fn list_calendars(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Calendar)?;
        let cals = self
            .inner
            .client
            .list_calendars()
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&cals)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_events",
        description = "List calendar events. Returns event JSON including the raw iCalendar content in the `content` field."
    )]
    async fn list_events(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Calendar)?;
        let events = self
            .inner
            .client
            .list_calendar_events(None)
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&events)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_contacts",
        description = "List contacts for the authenticated alias. Each contact includes the raw vCard in the `content` field and the CardDAV etag."
    )]
    async fn list_contacts(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Contacts)?;
        let contacts = self
            .inner
            .client
            .list_contacts()
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&contacts)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "list_sieve",
        description = "List server-side sieve filter scripts for the alias with their activation state and validation status."
    )]
    async fn list_sieve(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Sieve)?;
        let scripts = self
            .inner
            .client
            .list_sieve_scripts()
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&scripts)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    // ── Write tools (require readwrite permission) ───────────────────

    #[tool(
        name = "create_contact",
        description = "Create a new contact. Requires readwrite permission on contacts. Every write produces a git commit attributed to the caller with the `reason` in the commit message."
    )]
    async fn create_contact(
        &self,
        Parameters(p): Parameters<CreateContactParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let attr = self.attribution(None, p.reason);
        let emails: Vec<(&str, &str)> = p
            .emails
            .iter()
            .map(|e| (e.kind.as_str(), e.value.as_str()))
            .collect();
        let created = crate::write::contacts::create_contact(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.full_name,
            &emails,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&created)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "update_contact",
        description = "Update a contact's full_name. If `if_match` is provided, forwardemail uses it for optimistic concurrency — stale etags return 412. Requires readwrite permission."
    )]
    async fn update_contact(
        &self,
        Parameters(p): Parameters<UpdateContactParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let attr = self.attribution(None, p.reason);
        let updated = crate::write::contacts::update_contact_name(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
            &p.full_name,
            p.if_match.as_deref(),
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&updated)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "delete_contact",
        description = "Delete a contact by id. Destructive — the deletion is captured in git so a restore tool can bring it back."
    )]
    async fn delete_contact(
        &self,
        Parameters(p): Parameters<DeleteContactParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let attr = self.attribution(None, p.reason);
        crate::write::contacts::delete_contact(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("deleted contact {}", p.id))
    }

    #[tool(
        name = "install_sieve_script",
        description = "Install a new sieve filter script. Forwardemail parses the script server-side and rejects invalid syntax, giving dry-run validation for free."
    )]
    async fn install_sieve_script(
        &self,
        Parameters(p): Parameters<InstallSieveParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let attr = self.attribution(None, p.reason);
        let created = crate::write::sieve::install_sieve_script(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.name,
            &p.content,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&created)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "update_sieve_script",
        description = "Update an existing sieve script's content."
    )]
    async fn update_sieve_script(
        &self,
        Parameters(p): Parameters<UpdateSieveParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let attr = self.attribution(None, p.reason);
        let updated = crate::write::sieve::update_sieve_script(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
            &p.content,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&updated)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "delete_sieve_script",
        description = "Delete a sieve script by id."
    )]
    async fn delete_sieve_script(
        &self,
        Parameters(p): Parameters<DeleteSieveParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let attr = self.attribution(None, p.reason);
        crate::write::sieve::delete_sieve_script(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("deleted sieve script {}", p.id))
    }

    #[tool(
        name = "create_draft",
        description = "Create a new draft email in the Drafts folder. Provide recipients, subject, and body (text and/or html). The draft appears in the user's Drafts folder ready for review and manual sending. Does NOT send the email — only saves a draft."
    )]
    async fn create_draft(
        &self,
        Parameters(p): Parameters<CreateDraftParams>,
    ) -> Result<String, McpError> {
        // Drafts go into the special-use Drafts folder. Gate on write
        // access to that folder — the motivating use case is
        // default=read + Drafts=read_write.
        let folder = "Drafts";
        self.check_write_scoped(Scope::Email {
            folder: Some(folder),
        })?;
        let attr = self.attribution(None, p.reason);
        let msg = crate::forwardemail::writes::NewMessage {
            folder: folder.to_string(),
            to: p.to,
            cc: p.cc,
            bcc: p.bcc,
            subject: p.subject,
            text: p.text,
            html: p.html,
        };
        let result = crate::write::mail::create_draft(
            &self.inner.client,
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &msg,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let id = result
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        Ok(format!("draft created: {id} in {folder}"))
    }

    #[tool(
        name = "update_email_flags",
        description = "Replace a message's flag set. Use IMAP flag names with backslash-escaping: \\Seen, \\Flagged, \\Answered, \\Draft. Passing an empty list clears flags."
    )]
    async fn update_email_flags(
        &self,
        Parameters(p): Parameters<UpdateFlagsParams>,
    ) -> Result<String, McpError> {
        // Scoped-only: look up the message's current folder and gate on
        // write access *to that folder*. This makes per-folder overrides
        // (e.g. default=read + Drafts=read_write) actually work for flag
        // updates, which don't carry a folder in their params.
        let meta = self.lookup_message_meta(&p.id)?;
        let folder = meta.folder_path.as_deref().unwrap_or("INBOX");
        self.check_write_scoped(Scope::Email {
            folder: Some(folder),
        })?;
        let attr = self.attribution(None, p.reason);
        crate::write::mail::update_flags(
            self.inner.mail_writer.as_ref(),
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            folder,
            &meta.id,
            &p.flags,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("updated flags on {}", p.id))
    }

    #[tool(
        name = "move_email",
        description = "Move a message to a different folder by path."
    )]
    async fn move_email(
        &self,
        Parameters(p): Parameters<MoveMessageParams>,
    ) -> Result<String, McpError> {
        // A move is a write on BOTH the source folder (removing the
        // message) and the target folder (adding it). Check both. The
        // source folder is looked up from the message; the target comes
        // from params.
        let meta = self.lookup_message_meta(&p.id)?;
        let source_folder = meta.folder_path.as_deref().unwrap_or("INBOX");
        self.check_write_scoped(Scope::Email {
            folder: Some(source_folder),
        })?;
        self.check_write_scoped(Scope::Email {
            folder: Some(&p.folder),
        })?;
        let attr = self.attribution(None, p.reason);
        crate::write::mail::move_message(
            self.inner.mail_writer.as_ref(),
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            source_folder,
            &meta.id,
            &p.folder,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("moved {} to {}", p.id, p.folder))
    }

    #[tool(name = "delete_email", description = "Delete a message by id.")]
    async fn delete_email(
        &self,
        Parameters(p): Parameters<DeleteMessageParams>,
    ) -> Result<String, McpError> {
        // Scoped-only: delete is a write on the folder currently holding
        // the message. Look it up and gate per-folder so Trash=none (or
        // any other per-folder rule) is actually honoured.
        let meta = self.lookup_message_meta(&p.id)?;
        let folder = meta.folder_path.as_deref().unwrap_or("INBOX");
        self.check_write_scoped(Scope::Email {
            folder: Some(folder),
        })?;
        let attr = self.attribution(None, p.reason);
        crate::write::mail::delete_message(
            self.inner.mail_writer.as_ref(),
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            folder,
            &meta.id,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("deleted {}", p.id))
    }

    #[tool(
        name = "create_event",
        description = "Create a calendar event. Requires `ical` — the full iCalendar text including BEGIN:VCALENDAR and a VEVENT. Forwardemail normalizes the ics server-side. Requires readwrite on calendar."
    )]
    async fn create_event(
        &self,
        Parameters(p): Parameters<CreateEventParams>,
    ) -> Result<String, McpError> {
        // Per-calendar scoped check: the target calendar id is in the params.
        self.check_write_scoped(Scope::Calendar {
            calendar_id: Some(&p.calendar_id),
        })?;
        let attr = self.attribution(None, p.reason);
        let created = crate::write::calendar::create_event(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.calendar_id,
            &p.ical,
            p.event_id.as_deref(),
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&created)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "update_event",
        description = "Update a calendar event's iCal payload and/or move it to a different calendar. Either `ical` or `target_calendar_id` (or both) must be provided."
    )]
    async fn update_event(
        &self,
        Parameters(p): Parameters<UpdateEventParams>,
    ) -> Result<String, McpError> {
        // If moving to a new calendar, require write access on the target
        // calendar. Otherwise fall back to resource-level write check since
        // we don't know the source calendar without a lookup (and don't
        // want to force an extra API call per mutation).
        if let Some(ref target) = p.target_calendar_id {
            self.check_write_scoped(Scope::Calendar {
                calendar_id: Some(target),
            })?;
        } else {
            self.check_write(Resource::Calendar)?;
        }
        if p.ical.is_none() && p.target_calendar_id.is_none() {
            return Err(McpError::invalid_params(
                "update_event requires at least one of `ical` or `target_calendar_id`",
                None,
            ));
        }
        let attr = self.attribution(None, p.reason);
        let updated = crate::write::calendar::update_event(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
            p.ical.as_deref(),
            p.target_calendar_id.as_deref(),
            p.if_match.as_deref(),
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&updated)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(name = "delete_event", description = "Delete a calendar event by id.")]
    async fn delete_event(
        &self,
        Parameters(p): Parameters<DeleteEventParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Calendar)?;
        let attr = self.attribution(None, p.reason);
        crate::write::calendar::delete_event(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.id,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("deleted event {}", p.id))
    }

    // ── Restore tools (always available, gated by the two-call dance) ──

    #[tool(
        name = "restore_contact_dry_run",
        description = "Compute what it would take to restore a contact back to its state at a past git commit. Returns a RestorePlan object plus a plan_token. This does NOT execute the restore — it's the safe dry-run step. Pass the returned plan and plan_token verbatim to restore_contact_apply to actually apply."
    )]
    async fn restore_contact_dry_run(
        &self,
        Parameters(p): Parameters<RestoreContactDryRunParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let (plan, token) = crate::restore::plan_contact(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.contact_uid,
            &p.at_sha,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({
            "plan": plan,
            "plan_token": token,
            "note": "Pass `plan` and `plan_token` verbatim to restore_contact_apply to execute."
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "restore_contact_apply",
        description = "Execute a restore plan returned by restore_contact_dry_run. Re-computes plan_token from the submitted plan and refuses to proceed if it doesn't match the supplied token — this prevents dry-running one plan and applying a different one."
    )]
    async fn restore_contact_apply(
        &self,
        Parameters(p): Parameters<RestoreContactApplyParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let plan: crate::restore::RestorePlan = serde_json::from_value(p.plan).map_err(|e| {
            McpError::invalid_params(format!("plan is not a valid RestorePlan: {e}"), None)
        })?;
        let attr = self.attribution(None, p.reason);
        crate::restore::apply_contact(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &plan,
            &p.plan_token,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!(
            "restore applied: contact {} from {}",
            plan.contact_uid,
            &plan.at_sha[..8.min(plan.at_sha.len())]
        ))
    }

    // ── Sieve restore ──

    #[tool(
        name = "restore_sieve_dry_run",
        description = "Compute what it would take to restore a sieve script back to its historical state. Returns a plan + plan_token for the apply step."
    )]
    async fn restore_sieve_dry_run(
        &self,
        Parameters(p): Parameters<RestoreSieveDryRunParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let (plan, token) = crate::restore::sieve::plan_sieve(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.script_name,
            &p.at_sha,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({"plan": plan, "plan_token": token});
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "restore_sieve_apply",
        description = "Execute a sieve restore plan. Re-computes plan_token and refuses on mismatch."
    )]
    async fn restore_sieve_apply(
        &self,
        Parameters(p): Parameters<RestoreSieveApplyParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let plan: crate::restore::sieve::SieveRestorePlan = serde_json::from_value(p.plan)
            .map_err(|e| {
                McpError::invalid_params(format!("plan is not a SieveRestorePlan: {e}"), None)
            })?;
        let attr = self.attribution(None, p.reason);
        crate::restore::sieve::apply_sieve(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &plan,
            &p.plan_token,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!("restore applied: sieve/{}", plan.script_name))
    }

    // ── Calendar restore ──

    #[tool(
        name = "restore_calendar_event_dry_run",
        description = "Compute what it would take to restore a calendar event back to its historical state. Returns a plan + plan_token."
    )]
    async fn restore_calendar_event_dry_run(
        &self,
        Parameters(p): Parameters<RestoreCalendarDryRunParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Calendar)?;
        let (plan, token) = crate::restore::calendar::plan_calendar(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.calendar_id,
            &p.event_uid,
            &p.at_sha,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({"plan": plan, "plan_token": token});
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "restore_calendar_event_apply",
        description = "Execute a calendar event restore plan."
    )]
    async fn restore_calendar_event_apply(
        &self,
        Parameters(p): Parameters<RestoreCalendarApplyParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Calendar)?;
        let plan: crate::restore::calendar::CalendarRestorePlan = serde_json::from_value(p.plan)
            .map_err(|e| {
                McpError::invalid_params(format!("plan is not a CalendarRestorePlan: {e}"), None)
            })?;
        let attr = self.attribution(None, p.reason);
        crate::restore::calendar::apply_calendar(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &plan,
            &p.plan_token,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!(
            "restore applied: calendar/{}/{}",
            plan.calendar_id, plan.event_uid
        ))
    }

    // ── Mail restore (flags + folder only — body is immutable) ──

    #[tool(
        name = "restore_mail_dry_run",
        description = "Compute what it would take to restore a message's flags + folder to their historical state. Mail body cannot be restored automatically (forwardemail API silently ignores body rewrites). If the message has been deleted from forwardemail, the plan will mark it as Unrestorable."
    )]
    async fn restore_mail_dry_run(
        &self,
        Parameters(p): Parameters<RestoreMailDryRunParams>,
    ) -> Result<String, McpError> {
        // Scoped to the folder named in the plan params — restore is a
        // write against that folder (flags and/or folder movement).
        self.check_write_scoped(Scope::Email {
            folder: Some(&p.folder),
        })?;
        let (plan, token) = crate::restore::mail::plan_mail(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.folder,
            &p.message_id,
            &p.at_sha,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({"plan": plan, "plan_token": token});
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "restore_mail_apply",
        description = "Execute a mail restore plan (flags or folder only)."
    )]
    async fn restore_mail_apply(
        &self,
        Parameters(p): Parameters<RestoreMailApplyParams>,
    ) -> Result<String, McpError> {
        let plan: crate::restore::mail::MailRestorePlan =
            serde_json::from_value(p.plan).map_err(|e| {
                McpError::invalid_params(format!("plan is not a MailRestorePlan: {e}"), None)
            })?;
        // Restore writes against `plan.folder` (the historical home of
        // the message). If the operation also targets a different folder
        // (MoveBack, Append into a different destination), check that
        // one too so cross-folder restores respect per-folder rules.
        self.check_write_scoped(Scope::Email {
            folder: Some(&plan.folder),
        })?;
        let extra_target: Option<&str> = match &plan.operation {
            crate::restore::mail::MailOperation::MoveBack { target_folder } => Some(target_folder),
            crate::restore::mail::MailOperation::Append { target_folder, .. } => {
                Some(target_folder)
            }
            _ => None,
        };
        if let Some(target) = extra_target {
            if target != plan.folder {
                self.check_write_scoped(Scope::Email {
                    folder: Some(target),
                })?;
            }
        }
        let attr = self.attribution(None, p.reason);
        crate::restore::mail::apply_mail(
            &self.inner.client,
            self.inner.mail_writer.as_ref(),
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &plan,
            &p.plan_token,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        Ok(format!(
            "restore applied: mail/{}/{}",
            plan.folder, plan.message_id
        ))
    }

    // ── Bulk restore ──

    #[tool(
        name = "restore_path_dry_run",
        description = "Compute a bulk restore plan covering every contact, sieve script, and calendar event under a path prefix in the backup tree. Mail is excluded from bulk restore (body is immutable, partial failures confusing) — use restore_mail_* for individual messages. Returns a BulkRestorePlan with per-resource sub-plans and a deterministic plan_token."
    )]
    async fn restore_path_dry_run(
        &self,
        Parameters(p): Parameters<RestorePathDryRunParams>,
    ) -> Result<String, McpError> {
        // Permission: require readwrite on all three covered resources so
        // the plan computation can't be used to enumerate forbidden data.
        self.check_write(Resource::Contacts)?;
        self.check_write(Resource::Sieve)?;
        self.check_write(Resource::Calendar)?;

        let (plan, token) = crate::restore::bulk::plan_bulk(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.path_prefix,
            &p.at_sha,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({
            "plan": plan,
            "plan_token": token,
            "total_operations": plan.total_ops(),
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "restore_path_apply",
        description = "Execute a bulk restore plan. Re-verifies the bulk plan_token AND each sub-plan's individual token before executing. Continues past individual sub-plan failures and returns a summary of what succeeded and what failed."
    )]
    async fn restore_path_apply(
        &self,
        Parameters(p): Parameters<RestorePathApplyParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        self.check_write(Resource::Sieve)?;
        self.check_write(Resource::Calendar)?;

        let plan: crate::restore::bulk::BulkRestorePlan =
            serde_json::from_value(p.plan).map_err(|e| {
                McpError::invalid_params(format!("plan is not a BulkRestorePlan: {e}"), None)
            })?;
        let attr = self.attribution(None, p.reason);
        let result = crate::restore::bulk::apply_bulk(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &plan,
            &p.plan_token,
        )
        .await
        .map_err(|e| self.api_error(e))?;
        let out = serde_json::json!({
            "path_prefix": plan.path_prefix,
            "at_sha": plan.at_sha,
            "contacts_ok": result.contacts_ok,
            "sieve_ok": result.sieve_ok,
            "calendar_ok": result.calendar_ok,
            "total_ok": result.total_ok(),
            "errors": result.errors,
        });
        serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    // ── History / audit ──────────────────────────────────────────────

    #[tool(
        name = "history",
        description = "Git log for a path in the pimsteward backup tree. Shows commits that touched the file or directory, newest first. Use this to see who changed what and when, including AI-attributed mutations."
    )]
    async fn history(&self, Parameters(p): Parameters<HistoryParams>) -> Result<String, McpError> {
        let limit = p.limit.unwrap_or(20).clamp(1, 200);
        // Path traversal guard — the path must not escape the repo root
        // (e.g. "../../etc/passwd"). gitoxide would reject this too but
        // rejecting early gives a clearer error.
        if p.path.contains("..") {
            return Err(McpError::invalid_params("path must not contain '..'", None));
        }

        let out = Command::new("git")
            .args([
                "log",
                &format!("-{limit}"),
                "--pretty=format:%H%x09%an%x09%ad%x09%s",
                "--date=iso-strict",
                "--",
                &p.path,
            ])
            .current_dir(self.inner.repo.root())
            .output()
            .map_err(|e| McpError::internal_error(format!("git log: {e}"), None))?;

        if !out.status.success() {
            return Err(McpError::internal_error(
                format!("git log failed: {}", String::from_utf8_lossy(&out.stderr)),
                None,
            ));
        }

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        Ok(stdout)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PimstewardServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            format!(
                "pimsteward — permission-aware PIM mediator for forwardemail.net.\n\
                 Alias: {}\n\
                 Read, write, and restore tools for email, calendar, contacts, \
                 and sieve scripts, gated by the configured permission matrix. \
                 Every mutation produces an attributed git commit in the backup repo.",
                self.inner.alias
            ),
        )
    }
}

/// Minimal URL component encoder for query string values. We intentionally
/// avoid pulling in a full urlencoding crate for three call sites.
fn urlenc(s: &str) -> String {
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
