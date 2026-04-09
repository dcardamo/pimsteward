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
    model::{ErrorCode, ServerCapabilities, ServerInfo},
    schemars, tool, tool_router, ErrorData as McpError, ServerHandler,
};
use std::process::Command;
use std::sync::Arc;

/// Schema for `plan` fields in *ApplyParams. serde_json::Value generates
/// `true` (accept anything) via schemars, which is valid JSON Schema 2020-12
/// but Claude Code's MCP client rejects it, causing zero tools to register.
/// This explicit schema says "any JSON object" instead.
fn plan_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::json!({"type": "object"}).as_object().unwrap().clone().into()
}

/// JSON-RPC server-specific error code (the -32000..-32099 band is reserved
/// for application-defined errors by the JSON-RPC 2.0 spec). We use -32001
/// for "permission denied" so the LLM can distinguish a policy refusal from
/// a bad-params error (which would otherwise tempt it to retry with
/// different arguments).
pub const PERMISSION_DENIED_CODE: ErrorCode = ErrorCode(-32001);

fn perm_denied(msg: impl Into<std::borrow::Cow<'static, str>>) -> McpError {
    McpError::new(PERMISSION_DENIED_CODE, msg, None)
}

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
    /// Default caller name attributed to writes initiated through this
    /// server. Set from `PIMSTEWARD_CALLER` (or `config.mcp.caller`) at
    /// startup; defaults to `"ai"` when unset. Lets operators distinguish
    /// multiple assistants talking to the same backup repo in `git log`.
    caller: String,
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
        caller: String,
        mail_source: Arc<dyn MailSource>,
        mail_writer: Arc<dyn MailWriter>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                repo,
                permissions,
                alias,
                caller,
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
            .map_err(|e| perm_denied(format!("permission denied: {e}")))
    }

    fn check_write(&self, resource: Resource) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_write(resource)
            .map_err(|e| perm_denied(format!("permission denied: {e}")))
    }

    /// Scoped read check — per-folder for email, per-calendar for calendar.
    /// Passing `None` for the scope target behaves identically to
    /// [`Self::check`].
    fn check_scoped(&self, scope: Scope<'_>) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_read_scoped(&scope)
            .map_err(|e| perm_denied(format!("permission denied: {e}")))
    }

    /// Scoped write check.
    fn check_write_scoped(&self, scope: Scope<'_>) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_write_scoped(&scope)
            .map_err(|e| perm_denied(format!("permission denied: {e}")))
    }

    /// Gate an outgoing SMTP send. Deliberately separate from
    /// `check_write_scoped(Scope::Email { ... })` because send is its
    /// own permission — `read_write` on email does NOT imply send.
    fn check_email_send(&self) -> Result<(), McpError> {
        self.inner
            .permissions
            .check_email_send()
            .map_err(|e| perm_denied(format!("permission denied: {e}")))
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
            .join("mail");
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
        Attribution::new(
            caller.unwrap_or_else(|| self.inner.caller.clone()),
            reason,
        )
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
pub struct ListEventsParams {
    /// Optional calendar id to restrict the listing to. If omitted,
    /// events from all calendars the caller can read are returned; any
    /// events belonging to a calendar the caller does NOT have read
    /// access to are filtered out (rather than failing the whole call).
    #[serde(default)]
    pub calendar_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    /// Path within the backup tree, e.g.
    /// `calendars/` or `mail/INBOX/abc.json`.
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
    /// New full name. Required when `vcard` is omitted. When `vcard` is
    /// provided, `full_name` is optional and extracted from the card's
    /// `FN:` line.
    #[serde(default)]
    pub full_name: Option<String>,
    /// Optional full vCard (3.0 or 4.0) replacement. Pass this to update
    /// structured fields that `full_name` can't express: emails, phones,
    /// addresses, org, notes, etc. Forwardemail parses the card
    /// server-side and rewrites the contact from it. If both `full_name`
    /// and `vcard` are supplied the vCard wins; the `full_name` field is
    /// ignored (the FN line in the card is authoritative). Read the
    /// current card via `list_contacts`, edit the fields you want to
    /// change, and pass the result back.
    #[serde(default)]
    pub vcard: Option<String>,
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
pub struct SendEmailParams {
    /// Recipient email addresses (To:). Must be non-empty.
    pub to: Vec<String>,
    /// CC recipients. Optional.
    #[serde(default)]
    pub cc: Vec<String>,
    /// BCC recipients. Optional.
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Email subject line.
    pub subject: String,
    /// Plain-text body. At least one of `text` or `html` must be provided.
    #[serde(default)]
    pub text: Option<String>,
    /// HTML body. Optional.
    #[serde(default)]
    pub html: Option<String>,
    /// REQUIRED. Free-text reason explaining *why* this message is being
    /// sent. Send is an irreversible outbound action — the reason lands
    /// in the git audit trail so you (or a reviewer) can reconstruct the
    /// intent later. Must be non-empty; the server refuses the call
    /// otherwise.
    pub reason: String,
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
    /// Event title / SUMMARY. Usually the only field the user phrases
    /// directly ("book lunch with Sam").
    #[serde(default)]
    pub summary: Option<String>,
    /// Start timestamp. Accepts either an RFC 3339 / ISO-8601 instant
    /// (`2026-01-15T12:00:00Z`, `2026-01-15T08:00:00-04:00`) for a
    /// timed event, or a bare `YYYY-MM-DD` for an all-day event.
    /// Required unless you pass a raw `ical` override.
    #[serde(default)]
    pub start: Option<String>,
    /// End timestamp. Same format as `start`. For timed events: exclusive
    /// end instant. For all-day events: the DTEND date (exclusive — per
    /// RFC 5545 an all-day event ending on the same day as its start
    /// needs DTEND = start + 1 day).
    #[serde(default)]
    pub end: Option<String>,
    /// Optional description / notes. Free-text, multi-line OK.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional location string ("Conference room B", a URL, an address).
    #[serde(default)]
    pub location: Option<String>,
    /// Optional attendee email addresses. Each becomes an ATTENDEE line
    /// in the VEVENT.
    #[serde(default)]
    pub attendees: Vec<String>,
    /// Power-user override: if you'd rather hand-craft the iCalendar text
    /// (e.g. to set RRULE, VALARM, or other properties the structured
    /// fields don't expose), pass the full VCALENDAR/VEVENT block here
    /// and every structured field above is ignored. Forwardemail
    /// normalizes the ics server-side.
    #[serde(default)]
    pub ical: Option<String>,
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
    /// `contacts/default/<uid>.vcf`).
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
    #[schemars(schema_with = "plan_schema")]
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
    #[schemars(schema_with = "plan_schema")]
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
    #[schemars(schema_with = "plan_schema")]
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreMailDryRunParams {
    /// Folder path, e.g. "INBOX" or "Sent Mail". This should be the
    /// folder the message lived in AT `at_sha` (its historical home),
    /// not necessarily its current folder.
    pub folder: String,
    /// Message id — accepts either a canonical id (16-char hex filename
    /// stem, as returned by `search_email` / `get_email` / `history`) OR
    /// the forwardemail source id. The plan computation resolves both.
    /// Use the same id you'd pass to `get_email`.
    pub id: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestoreMailApplyParams {
    #[schemars(schema_with = "plan_schema")]
    pub plan: serde_json::Value,
    pub plan_token: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestorePathDryRunParams {
    /// Backup-tree path prefix to restore, e.g. `contacts/` to restore
    /// all contacts, or `calendars/<id>/events/` for one calendar.
    pub path_prefix: String,
    /// Git commit SHA to restore from.
    pub at_sha: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RestorePathApplyParams {
    #[schemars(schema_with = "plan_schema")]
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
            .join("mail");
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
                let (headers_map, content_type) = parse_headers(text);
                obj.insert("headers".into(), serde_json::Value::Object(headers_map));

                // Extract the best plain-text preview we can. For
                // multipart/alternative (the common HTML-first layout) we
                // pick the first text/plain part; if absent, fall back to
                // stripping tags from the first text/html part. For
                // single-part bodies we return the body verbatim. Full
                // MIME parsing is deferred to the caller via include_raw.
                let body_text = extract_body_preview(text, content_type.as_deref());
                let truncated = if body_text.len() > 4096 {
                    format!(
                        "{}…[truncated, {} bytes total]",
                        &body_text[..4096.min(body_text.len())],
                        body_text.len()
                    )
                } else {
                    body_text
                };
                obj.insert("body_preview".into(), serde_json::Value::String(truncated));

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
        description = "List calendar events. Pass `calendar_id` to restrict to one calendar (checked against scoped per-calendar permissions). With no `calendar_id`, events are returned for every calendar the caller has read access to — events in calendars you can't read are filtered out silently rather than failing the call. Returns event JSON including the raw iCalendar text."
    )]
    async fn list_events(
        &self,
        Parameters(p): Parameters<ListEventsParams>,
    ) -> Result<String, McpError> {
        // Scoped path: caller asked for one calendar. Gate that specific
        // calendar id and pass it through to forwardemail as a filter.
        if let Some(ref cal_id) = p.calendar_id {
            self.check_scoped(Scope::Calendar {
                calendar_id: Some(cal_id),
            })?;
            let events = self
                .inner
                .client
                .list_calendar_events(Some(cal_id))
                .await
                .map_err(|e| self.api_error(e))?;
            return serde_json::to_string_pretty(&events)
                .map_err(|e| McpError::internal_error(e.to_string(), None));
        }

        // Unscoped path: the caller wants everything they can see. We need
        // at least one readable calendar — default OR an override. If
        // nothing is readable, refuse. If *something* is readable, fetch
        // the full list and filter in-memory to the allowed set.
        if !self.inner.permissions.has_any_read(Resource::Calendar) {
            return Err(perm_denied(
                "permission denied: calendar read access not granted in any scope",
            ));
        }
        let events = self
            .inner
            .client
            .list_calendar_events(None)
            .await
            .map_err(|e| self.api_error(e))?;
        let filtered: Vec<_> = events
            .into_iter()
            .filter(|ev| {
                self.inner
                    .permissions
                    .check_read_scoped(&Scope::Calendar {
                        calendar_id: ev.calendar_id.as_deref(),
                    })
                    .is_ok()
            })
            .collect();
        serde_json::to_string_pretty(&filtered)
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
        description = "Update a contact. Two modes:\n\n• **Name only.** Pass `full_name` to rename. This is the quick path for fixing typos — it does not touch emails, phones, or any other field.\n\n• **Full vCard replacement.** Pass `vcard` with a complete vCard 3.0/4.0 card to update structured fields like emails, phones, addresses, org, or notes. Read the current card via `list_contacts`, edit it, and pass it back. Forwardemail parses the card server-side. If both `full_name` and `vcard` are supplied, the vCard's FN line wins.\n\n`if_match` carries an etag for optimistic concurrency — stale etags return 412. Requires readwrite permission."
    )]
    async fn update_contact(
        &self,
        Parameters(p): Parameters<UpdateContactParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Contacts)?;
        let attr = self.attribution(None, p.reason);
        let updated = if let Some(vcard) = p.vcard.as_deref() {
            crate::write::contacts::update_contact_vcard(
                &self.inner.client,
                &self.inner.repo,
                &self.inner.alias,
                &attr,
                &p.id,
                vcard,
                p.if_match.as_deref(),
            )
            .await
            .map_err(|e| self.api_error(e))?
        } else {
            let full_name = p.full_name.as_deref().ok_or_else(|| {
                McpError::invalid_params(
                    "update_contact: either `full_name` or `vcard` must be provided",
                    None,
                )
            })?;
            crate::write::contacts::update_contact_name(
                &self.inner.client,
                &self.inner.repo,
                &self.inner.alias,
                &attr,
                &p.id,
                full_name,
                p.if_match.as_deref(),
            )
            .await
            .map_err(|e| self.api_error(e))?
        };
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
        name = "send_email",
        description = "Send an email over SMTP via forwardemail's outgoing bridge. IRREVERSIBLE: once this returns success, the message has been accepted for delivery to third parties and there is no 'undo'. A copy is saved to the Sent folder automatically and captured into git on the next pull. Every send is recorded in the git audit log with tool=send_email plus recipients, subject, and body sha256 — `git log --grep='tool: send_email'` enumerates them.\n\nRequires the separate `email_send` permission (default: denied). Granting `email = \"read_write\"` does NOT grant send — you must set `email_send = \"allowed\"` in [permissions] explicitly. If you only want the assistant to prepare outgoing mail for human review, use `create_draft` instead; drafts are safely reversible."
    )]
    async fn send_email(
        &self,
        Parameters(p): Parameters<SendEmailParams>,
    ) -> Result<String, McpError> {
        // Independent permission check — does NOT piggyback on email
        // read/write. See permission::SendPermission for the rationale.
        self.check_email_send()?;

        // Minimal structural validation at the MCP layer so the model
        // gets a useful error instead of a 400 from forwardemail.
        if p.to.is_empty() {
            return Err(McpError::invalid_params(
                "send_email: `to` must contain at least one recipient",
                None,
            ));
        }
        if p.text.is_none() && p.html.is_none() {
            return Err(McpError::invalid_params(
                "send_email: at least one of `text` or `html` must be provided",
                None,
            ));
        }
        if p.subject.trim().is_empty() {
            return Err(McpError::invalid_params(
                "send_email: `subject` must not be empty",
                None,
            ));
        }
        if p.reason.trim().is_empty() {
            return Err(McpError::invalid_params(
                "send_email: `reason` is required and must not be empty — it lands in the git audit trail as the justification for this irreversible send",
                None,
            ));
        }

        let attr = self.attribution(None, Some(p.reason));
        let msg = crate::forwardemail::writes::NewMessage {
            // `folder` is unused by the send path — forwardemail writes
            // to Sent automatically — but the NewMessage struct is shared
            // with create_draft, so we set it for completeness.
            folder: "Sent".to_string(),
            to: p.to,
            cc: p.cc,
            bcc: p.bcc,
            subject: p.subject,
            text: p.text,
            html: p.html,
        };
        let result = crate::write::mail::send_email(
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
        Ok(format!(
            "sent: {id} → {:?} — recorded in git with tool: send_email",
            msg.to
        ))
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
        description = "Create a calendar event. Two modes:\n\n• **Structured (recommended).** Pass `summary`, `start`, `end`, and optionally `description`/`location`/`attendees`. Times are RFC 3339 (`2026-01-15T12:00:00Z` or with offset) for timed events, or bare `YYYY-MM-DD` for all-day. The server builds a well-formed VEVENT for you — no iCal hand-crafting.\n\n• **Raw iCal (power-user).** Pass `ical` with a complete VCALENDAR/VEVENT block to control RRULE, VALARM, or properties the structured fields don't expose. Any structured fields are ignored in this mode.\n\nRequires readwrite on the target calendar (per-calendar scoped permissions are honored)."
    )]
    async fn create_event(
        &self,
        Parameters(p): Parameters<CreateEventParams>,
    ) -> Result<String, McpError> {
        // Per-calendar scoped check: the target calendar id is in the params.
        self.check_write_scoped(Scope::Calendar {
            calendar_id: Some(&p.calendar_id),
        })?;

        // Assemble the iCal payload. Prefer the raw override when present;
        // otherwise build one from the structured fields.
        let ical = if let Some(raw) = p.ical.as_deref() {
            raw.to_string()
        } else {
            let summary = p.summary.as_deref().ok_or_else(|| {
                McpError::invalid_params(
                    "create_event: provide `summary`+`start`+`end` (structured) or `ical` (raw)",
                    None,
                )
            })?;
            let start = p.start.as_deref().ok_or_else(|| {
                McpError::invalid_params("create_event: `start` is required in structured mode", None)
            })?;
            let end = p.end.as_deref().ok_or_else(|| {
                McpError::invalid_params("create_event: `end` is required in structured mode", None)
            })?;
            build_ical_event(
                summary,
                start,
                end,
                p.description.as_deref(),
                p.location.as_deref(),
                &p.attendees,
                p.event_id.as_deref(),
            )
            .map_err(|e| McpError::invalid_params(format!("create_event: {e}"), None))?
        };

        let attr = self.attribution(None, p.reason);
        let created = crate::write::calendar::create_event(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &p.calendar_id,
            &ical,
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
        // Dry-run only reads history and the live state — no mutation.
        // Gate on read so callers can preview what a restore *would* do
        // before deciding whether to grant write. The matching apply tool
        // still enforces write.
        self.check(Resource::Contacts)?;
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
        description = "Execute a restore plan returned by restore_contact_dry_run. Re-computes plan_token from the submitted plan and refuses to proceed if it doesn't match the supplied token — this prevents dry-running one plan and applying a different one.\n\n⚠️ **Pass the `plan` object through VERBATIM.** Do not reformat, pretty-print, re-serialize, reorder keys, drop fields you think are redundant, or 'clean it up'. Any modification — including whitespace changes on structured sub-values — invalidates the plan_token and the apply will be refused. Copy the exact JSON value returned by the dry-run."
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
        // Read-only preview; apply enforces write.
        self.check(Resource::Sieve)?;
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
        description = "Execute a sieve restore plan. Re-computes plan_token and refuses on mismatch.\n\n⚠️ **Pass the `plan` object through VERBATIM** — do not reformat, re-serialize, or 'clean up' the JSON returned by the dry-run. Any modification invalidates the plan_token."
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
        // Read-only preview scoped to the specific calendar id in the
        // plan params; apply enforces write.
        self.check_scoped(Scope::Calendar {
            calendar_id: Some(&p.calendar_id),
        })?;
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
        description = "Execute a calendar event restore plan.\n\n⚠️ **Pass the `plan` object through VERBATIM** — do not reformat, re-serialize, or 'clean up' the JSON returned by the dry-run. Any modification invalidates the plan_token."
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
        // Dry-run: read-only preview of the folder the caller is asking
        // about. Apply enforces write on both the historical folder and
        // any cross-folder destination.
        self.check_scoped(Scope::Email {
            folder: Some(&p.folder),
        })?;
        let (plan, token) = crate::restore::mail::plan_mail(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &p.folder,
            &p.id,
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
        description = "Execute a mail restore plan (flags, folder, or re-append).\n\n⚠️ **Pass the `plan` object through VERBATIM** — do not reformat, re-serialize, or 'clean up' the JSON returned by the dry-run. The plan may carry a `raw_bytes` array for the re-append case; that array binds the plan_token to the exact bytes being restored, so dropping or reordering it will invalidate the token."
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
        description = "Execute a bulk restore plan. Re-verifies the bulk plan_token AND each sub-plan's individual token before executing. Continues past individual sub-plan failures and returns a summary of what succeeded and what failed.\n\n⚠️ **Pass the `plan` object through VERBATIM** — every sub-plan inside also carries its own token, and any whitespace or ordering change anywhere in the tree invalidates one of them."
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

    // ── Introspection ────────────────────────────────────────────────

    #[tool(
        name = "get_permissions",
        description = "Returns the effective permission matrix for this MCP session as structured JSON. Use this to discover which resources you can read, write, or send before calling other tools. The response includes per-resource access levels and any scoped overrides (per-folder for email, per-calendar-id for calendar)."
    )]
    async fn get_permissions(
        &self,
        Parameters(_p): Parameters<EmptyParams>,
    ) -> Result<String, McpError> {
        let perms = &self.inner.permissions;

        // Build a structured view of the permission matrix. This is NOT
        // the raw config.toml — it's the resolved, effective permissions
        // as the MCP server sees them.
        let email = match &perms.email {
            crate::permission::EmailPermission::Flat(a) => {
                serde_json::json!({ "default": a, "folders": {} })
            }
            crate::permission::EmailPermission::Scoped(s) => {
                serde_json::json!({ "default": s.default, "folders": s.folders })
            }
        };

        let calendar = match &perms.calendar {
            crate::permission::CalendarPermission::Flat(a) => {
                serde_json::json!({ "default": a, "by_id": {} })
            }
            crate::permission::CalendarPermission::Scoped(s) => {
                serde_json::json!({ "default": s.default, "by_id": s.by_id })
            }
        };

        let result = serde_json::json!({
            "email": email,
            "email_send": perms.email_send,
            "calendar": calendar,
            "contacts": perms.contacts,
            "sieve": perms.sieve,
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    // ── History / audit ──────────────────────────────────────────────

    #[tool(
        name = "history",
        description = "Git log for a path in the pimsteward backup tree. Shows commits that touched the file or directory, newest first. Returns a JSON array of objects with `sha`, `author`, `date` (ISO-8601), and `subject`. Use this to see who changed what and when, including AI-attributed mutations — `author` distinguishes `pimsteward-pull` (reconciliation), caller names like `ai` (AI writes), and any human callers."
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

        // Parse the tab-separated output into structured commits so the
        // caller (often an LLM) doesn't have to re-parse strings.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let commits: Vec<serde_json::Value> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut parts = line.splitn(4, '\t');
                let sha = parts.next().unwrap_or("");
                let author = parts.next().unwrap_or("");
                let date = parts.next().unwrap_or("");
                let subject = parts.next().unwrap_or("");
                serde_json::json!({
                    "sha": sha,
                    "author": author,
                    "date": date,
                    "subject": subject,
                })
            })
            .collect();
        serde_json::to_string_pretty(&commits)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

/// Visibility rule for a tool — what the caller's permission state must
/// satisfy for the tool to appear in `list_tools` and dispatch in
/// `call_tool`. Checked at *any-scope* granularity (a per-folder override
/// granting write is enough to expose a resource-level write tool); the
/// per-call scoped checks inside each tool handler still enforce the fine
/// grain.
#[derive(Debug, Clone, Copy)]
enum ToolReq {
    /// Any scope of the resource grants read.
    Read(Resource),
    /// Any scope of the resource grants write.
    Write(Resource),
    /// The dedicated `email_send` permission is allowed.
    EmailSend,
    /// Always visible (audit/history tools).
    Always,
}

impl ToolReq {
    fn is_satisfied(self, perms: &Permissions) -> bool {
        match self {
            Self::Read(r) => perms.has_any_read(r),
            Self::Write(r) => perms.has_any_write(r),
            Self::EmailSend => perms.email_send.is_allowed(),
            Self::Always => true,
        }
    }
}

/// Static map of tool names to their visibility requirement. Any tool
/// whose resource is fully denied in the current permission matrix is
/// hidden from `list_tools` and rejected up-front in `call_tool` — the
/// model never even learns the tool exists, which matches the threat
/// model described in the README.
const TOOL_REQS: &[(&str, ToolReq)] = &[
    // Email reads
    ("search_email", ToolReq::Read(Resource::Email)),
    ("get_email", ToolReq::Read(Resource::Email)),
    ("list_folders", ToolReq::Read(Resource::Email)),
    // Email writes
    ("create_draft", ToolReq::Write(Resource::Email)),
    ("update_email_flags", ToolReq::Write(Resource::Email)),
    ("move_email", ToolReq::Write(Resource::Email)),
    ("delete_email", ToolReq::Write(Resource::Email)),
    // Email send (its own permission)
    ("send_email", ToolReq::EmailSend),
    // Calendar reads
    ("list_calendars", ToolReq::Read(Resource::Calendar)),
    ("list_events", ToolReq::Read(Resource::Calendar)),
    // Calendar writes
    ("create_event", ToolReq::Write(Resource::Calendar)),
    ("update_event", ToolReq::Write(Resource::Calendar)),
    ("delete_event", ToolReq::Write(Resource::Calendar)),
    // Contacts
    ("list_contacts", ToolReq::Read(Resource::Contacts)),
    ("create_contact", ToolReq::Write(Resource::Contacts)),
    ("update_contact", ToolReq::Write(Resource::Contacts)),
    ("delete_contact", ToolReq::Write(Resource::Contacts)),
    // Sieve
    ("list_sieve", ToolReq::Read(Resource::Sieve)),
    ("install_sieve_script", ToolReq::Write(Resource::Sieve)),
    ("update_sieve_script", ToolReq::Write(Resource::Sieve)),
    ("delete_sieve_script", ToolReq::Write(Resource::Sieve)),
    // Restore — dry-runs require READ, applies require WRITE. Bulk
    // restore touches three resources, so we expose it whenever any of
    // them is writable; the handler re-checks each resource per-op.
    ("restore_contact_dry_run", ToolReq::Read(Resource::Contacts)),
    ("restore_contact_apply", ToolReq::Write(Resource::Contacts)),
    ("restore_sieve_dry_run", ToolReq::Read(Resource::Sieve)),
    ("restore_sieve_apply", ToolReq::Write(Resource::Sieve)),
    ("restore_calendar_event_dry_run", ToolReq::Read(Resource::Calendar)),
    ("restore_calendar_event_apply", ToolReq::Write(Resource::Calendar)),
    ("restore_mail_dry_run", ToolReq::Read(Resource::Email)),
    ("restore_mail_apply", ToolReq::Write(Resource::Email)),
    ("restore_path_dry_run", ToolReq::Read(Resource::Contacts)),
    ("restore_path_apply", ToolReq::Write(Resource::Contacts)),
    // Always-available introspection + audit tools.
    ("get_permissions", ToolReq::Always),
    ("history", ToolReq::Always),
];

impl PimstewardServer {
    /// Return true if `tool_name` is exposed in the current permission
    /// configuration. Used by both `list_tools` (to filter) and
    /// `call_tool` (to refuse hidden tools with a proper error).
    fn tool_visible(&self, tool_name: &str) -> bool {
        TOOL_REQS
            .iter()
            .find(|(n, _)| *n == tool_name)
            .map(|(_, req)| req.is_satisfied(&self.inner.permissions))
            // Unknown tool name: fail closed. If the code ever grows a
            // tool without an entry in TOOL_REQS, it'll be hidden until
            // the map is updated — safer than silently granting access.
            .unwrap_or(false)
    }
}

impl ServerHandler for PimstewardServer {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        // Refuse up-front if the tool is hidden by the current permission
        // configuration. Use METHOD_NOT_FOUND so the caller sees the same
        // shape they'd see if the tool truly didn't exist — which, from
        // their point of view, it doesn't.
        if !self.tool_visible(&request.name) {
            return Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("tool not available: {}", request.name),
                None,
            ));
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        // Filter the static tool list against the current permission
        // configuration. Resources that are fully denied (e.g.
        // `email = "none"`) disappear entirely — the model doesn't see
        // their tool names, schemas, or descriptions.
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| self.tool_visible(&t.name))
            .collect();
        Ok(rmcp::model::ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if !self.tool_visible(name) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            format!(
                "pimsteward — permission-aware PIM mediator for forwardemail.net.\n\
                 Alias: {}\n\
                 Caller attribution: {}\n\n\
                 Read, write, and restore tools for email, calendar, contacts, \
                 and sieve scripts, gated by the configured permission matrix. \
                 Every mutation produces an attributed git commit in the backup repo.\n\n\
                 PERMISSIONS: call `get_permissions` to discover your effective \
                 access levels before using other tools. Tools whose resource is \
                 denied are hidden from this list entirely.\n\n\
                 UNDO: almost every mutation is reversible. Each resource has a \
                 `restore_*_dry_run` tool that computes what a rollback would do \
                 (returning a plan + plan_token) and a matching `restore_*_apply` \
                 tool that re-verifies the token before executing. Use these \
                 whenever the user asks you to undo a change. The one exception \
                 is `send_email` — once forwardemail has accepted the message \
                 for delivery, there is no rewind, only an audit trailer.\n\n\
                 AUDIT: the `history` tool is `git log` for any path under the \
                 backup tree, returning structured commits so you can see who \
                 changed what.",
                self.inner.alias, self.inner.caller
            ),
        )
    }
}

/// Build a minimal, RFC 5545-conformant VCALENDAR/VEVENT from structured
/// fields. Handles two common cases:
///
/// - All-day events: `start` and `end` are bare `YYYY-MM-DD` dates → emits
///   `DTSTART;VALUE=DATE:YYYYMMDD` / `DTEND;VALUE=DATE:YYYYMMDD` (DTEND
///   exclusive per RFC 5545).
/// - Timed events: `start` and `end` are RFC 3339 instants → emits
///   `DTSTART:YYYYMMDDTHHMMSSZ` in UTC. Offsets are normalized to Z.
///
/// Returns a Result with a human-readable error on malformed input so the
/// MCP layer can surface it back to the caller as an `invalid_params`.
fn build_ical_event(
    summary: &str,
    start: &str,
    end: &str,
    description: Option<&str>,
    location: Option<&str>,
    attendees: &[String],
    event_id: Option<&str>,
) -> Result<String, String> {
    use chrono::{DateTime, NaiveDate, Utc};

    // Escape per RFC 5545 §3.3.11: backslash-escape backslash, semicolon,
    // comma, and fold newlines to \n.
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace(';', "\\;")
            .replace(',', "\\,")
            .replace('\r', "")
            .replace('\n', "\\n")
    }

    let (dtstart, dtend) = if start.len() == 10 && start.as_bytes()[4] == b'-' {
        // Date-only all-day event.
        let s = NaiveDate::parse_from_str(start, "%Y-%m-%d")
            .map_err(|e| format!("start: invalid YYYY-MM-DD: {e}"))?;
        let e = NaiveDate::parse_from_str(end, "%Y-%m-%d")
            .map_err(|e| format!("end: invalid YYYY-MM-DD: {e}"))?;
        (
            format!("DTSTART;VALUE=DATE:{}", s.format("%Y%m%d")),
            format!("DTEND;VALUE=DATE:{}", e.format("%Y%m%d")),
        )
    } else {
        // Timed event: parse RFC 3339 and normalize to UTC.
        let s: DateTime<Utc> = DateTime::parse_from_rfc3339(start)
            .map_err(|e| format!("start: invalid RFC 3339: {e}"))?
            .with_timezone(&Utc);
        let e: DateTime<Utc> = DateTime::parse_from_rfc3339(end)
            .map_err(|e| format!("end: invalid RFC 3339: {e}"))?
            .with_timezone(&Utc);
        (
            format!("DTSTART:{}", s.format("%Y%m%dT%H%M%SZ")),
            format!("DTEND:{}", e.format("%Y%m%dT%H%M%SZ")),
        )
    };

    let uid = match event_id {
        Some(id) => id.to_string(),
        // RFC 5545 requires a globally unique UID. Use current timestamp +
        // a hash of summary+start for determinism within a request.
        None => {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(summary.as_bytes());
            h.update(start.as_bytes());
            h.update(Utc::now().timestamp_nanos_opt().unwrap_or(0).to_le_bytes());
            format!("{:x}@pimsteward", h.finalize())
        }
    };
    let dtstamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//pimsteward//create_event//EN\r\n");
    out.push_str("BEGIN:VEVENT\r\n");
    out.push_str(&format!("UID:{uid}\r\n"));
    out.push_str(&format!("DTSTAMP:{dtstamp}\r\n"));
    out.push_str(&format!("{dtstart}\r\n"));
    out.push_str(&format!("{dtend}\r\n"));
    out.push_str(&format!("SUMMARY:{}\r\n", esc(summary)));
    if let Some(d) = description {
        out.push_str(&format!("DESCRIPTION:{}\r\n", esc(d)));
    }
    if let Some(l) = location {
        out.push_str(&format!("LOCATION:{}\r\n", esc(l)));
    }
    for a in attendees {
        out.push_str(&format!("ATTENDEE:mailto:{}\r\n", esc(a)));
    }
    out.push_str("END:VEVENT\r\n");
    out.push_str("END:VCALENDAR\r\n");
    Ok(out)
}

/// Parse the header block of an RFC822 message. Returns (a map of the
/// interesting display headers, the full `Content-Type` value if present
/// — used downstream to find MIME boundaries).
///
/// Unfolds RFC 5322 continuation lines (leading whitespace on a line
/// means "continuation of the previous header") so `Content-Type` values
/// split across lines parse correctly.
fn parse_headers(text: &str) -> (serde_json::Map<String, serde_json::Value>, Option<String>) {
    let mut headers = serde_json::Map::new();
    let mut content_type: Option<String> = None;
    let mut current: Option<(String, String)> = None;

    let end = text
        .find("\r\n\r\n")
        .or_else(|| text.find("\n\n"))
        .unwrap_or(text.len());
    let header_block = &text[..end];

    let flush = |current: &mut Option<(String, String)>,
                 headers: &mut serde_json::Map<String, serde_json::Value>,
                 content_type: &mut Option<String>| {
        if let Some((k, v)) = current.take() {
            if k == "content-type" {
                *content_type = Some(v.clone());
            }
            if matches!(
                k.as_str(),
                "from"
                    | "to"
                    | "cc"
                    | "subject"
                    | "date"
                    | "message-id"
                    | "in-reply-to"
                    | "references"
                    | "content-type"
            ) {
                headers.insert(k, serde_json::Value::String(v));
            }
        }
    };

    for line in header_block.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation of the previous header.
            if let Some((_, ref mut v)) = current {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        flush(&mut current, &mut headers, &mut content_type);
        if let Some((key, val)) = line.split_once(':') {
            current = Some((key.trim().to_lowercase(), val.trim().to_string()));
        }
    }
    flush(&mut current, &mut headers, &mut content_type);
    (headers, content_type)
}

/// Pull the best text preview we can out of an RFC822 body. Handles
/// multipart/alternative by selecting the first text/plain part; if none
/// exists, falls back to stripping tags out of the first text/html part.
/// Single-part bodies are returned verbatim (after the header/body split).
///
/// This is a deliberately minimal MIME parser — it doesn't decode
/// quoted-printable or base64 transfer encodings, and it doesn't recurse
/// into nested multiparts. Callers who need full fidelity pass
/// `include_raw: true` and parse the .eml themselves.
fn extract_body_preview(text: &str, content_type: Option<&str>) -> String {
    let body_start = match text.find("\r\n\r\n").map(|i| i + 4).or_else(|| text.find("\n\n").map(|i| i + 2)) {
        Some(i) => i,
        None => return String::new(),
    };
    let body = &text[body_start..];

    // Only handle multipart/* here; everything else is treated as a
    // single-part text body.
    let ct = match content_type {
        Some(s) => s.to_ascii_lowercase(),
        None => return body.to_string(),
    };
    if !ct.starts_with("multipart/") {
        return body.to_string();
    }

    // Extract boundary="..." (or boundary=... with no quotes).
    let boundary = match ct.split(';').find_map(|kv| {
        let kv = kv.trim();
        kv.strip_prefix("boundary=").map(|b| b.trim_matches('"').to_string())
    }) {
        Some(b) if !b.is_empty() => b,
        _ => return body.to_string(),
    };
    let delim = format!("--{boundary}");

    // Walk the parts. For each one, read its own Content-Type and body.
    // Prefer the first text/plain; remember the first text/html as a
    // fallback.
    let mut plain: Option<String> = None;
    let mut html: Option<String> = None;
    for part in body.split(&delim).skip(1) {
        // Trim the leading CRLF that follows the boundary line, and stop
        // at the closing "--" marker.
        let part = part.trim_start_matches(['\r', '\n']);
        if part.starts_with("--") {
            break;
        }
        let (part_headers_end, offset) = match part
            .find("\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| part.find("\n\n").map(|i| (i, 2)))
        {
            Some(v) => v,
            None => continue,
        };
        let part_headers = &part[..part_headers_end];
        let part_body = part[part_headers_end + offset..]
            .trim_end_matches(['\r', '\n', '-']);
        let part_ct = part_headers
            .lines()
            .find_map(|l| {
                let lower = l.to_ascii_lowercase();
                lower
                    .strip_prefix("content-type:")
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_default();
        if part_ct.starts_with("text/plain") && plain.is_none() {
            plain = Some(part_body.to_string());
        } else if part_ct.starts_with("text/html") && html.is_none() {
            html = Some(part_body.to_string());
        }
        if plain.is_some() {
            break;
        }
    }
    if let Some(p) = plain {
        return p;
    }
    if let Some(h) = html {
        return strip_tags(&h);
    }
    body.to_string()
}

/// Crude HTML-tag stripper used as a last-resort fallback when a message
/// is html-only. Good enough for preview purposes; anything more serious
/// should use include_raw and a real HTML parser.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{
        Access, CalendarPermission, EmailPermission, Permissions, ScopedCalendar, ScopedEmail,
        SendPermission,
    };
    use std::collections::HashMap;

    // ── Tool visibility filter ──────────────────────────────────────
    //
    // The README guarantees that a denied resource's tools are hidden
    // entirely — not just gated at call time. These tests pin that
    // behaviour so a later refactor of the permission model can't
    // silently re-expose the tools.

    fn perms_with(email: Access, calendar: Access, contacts: Access, sieve: Access) -> Permissions {
        Permissions {
            email: EmailPermission::Flat(email),
            email_send: SendPermission::Denied,
            calendar: CalendarPermission::Flat(calendar),
            contacts,
            sieve,
        }
    }

    fn req_for(name: &str) -> ToolReq {
        TOOL_REQS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| panic!("tool {name} not in TOOL_REQS — every tool must be registered"))
    }

    #[test]
    fn every_known_tool_has_a_visibility_rule() {
        for name in [
            "search_email", "get_email", "list_folders",
            "create_draft", "update_email_flags", "move_email", "delete_email", "send_email",
            "list_calendars", "list_events",
            "create_event", "update_event", "delete_event",
            "list_contacts", "create_contact", "update_contact", "delete_contact",
            "list_sieve", "install_sieve_script", "update_sieve_script", "delete_sieve_script",
            "restore_contact_dry_run", "restore_contact_apply",
            "restore_sieve_dry_run", "restore_sieve_apply",
            "restore_calendar_event_dry_run", "restore_calendar_event_apply",
            "restore_mail_dry_run", "restore_mail_apply",
            "restore_path_dry_run", "restore_path_apply",
            "get_permissions",
            "history",
        ] {
            let _ = req_for(name);
        }
    }

    #[test]
    fn none_resource_hides_all_of_its_tools() {
        let p = perms_with(Access::None, Access::ReadWrite, Access::ReadWrite, Access::ReadWrite);
        // Email is none → every email tool must be hidden.
        for t in ["search_email", "get_email", "list_folders", "create_draft",
                  "update_email_flags", "move_email", "delete_email",
                  "restore_mail_dry_run", "restore_mail_apply"] {
            assert!(!req_for(t).is_satisfied(&p), "{t} should be hidden when email=none");
        }
        // Unrelated resources stay visible.
        assert!(req_for("list_calendars").is_satisfied(&p));
        assert!(req_for("list_contacts").is_satisfied(&p));
        assert!(req_for("list_sieve").is_satisfied(&p));
    }

    #[test]
    fn read_only_hides_write_tools_but_keeps_reads() {
        let p = perms_with(Access::Read, Access::Read, Access::Read, Access::Read);
        assert!(req_for("search_email").is_satisfied(&p));
        assert!(req_for("list_events").is_satisfied(&p));
        assert!(req_for("list_contacts").is_satisfied(&p));
        assert!(!req_for("create_draft").is_satisfied(&p));
        assert!(!req_for("create_event").is_satisfied(&p));
        assert!(!req_for("create_contact").is_satisfied(&p));
        // Dry-runs take read, so they should still be visible.
        assert!(req_for("restore_contact_dry_run").is_satisfied(&p));
        // Applies require write, so they should not.
        assert!(!req_for("restore_contact_apply").is_satisfied(&p));
    }

    #[test]
    fn any_scope_grant_exposes_the_tool() {
        // Default=none, but one folder has read_write → email write
        // tools should be exposed (the per-call scoped check still
        // enforces the actual folder rule).
        let mut folders = HashMap::new();
        folders.insert("Drafts".to_string(), Access::ReadWrite);
        let p = Permissions {
            email: EmailPermission::Scoped(ScopedEmail {
                default: Access::None,
                folders,
            }),
            ..Permissions::default()
        };
        assert!(req_for("create_draft").is_satisfied(&p));
        assert!(req_for("search_email").is_satisfied(&p));
    }

    #[test]
    fn send_email_requires_its_own_permission_not_read_write() {
        // read_write on email must NOT expose send_email — this is the
        // invariant the whole `email_send` split exists to protect.
        let p = Permissions {
            email: EmailPermission::Flat(Access::ReadWrite),
            ..Permissions::default()
        };
        assert!(!req_for("send_email").is_satisfied(&p));
        let p = Permissions {
            email_send: SendPermission::Allowed,
            ..Permissions::default()
        };
        assert!(req_for("send_email").is_satisfied(&p));
    }

    #[test]
    fn introspection_tools_are_always_visible() {
        let p = Permissions::default(); // everything denied
        assert!(req_for("get_permissions").is_satisfied(&p));
        assert!(req_for("history").is_satisfied(&p));
    }

    #[test]
    fn calendar_scoped_with_one_allowed_id_exposes_tools() {
        // default=none with a single per-id read_write override — the
        // calendar tools should appear and the per-tool scoped check
        // enforces the specific id on each call.
        let mut by_id = HashMap::new();
        by_id.insert("cal-work".to_string(), Access::ReadWrite);
        let p = Permissions {
            calendar: CalendarPermission::Scoped(ScopedCalendar {
                default: Access::None,
                by_id,
            }),
            ..Permissions::default()
        };
        assert!(req_for("list_events").is_satisfied(&p));
        assert!(req_for("create_event").is_satisfied(&p));
        assert!(req_for("delete_event").is_satisfied(&p));
    }

    // ── build_ical_event ────────────────────────────────────────────

    #[test]
    fn ical_builder_timed_event_uses_utc_format() {
        let ics = build_ical_event(
            "Lunch",
            "2026-01-15T12:00:00Z",
            "2026-01-15T13:00:00Z",
            None,
            None,
            &[],
            Some("abc@example"),
        )
        .unwrap();
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(ics.contains("UID:abc@example"));
        assert!(ics.contains("DTSTART:20260115T120000Z"));
        assert!(ics.contains("DTEND:20260115T130000Z"));
        assert!(ics.contains("SUMMARY:Lunch"));
        assert!(ics.contains("END:VEVENT"));
    }

    #[test]
    fn ical_builder_normalizes_offsets_to_utc() {
        // 08:00 -04:00 == 12:00 Z. The builder normalizes everything to
        // UTC so the stored DTSTART is unambiguous.
        let ics = build_ical_event(
            "Lunch",
            "2026-01-15T08:00:00-04:00",
            "2026-01-15T09:00:00-04:00",
            None, None, &[], Some("u@x"),
        ).unwrap();
        assert!(ics.contains("DTSTART:20260115T120000Z"));
        assert!(ics.contains("DTEND:20260115T130000Z"));
    }

    #[test]
    fn ical_builder_all_day_event() {
        let ics = build_ical_event(
            "Holiday",
            "2026-07-04",
            "2026-07-05",
            None, None, &[], Some("h@x"),
        ).unwrap();
        assert!(ics.contains("DTSTART;VALUE=DATE:20260704"));
        assert!(ics.contains("DTEND;VALUE=DATE:20260705"));
    }

    #[test]
    fn ical_builder_escapes_semicolons_and_commas_in_summary() {
        let ics = build_ical_event(
            "Meeting; with, Alice",
            "2026-01-15T12:00:00Z",
            "2026-01-15T13:00:00Z",
            Some("multi\nline"),
            None, &[], Some("m@x"),
        ).unwrap();
        assert!(ics.contains("SUMMARY:Meeting\\; with\\, Alice"));
        assert!(ics.contains("DESCRIPTION:multi\\nline"));
    }

    #[test]
    fn ical_builder_rejects_malformed_timestamps() {
        let err = build_ical_event(
            "x", "not-a-date", "2026-01-15T13:00:00Z",
            None, None, &[], Some("e@x"),
        );
        assert!(err.is_err());
    }

    #[test]
    fn ical_builder_emits_attendees() {
        let ics = build_ical_event(
            "Standup",
            "2026-01-15T09:00:00Z",
            "2026-01-15T09:15:00Z",
            None, None,
            &["alice@example.com".into(), "bob@example.com".into()],
            Some("s@x"),
        ).unwrap();
        assert!(ics.contains("ATTENDEE:mailto:alice@example.com"));
        assert!(ics.contains("ATTENDEE:mailto:bob@example.com"));
    }

    // ── MIME preview ───────────────────────────────────────────────

    #[test]
    fn extract_body_preview_single_part_plain_returns_body_verbatim() {
        let msg = "Subject: hi\r\n\r\nhello there";
        let out = extract_body_preview(msg, Some("text/plain"));
        assert_eq!(out, "hello there");
    }

    #[test]
    fn extract_body_preview_multipart_alternative_prefers_text_plain() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"abc\"\r\n\r\n",
            "--abc\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>html version</p>\r\n",
            "--abc\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\n",
            "plain version\r\n",
            "--abc--\r\n",
        );
        let out = extract_body_preview(msg, Some("multipart/alternative; boundary=\"abc\""));
        assert!(out.contains("plain version"), "got: {out:?}");
        assert!(!out.contains("<p>"), "should not contain html tags");
    }

    #[test]
    fn extract_body_preview_html_only_strips_tags() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"xyz\"\r\n\r\n",
            "--xyz\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<html><body><p>hello <b>world</b></p></body></html>\r\n",
            "--xyz--\r\n",
        );
        let out = extract_body_preview(msg, Some("multipart/alternative; boundary=\"xyz\""));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
        assert!(!out.contains("<b>"));
    }

    #[test]
    fn parse_headers_unfolds_continuation_lines() {
        let msg = "Subject: a very long\r\n subject line\r\nFrom: alice@x\r\n\r\nbody";
        let (hs, _) = parse_headers(msg);
        assert_eq!(hs.get("subject").unwrap().as_str().unwrap(), "a very long subject line");
        assert_eq!(hs.get("from").unwrap().as_str().unwrap(), "alice@x");
    }

    #[test]
    fn parse_headers_extracts_content_type_for_mime_selection() {
        let msg = "Content-Type: multipart/alternative; boundary=\"foo\"\r\n\r\n";
        let (_, ct) = parse_headers(msg);
        assert_eq!(ct.unwrap(), "multipart/alternative; boundary=\"foo\"");
    }
}
