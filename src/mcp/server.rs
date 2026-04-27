//! rmcp-based MCP server. The tool implementations live here; most are thin
//! wrappers around `crate::forwardemail::Client` methods with a permission
//! check on the front and a JSON-ready return value on the back.

use crate::forwardemail::Client;
use crate::index::{FlagFilter, FolderFilter, SearchQuery, SearchResult, Sort};
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

/// Check if a calendar event has STATUS:CANCELLED. Checks the struct field
/// first (populated by the REST API), then falls back to parsing the raw
/// iCalendar text (needed for CalDAV-sourced events where the field may
/// not be populated by upstream).
fn is_cancelled(ev: &crate::forwardemail::calendar::CalendarEvent) -> bool {
    // Struct field — populated by REST API responses
    if let Some(ref s) = ev.status {
        return s.eq_ignore_ascii_case("CANCELLED");
    }
    // Fallback: grep the raw iCal for STATUS:CANCELLED
    if let Some(ref ical) = ev.ical {
        return ical.lines().any(|line| {
            let l = line.trim();
            l.eq_ignore_ascii_case("STATUS:CANCELLED")
        });
    }
    false
}

/// Error text for a canonical id that isn't present in pimsteward's
/// backup tree yet.
///
/// This is a per-message DATA state — almost always a race between
/// pimsteward's `/notifications` SSE signal and the next mail puller
/// cycle writing the new message's `meta.json`. It is NOT an MCP server
/// health issue, and the wording deliberately avoids the words "tree"
/// and "server" (prior callers — notably the rocky@hld.ca agent — have
/// read "not found in backup tree" as "infrastructure is down" and
/// escalated to operator alerts). Stable text so callers can match on
/// it if they want to.
fn message_not_indexed_error(canonical_id: &str) -> String {
    format!(
        "message {canonical_id} not yet indexed by pimsteward \
         (try again in a few seconds or wait for the next \
         /notifications event — this is a per-message data state, \
         not an MCP server failure)"
    )
}

/// Inject a `canonical_id` field on every message in a `search_email`
/// response so callers can go straight to `get_email` without
/// reconstructing the id themselves.
///
/// Why this exists: forwardemail's `/v1/messages` endpoint returns a
/// REST `id` in its own namespace — `get_email` rejects that id with
/// "message X not yet indexed" because pimsteward's backup tree keys
/// files by `sha256(Message-ID)[:16]`. Every consumer that wanted to
/// walk "search → fetch" has had to re-derive the canonical id itself:
///
///   - assistant-email-watcher.py reimplements the hash in Python.
///   - spamguard used to bypass MCP entirely and scan `.eml` filenames
///     off the filesystem via a bind mount.
///   - Agents (rocky) couldn't figure it out at all and narrated
///     "MCP is down" when `get_email(search_result.id)` failed.
///
/// Enriching the response here is the one-line fix that closes the
/// gap for every client simultaneously. The canonical id is computed
/// the same way `pull::mail::derive_canonical_id` computes it during
/// IMAP/REST ingest, so round-tripping `search_email → get_email`
/// inside a single backup tree is stable.
///
/// Accepts both the `[{...}, ...]` list shape and the
/// `{"messages": [...]}` wrapper shape, because forwardemail has been
/// observed returning both over the life of the API.
#[allow(dead_code)] // Retained for existing unit tests; superseded by search_email's local-index path.
fn enrich_search_results_with_canonical_id(v: &mut serde_json::Value) {
    fn enrich_one(msg: &mut serde_json::Value) {
        let Some(obj) = msg.as_object_mut() else { return };
        // Already present (paranoid no-op, e.g. if forwardemail ever
        // starts returning this field themselves): trust ours over theirs
        // so behaviour stays deterministic.
        let mid = obj
            .get("header_message_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(mid) = mid {
            let cid = crate::pull::mail::hash_to_canonical(mid);
            obj.insert("canonical_id".into(), serde_json::Value::String(cid));
        }
    }

    if let Some(arr) = v.as_array_mut() {
        for msg in arr.iter_mut() {
            enrich_one(msg);
        }
        return;
    }
    if let Some(arr) = v.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for msg in arr.iter_mut() {
            enrich_one(msg);
        }
    }
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
    /// ManageSieve connection info for sieve script activation.
    /// Forwardemail's REST API treats is_active as read-only.
    managesieve: ManageSieveConfig,
    /// Local search index (SQLite + FTS5) backing `search_email`.  See
    /// `crate::index` and `docs/specs/2026-04-20-local-search-index-
    /// design.md` for layout and invariants.
    search_index: Arc<crate::index::SearchIndex>,
}

/// ManageSieve connection parameters, passed through from Config.
#[derive(Clone)]
pub struct ManageSieveConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for PimstewardServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PimstewardServer")
            .field("alias", &self.inner.alias)
            .finish_non_exhaustive()
    }
}

impl PimstewardServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        repo: Repo,
        permissions: Permissions,
        alias: String,
        caller: String,
        mail_source: Arc<dyn MailSource>,
        mail_writer: Arc<dyn MailWriter>,
        managesieve: ManageSieveConfig,
        search_index: Arc<crate::index::SearchIndex>,
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
                managesieve,
                search_index,
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
        Err(McpError::invalid_params(message_not_indexed_error(canonical_id), None))
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
    /// FTS5 match expression over subject, body, and sender/recipient
    /// names.  Supports booleans (`apple NOT receipt`), phrases
    /// (`"your order"`), and column prefixes (`subject:invoice`).
    #[serde(default)]
    pub query: Option<String>,
    /// Substring match on the From address OR display name, case-insensitive.
    #[serde(default)]
    pub from: Option<String>,
    /// Substring match on any To or Cc address, case-insensitive.
    #[serde(default)]
    pub to: Option<String>,
    /// Substring match on Subject, case-insensitive.
    #[serde(default)]
    pub subject: Option<String>,
    /// Folder filter.  `"INBOX"` for exact match, `"Archive/*"` for
    /// the folder and all descendants, `"*"` (or omitted) to search
    /// every folder.
    #[serde(default)]
    pub folder: Option<String>,
    /// Inclusive lower bound on message date (RFC3339, e.g.
    /// `2026-04-20T00:00:00Z`).
    #[serde(default)]
    pub since: Option<String>,
    /// Exclusive upper bound on message date (RFC3339).
    #[serde(default)]
    pub before: Option<String>,
    /// Flag filters.  Supply any combination of `any_of`, `all_of`,
    /// `none_of`.  Flag values are IMAP-style: `\\Seen`, `\\Flagged`,
    /// `\\Answered`, `\\Draft`, etc.
    #[serde(default)]
    pub flags: Option<FlagFilterParam>,
    /// Shortcut for `flags: { none_of: ["\\Seen"] }`.
    #[serde(default)]
    pub unread: Option<bool>,
    /// `true` to return only messages with attachments, `false` for
    /// only those without.
    #[serde(default)]
    pub has_attachments: Option<bool>,
    /// 0-indexed page offset.  Default 0.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Page size.  Default 25, max 200.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Result ordering: `"date_desc"` (default), `"date_asc"`, or
    /// `"relevance"` (requires `query`).
    #[serde(default)]
    pub sort: Option<String>,
    /// When true, skip fetching hits and return only `total_matches`.
    #[serde(default)]
    pub count_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FlagFilterParam {
    /// Match messages that carry at least one of these flags.
    #[serde(default)]
    pub any_of: Option<Vec<String>>,
    /// Match messages that carry all of these flags.
    #[serde(default)]
    pub all_of: Option<Vec<String>>,
    /// Exclude messages that carry any of these flags.
    #[serde(default)]
    pub none_of: Option<Vec<String>>,
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
    /// Include cancelled events in the result. Defaults to false —
    /// events with STATUS:CANCELLED are excluded by default.
    #[serde(default)]
    pub include_cancelled: Option<bool>,
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
pub struct ActivateSieveParams {
    /// Name of the script to activate. ManageSieve allows exactly one
    /// active script at a time — activating a script deactivates any
    /// previously active one. Pass an empty string to deactivate all.
    pub name: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetSieveParams {
    pub id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddSieveRuleParams {
    /// Sieve text for the new rule. May include its own
    /// `require [...]` declarations — they will be merged into the
    /// active script's existing requires. The rule body itself is
    /// appended after the existing rules.
    pub rule: String,
    /// Optional human-readable comment placed above the new rule. One
    /// `# ` prefix is added per line.
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetEmailParams {
    /// Canonical message id (the filename stem from the backup tree,
    /// e.g. as returned in search_email results or history).
    pub id: String,
    /// If true, include the raw RFC822 .eml bytes in the response
    /// (base64-encoded). Default false — the response always carries
    /// parsed headers, metadata, and the full extracted plain-text
    /// `body`, which is usually enough for an AI agent.
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
pub struct CreateReplyDraftParams {
    /// Recipient email addresses (To:). For reply-all, include all
    /// original recipients except yourself.
    pub to: Vec<String>,
    /// CC recipients. Optional — include original CC recipients for reply-all.
    #[serde(default)]
    pub cc: Vec<String>,
    /// Email subject line. Usually "Re: <original subject>".
    pub subject: String,
    /// Message-ID of the email being replied to. Found in the original
    /// email's Message-ID header. Required for proper threading.
    pub in_reply_to: String,
    /// References chain from the original email's References header,
    /// as a list of Message-IDs. Pass the original References array
    /// verbatim — the tool appends in_reply_to automatically.
    #[serde(default)]
    pub references: Vec<String>,
    /// Plain-text reply body.
    #[serde(default)]
    pub text: Option<String>,
    /// Free-text reason for the audit trail.
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
    /// RFC822 Message-ID of the message being replied to. Sets the
    /// In-Reply-To header so the reply threads correctly in mail clients.
    /// Get this from the original email's headers via get_email.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// References chain for threading. Should contain the Message-IDs from
    /// the original email's References header plus its Message-ID. Combined
    /// with in_reply_to to form proper threading.
    #[serde(default)]
    pub references: Vec<String>,
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
        description = "Search mail in the local SQLite + FTS5 index.  Cross-folder by default (folder=\"*\").  Supports full-text query, substring from/to/subject, folder globs, date ranges, flag filters, unread shortcut, has_attachments, pagination, and sort (date_desc | date_asc | relevance).  Returns {total_matches, returned, offset, hits[]}.  Each hit carries a 200-char preview — use get_email for the full plain-text body, or get_email with include_raw=true for the raw RFC822."
    )]
    async fn search_email(
        &self,
        Parameters(p): Parameters<SearchEmailParams>,
    ) -> Result<String, McpError> {
        // If folder is specified exactly (not a glob), honor per-folder
        // permission.  Otherwise fall back to the email resource-level
        // default — glob queries span folders so per-folder scoping
        // doesn't apply.
        let perm_folder = p.folder.as_deref().filter(|f| !f.contains('*'));
        self.check_scoped(Scope::Email {
            folder: perm_folder,
        })?;

        let query = build_search_query(&p)?;
        let result = self
            .inner
            .search_index
            .search(&query)
            .map_err(|e| McpError::internal_error(format!("search_index: {e}"), None))?;

        let body = search_result_to_json(&result);
        serde_json::to_string_pretty(&body)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "get_email",
        description = "Get a single email message by canonical id. Returns parsed headers, metadata (flags, folder, modseq), and the extracted plain-text body in full (no truncation). Set include_raw=true to also receive the raw RFC822 bytes (base64). Refuses messages larger than 25 MiB on disk; use the .eml file path on the daemon host for those. The canonical id is the filename stem from the backup tree (16 hex chars)."
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

        // Refuse to read absurdly large .eml files. forwardemail caps
        // inbound mail well under 25 MiB, so this only fires on
        // corrupted-on-disk cases — guards against an OOM if the file
        // somehow grew, without ever firing on real mail.
        const MAX_EML_BYTES: u64 = 25 * 1024 * 1024;
        if let Ok(meta) = std::fs::metadata(&eml_path) {
            if meta.len() > MAX_EML_BYTES {
                obj.insert(
                    "error".into(),
                    serde_json::Value::String(format!(
                        "eml file is {} bytes, exceeds 25 MiB cap; read {} directly on the daemon host",
                        meta.len(),
                        eml_path.display()
                    )),
                );
                return serde_json::to_string_pretty(&result)
                    .map_err(|e| McpError::internal_error(e.to_string(), None));
            }
        }

        // Parse key headers and extract the body from the .eml.
        if let Ok(raw) = std::fs::read(&eml_path) {
            if let Ok(text) = std::str::from_utf8(&raw) {
                let (headers_map, content_type) = parse_headers(text);
                obj.insert("headers".into(), serde_json::Value::Object(headers_map));

                // Extract the best plain-text view we can from the MIME
                // tree. For multipart/alternative (HTML-first layout) we
                // pick the first text/plain part; if absent, fall back
                // to stripping tags from the first text/html part. For
                // single-part bodies we return the body verbatim. Full
                // MIME fidelity (nested multipart/related, attachments)
                // is available via include_raw.
                let body_text = extract_body_text(text, content_type.as_deref());
                obj.insert("body".into(), serde_json::Value::String(body_text));

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
        description = "List calendar events. Pass `calendar_id` to restrict to one calendar (checked against scoped per-calendar permissions). With no `calendar_id`, events are returned for every calendar the caller has read access to — events in calendars you can't read are filtered out silently rather than failing the call. Cancelled events (STATUS:CANCELLED) are excluded by default; pass `include_cancelled: true` to include them. Returns event JSON including the raw iCalendar text."
    )]
    async fn list_events(
        &self,
        Parameters(p): Parameters<ListEventsParams>,
    ) -> Result<String, McpError> {
        let include_cancelled = p.include_cancelled.unwrap_or(false);

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
            let filtered: Vec<_> = if include_cancelled {
                events
            } else {
                events.into_iter().filter(|ev| !is_cancelled(ev)).collect()
            };
            return serde_json::to_string_pretty(&filtered)
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
                    && (include_cancelled || !is_cancelled(ev))
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
        description = "List server-side sieve filter scripts for the alias with their activation state and validation status. Note: forwardemail allows exactly ONE active script at a time. The is_active field is sourced from ManageSieve (the REST API reports it incorrectly)."
    )]
    async fn list_sieve(&self, _p: Parameters<EmptyParams>) -> Result<String, McpError> {
        self.check(Resource::Sieve)?;
        let mut scripts = self
            .inner
            .client
            .list_sieve_scripts()
            .await
            .map_err(|e| self.api_error(e))?;

        // Forwardemail's REST API reports is_active incorrectly (always
        // false). Get the real active state from ManageSieve LISTSCRIPTS.
        let ms = &self.inner.managesieve;
        match crate::forwardemail::managesieve::get_active_script(
            &ms.host, ms.port, &ms.user, &ms.password,
        )
        .await
        {
            Ok(active_name) => {
                for s in &mut scripts {
                    s.is_active = active_name.as_deref() == Some(&s.name);
                }
            }
            Err(e) => {
                // Non-fatal: return REST data with a warning rather than
                // failing the entire list.
                tracing::warn!(error = %e, "ManageSieve LISTSCRIPTS failed, is_active may be inaccurate");
            }
        }

        serde_json::to_string_pretty(&scripts)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "get_sieve_script",
        description = "Fetch a single sieve script by id, including its full content (the list endpoint omits content). Use this to read the active script before composing a new version."
    )]
    async fn get_sieve_script(
        &self,
        Parameters(p): Parameters<GetSieveParams>,
    ) -> Result<String, McpError> {
        self.check(Resource::Sieve)?;
        let script = self
            .inner
            .client
            .get_sieve_script(&p.id)
            .await
            .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&script)
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
        description = "Create a brand-new named sieve script. Forwardemail allows exactly ONE active script at a time, so installing a new script and activating it deactivates the previous one and silently disables every rule it contained. Do NOT use this tool to add a rule — use `add_sieve_rule` instead, which appends to the currently active script. Reach for `install_sieve_script` only when you genuinely need a separate, named script (e.g. to stage an alternate ruleset for later activation). Forwardemail parses the script server-side and rejects invalid syntax."
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
        name = "add_sieve_rule",
        description = "Append a rule to the currently active sieve script. This is the right tool for the common request 'add a filter rule' — it fetches the active script, merges any `require [...]` capabilities the new rule needs, appends the rule body, and updates the script in place. Errors with HTTP 409 if no script is currently active (call `install_sieve_script` + `activate_sieve_script` first to bootstrap). The `rule` field may include its own require declarations; they are merged automatically. Optional `comment` is rendered as `# <line>` per line above the new rule."
    )]
    async fn add_sieve_rule(
        &self,
        Parameters(p): Parameters<AddSieveRuleParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let attr = self.attribution(None, p.reason);
        let updated = crate::write::sieve::add_sieve_rule(
            &self.inner.client,
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &self.inner.managesieve,
            &p.rule,
            p.comment.as_deref(),
        )
        .await
        .map_err(|e| self.api_error(e))?;
        serde_json::to_string_pretty(&updated)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(
        name = "update_sieve_script",
        description = "Replace an existing sieve script's full content (low-level; for adding a single rule, prefer `add_sieve_rule`). Note: is_active cannot be changed via the forwardemail REST API — scripts must be activated through `activate_sieve_script` (ManageSieve)."
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
        name = "activate_sieve_script",
        description = "Activate a sieve script by name via ManageSieve. Forwardemail allows exactly ONE active script at a time — activating a script deactivates any previously active one. To have multiple filter rules active simultaneously, combine them into a single script. Pass an empty name to deactivate all scripts."
    )]
    async fn activate_sieve_script(
        &self,
        Parameters(p): Parameters<ActivateSieveParams>,
    ) -> Result<String, McpError> {
        self.check_write(Resource::Sieve)?;
        let ms = &self.inner.managesieve;
        crate::forwardemail::managesieve::activate_script(
            &ms.host, ms.port, &ms.user, &ms.password, &p.name,
        )
        .await
        .map_err(|e| self.api_error(e))?;

        // Audit the activation in git.
        let attr = self.attribution(None, p.reason);
        let msg = format!(
            "sieve: activate \"{}\"\n\nCaller: {}\nEmail: {}",
            p.name, attr.caller, attr.caller_email
        );
        let _ = self.inner.repo.commit_all(
            &attr.caller,
            &attr.caller_email,
            &msg,
        );

        if p.name.is_empty() {
            Ok("all sieve scripts deactivated".to_string())
        } else {
            Ok(format!("activated sieve script \"{}\"", p.name))
        }
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
            in_reply_to: None,
            references: vec![],
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
        name = "create_reply_draft",
        description = "Create a threaded reply draft in the Drafts folder. Like create_draft but with proper In-Reply-To and References headers so the draft threads correctly in any email client. Use this when drafting a reply to an existing email — provide the original message's Message-ID and References chain. Reply-all by default: pass all original recipients in to/cc."
    )]
    async fn create_reply_draft(
        &self,
        Parameters(p): Parameters<CreateReplyDraftParams>,
    ) -> Result<String, McpError> {
        let folder = "Drafts";
        self.check_write_scoped(Scope::Email {
            folder: Some(folder),
        })?;

        let from = self.inner.client.alias_user();
        let attr = self.attribution(None, p.reason.clone());

        // Build RFC822 message with threading headers
        let mut headers = format!(
            "From: {from}\r\nTo: {to}\r\n",
            from = from,
            to = p.to.join(", "),
        );
        if !p.cc.is_empty() {
            headers.push_str(&format!("Cc: {}\r\n", p.cc.join(", ")));
        }
        headers.push_str(&format!("Subject: {}\r\n", p.subject));
        headers.push_str(&format!("In-Reply-To: {}\r\n", p.in_reply_to));

        // References = original References + original Message-ID
        let mut refs = p.references.clone();
        if !refs.contains(&p.in_reply_to) {
            refs.push(p.in_reply_to.clone());
        }
        headers.push_str(&format!("References: {}\r\n", refs.join(" ")));
        headers.push_str("X-Rocky-Draft: true\r\n");
        headers.push_str("MIME-Version: 1.0\r\n");
        headers.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        headers.push_str("Content-Transfer-Encoding: 8bit\r\n");
        headers.push_str("\r\n");

        let body = p.text.as_deref().unwrap_or("");
        let raw = format!("{headers}{body}");

        let result = self
            .inner
            .client
            .append_raw_message(folder, raw.as_bytes())
            .await
            .map_err(|e| self.api_error(e))?;

        // Audit + refresh backup tree (same pattern as create_draft)
        let msg_id = result
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let audit = crate::write::audit::WriteAudit {
            attribution: &attr,
            tool: "create_reply_draft",
            resource: "mail",
            resource_id: msg_id.to_string(),
            args: serde_json::json!({
                "folder": folder,
                "to": &p.to,
                "subject": &p.subject,
                "in_reply_to": &p.in_reply_to,
            }),
            summary: format!("mail: reply draft in {} → {} (replying to {})",
                folder, p.subject, p.in_reply_to),
        };
        let _ = crate::write::mail::refresh(
            self.inner.mail_source.as_ref(),
            &self.inner.repo,
            &self.inner.alias,
            &attr,
            &audit,
            &[&folder],
        )
        .await;

        Ok(format!("reply draft created: {msg_id} in {folder} (threaded via In-Reply-To: {})", p.in_reply_to))
    }

    #[tool(
        name = "send_email",
        description = "Send an email over SMTP via forwardemail's outgoing bridge. IRREVERSIBLE: once this returns success, the message has been accepted for delivery to third parties and there is no 'undo'. A copy is saved to the Sent folder automatically and captured into git on the next pull. Every send is recorded in the git audit log with tool=send_email plus recipients, subject, and body sha256 — `git log --grep='tool: send_email'` enumerates them.\n\nRequires the separate `email_send` permission (default: denied). Granting `email = \"read_write\"` does NOT grant send — you must set `email_send = \"allowed\"` in [permissions] explicitly. If you only want the assistant to prepare outgoing mail for human review, use `create_draft` instead; drafts are safely reversible.\n\nFor threaded replies: pass `in_reply_to` (the original Message-ID from get_email headers) and `references` (the original References chain) to set proper threading headers. The reply will appear in the same thread in mail clients."
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
            in_reply_to: p.in_reply_to,
            references: p.references,
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
    ("create_reply_draft", ToolReq::Write(Resource::Email)),
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
    ("activate_sieve_script", ToolReq::Write(Resource::Sieve)),
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
            // Authentication-Results / ARC-Authentication-Results legitimately
            // appear multiple times on a single message — one per handling
            // hop and one per ARC seal. If we dedupe down to the last value
            // the watcher's sender gate becomes a roll of the dice (pass/fail
            // depending on which hop landed last), so accumulate every
            // occurrence into a JSON array. Callers that only care about
            // one get to pick; the gate grep matches across all of them.
            if matches!(
                k.as_str(),
                "authentication-results" | "arc-authentication-results"
            ) {
                let entry = headers
                    .entry(k)
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                if let serde_json::Value::Array(arr) = entry {
                    arr.push(serde_json::Value::String(v));
                }
            } else if matches!(
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

/// Pull the best plain-text view out of an RFC822 body. For
/// multipart/alternative (the common HTML-first layout) we pick the
/// first text/plain part; if absent, fall back to stripping tags from
/// the first text/html part. Single-part bodies are decoded using the
/// top-level Content-Transfer-Encoding. No truncation — caller gets
/// the full extracted text.
///
/// Per-part Content-Transfer-Encoding is honored: `quoted-printable`
/// and `base64` are decoded before the body is returned, so LLM callers
/// see real text (e.g. `€` instead of `=E2=82=AC`) and `<br>` turns
/// into newlines. Charsets other than UTF-8 are treated as UTF-8 lossy
/// — good enough for LLM consumption. Callers who need full fidelity
/// (nested multipart/related, attachments, exact byte preservation)
/// pass `include_raw: true` and parse the .eml themselves.
fn extract_body_text(text: &str, content_type: Option<&str>) -> String {
    let body_start = match text.find("\r\n\r\n").map(|i| i + 4).or_else(|| text.find("\n\n").map(|i| i + 2)) {
        Some(i) => i,
        None => return String::new(),
    };
    let header_block = &text[..body_start];
    let body = &text[body_start..];

    // Top-level Content-Transfer-Encoding applies to single-part bodies.
    let top_cte = find_header_value(header_block, "content-transfer-encoding");

    let ct = match content_type {
        Some(s) => s.to_ascii_lowercase(),
        None => return decode_transfer(body, top_cte.as_deref()),
    };
    if !ct.starts_with("multipart/") {
        let decoded = decode_transfer(body, top_cte.as_deref());
        if ct.starts_with("text/html") {
            return strip_tags(&decoded);
        }
        return decoded;
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

    // Walk the parts. For each one, read its own Content-Type and
    // Content-Transfer-Encoding and decode the body. Prefer the first
    // text/plain; remember the first text/html as a fallback.
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
        let part_ct = find_header_value(part_headers, "content-type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let part_cte = find_header_value(part_headers, "content-transfer-encoding");
        if part_ct.starts_with("text/plain") && plain.is_none() {
            plain = Some(decode_transfer(part_body, part_cte.as_deref()));
        } else if part_ct.starts_with("text/html") && html.is_none() {
            html = Some(decode_transfer(part_body, part_cte.as_deref()));
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

/// Look up a header's value (case-insensitive) in a raw header block,
/// unfolding RFC 5322 continuation lines.
fn find_header_value(headers: &str, name: &str) -> Option<String> {
    let name_lc = name.to_ascii_lowercase();
    let mut found: Option<String> = None;
    for line in headers.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(v) = found.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if found.is_some() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().to_ascii_lowercase() == name_lc {
                found = Some(v.trim().to_string());
            }
        }
    }
    found
}

/// Apply Content-Transfer-Encoding to a part body: decode
/// `quoted-printable` or `base64`, leaving `7bit`/`8bit`/`binary`/unset
/// untouched. Output is always UTF-8 (invalid bytes replaced).
fn decode_transfer(body: &str, cte: Option<&str>) -> String {
    let cte_lc = cte
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match cte_lc.as_str() {
        "quoted-printable" => decode_quoted_printable(body),
        "base64" => {
            use base64::Engine;
            // base64 bodies often have internal CRLFs — strip all whitespace.
            let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
            match base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => body.to_string(),
            }
        }
        _ => body.to_string(),
    }
}

/// Decode RFC 2045 quoted-printable: `=HH` hex escapes and soft line
/// breaks (`=\r\n` / `=\n`). Malformed escapes pass through literally
/// rather than raising errors — preview quality over strictness.
fn decode_quoted_printable(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'=' && i + 1 < bytes.len() {
            let n1 = bytes[i + 1];
            // Soft line break: =\n or =\r\n
            if n1 == b'\n' {
                i += 2;
                continue;
            }
            if n1 == b'\r' && i + 2 < bytes.len() && bytes[i + 2] == b'\n' {
                i += 3;
                continue;
            }
            if i + 2 < bytes.len() {
                if let (Some(hi), Some(lo)) = (hex_val(n1), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

/// Render HTML as plaintext. Drops `<script>`/`<style>` blocks
/// wholesale, turns block-level closers and `<br>` into newlines,
/// decodes common HTML entities, and collapses excess whitespace.
/// Good enough that an LLM can extract order details, prices, and
/// structure from a marketing email without re-parsing the HTML.
fn strip_tags(html: &str) -> String {
    let html = remove_block(html, "script");
    let html = remove_block(&html, "style");

    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        // Collect the tag contents up to '>'. Unterminated tags get
        // dropped together with the remainder of the input — mirrors
        // what a browser would do with broken markup.
        let mut tag = String::new();
        let mut closed = false;
        for tc in chars.by_ref() {
            if tc == '>' {
                closed = true;
                break;
            }
            tag.push(tc);
        }
        if !closed {
            break;
        }
        // Bare tag name (without a leading '/') for block-level check.
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '/')
            .collect::<String>()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "br" | "p"
                | "div"
                | "tr"
                | "li"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "hr"
        ) {
            out.push('\n');
        }
    }

    let decoded = decode_entities(&out);
    collapse_whitespace(&decoded)
}

/// Strip `<tag>…</tag>` blocks in their entirety (tag name is
/// ASCII-case-insensitive). Used to drop `<script>` and `<style>`
/// whose contents are useless noise in a text preview.
fn remove_block(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(&open) {
        let start = cursor + rel;
        out.push_str(&s[cursor..start]);
        // After the opening tag name, find the end of the opening tag
        // ('>'), then the matching close tag.
        if let Some(end_of_open) = lower[start..].find('>').map(|n| start + n + 1) {
            if let Some(close_rel) = lower[end_of_open..].find(&close) {
                cursor = end_of_open + close_rel + close.len();
                continue;
            }
        }
        return out;
    }
    out.push_str(&s[cursor..]);
    out
}

/// Decode common HTML entities: named (`&amp;` `&nbsp;` `&mdash;` …)
/// and numeric (`&#233;` / `&#xE9;`). Unrecognised entities pass
/// through as literal `&name;` so nothing is silently lost.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut ent = String::new();
        let mut found_semi = false;
        for _ in 0..10 {
            match chars.peek().copied() {
                Some(';') => {
                    chars.next();
                    found_semi = true;
                    break;
                }
                Some(nc) if nc.is_ascii_alphanumeric() || nc == '#' => {
                    ent.push(nc);
                    chars.next();
                }
                _ => break,
            }
        }
        if !found_semi {
            out.push('&');
            out.push_str(&ent);
            continue;
        }
        let named = match ent.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "copy" => Some('©'),
            "reg" => Some('®'),
            "trade" => Some('™'),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "lsquo" | "rsquo" | "sbquo" => Some('\''),
            "ldquo" | "rdquo" | "bdquo" => Some('"'),
            _ => None,
        };
        if let Some(ch) = named {
            out.push(ch);
            continue;
        }
        if let Some(rest) = ent.strip_prefix('#') {
            let n = if let Some(hex) = rest.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                rest.parse::<u32>().ok()
            };
            if let Some(cp) = n.and_then(char::from_u32) {
                out.push(cp);
                continue;
            }
        }
        out.push('&');
        out.push_str(&ent);
        out.push(';');
    }
    out
}

/// Collapse runs of horizontal whitespace to a single space and runs
/// of blank lines to at most one. Preserves single newlines so the
/// block-level structure surfaced by `strip_tags` survives.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_streak = 0usize;
    for line in s.lines() {
        let mut trimmed = String::with_capacity(line.len());
        let mut prev_space = false;
        for c in line.chars() {
            if c == ' ' || c == '\t' {
                if !prev_space && !trimmed.is_empty() {
                    trimmed.push(' ');
                }
                prev_space = true;
            } else {
                trimmed.push(c);
                prev_space = false;
            }
        }
        let trimmed = trimmed.trim_end();
        if trimmed.is_empty() {
            blank_streak += 1;
            if blank_streak == 1 {
                out.push('\n');
            }
        } else {
            blank_streak = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Minimal URL component encoder for query string values. We intentionally
/// avoid pulling in a full urlencoding crate for three call sites.
#[allow(dead_code)]
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

// ── search_email helpers ─────────────────────────────────────────────────

fn build_search_query(p: &SearchEmailParams) -> Result<SearchQuery, McpError> {
    let sort = match p.sort.as_deref() {
        None => None,
        Some("date_desc") => Some(Sort::DateDesc),
        Some("date_asc") => Some(Sort::DateAsc),
        Some("relevance") => Some(Sort::Relevance),
        Some(other) => {
            return Err(McpError::invalid_params(
                format!("unknown sort value: {other:?} (expected date_desc | date_asc | relevance)"),
                None,
            ));
        }
    };

    let since_unix = match p.since.as_deref() {
        Some(s) => Some(parse_rfc3339(s, "since")?),
        None => None,
    };
    let before_unix = match p.before.as_deref() {
        Some(s) => Some(parse_rfc3339(s, "before")?),
        None => None,
    };

    let folder = p.folder.as_deref().map(FolderFilter::parse);

    let flags = p.flags.as_ref().map(|f| FlagFilter {
        any_of: f.any_of.clone(),
        all_of: f.all_of.clone(),
        none_of: f.none_of.clone(),
    });

    Ok(SearchQuery {
        query: p.query.clone(),
        from: p.from.clone(),
        to: p.to.clone(),
        subject: p.subject.clone(),
        folder,
        since_unix,
        before_unix,
        flags,
        unread: p.unread,
        has_attachments: p.has_attachments,
        offset: p.offset,
        limit: p.limit,
        sort,
        count_only: p.count_only,
    })
}

fn parse_rfc3339(s: &str, field: &str) -> Result<i64, McpError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .map_err(|e| {
            McpError::invalid_params(
                format!("{field} is not valid RFC3339: {e}"),
                None,
            )
        })
}

fn search_result_to_json(r: &SearchResult) -> serde_json::Value {
    let hits: Vec<serde_json::Value> = r
        .hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "canonical_id": h.canonical_id,
                "folder": h.folder,
                "date": h.date_unix.and_then(|t| {
                    chrono::DateTime::from_timestamp(t, 0).map(|d| d.to_rfc3339())
                }),
                "message_id": h.message_id,
                "from": {
                    "address": h.from.address,
                    "name": h.from.name,
                },
                "to": h.to.iter().map(|a| {
                    serde_json::json!({"address": a.address, "name": a.name})
                }).collect::<Vec<_>>(),
                "cc": h.cc.iter().map(|a| {
                    serde_json::json!({"address": a.address, "name": a.name})
                }).collect::<Vec<_>>(),
                "subject": h.subject,
                "flags": h.flags,
                "size": h.size,
                "has_attachments": h.has_attachments,
                "preview": h.preview,
            })
        })
        .collect();
    serde_json::json!({
        "total_matches": r.total_matches,
        "returned": r.returned,
        "offset": r.offset,
        "hits": hits,
    })
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
            "create_draft", "create_reply_draft", "update_email_flags", "move_email", "delete_email", "send_email",
            "list_calendars", "list_events",
            "create_event", "update_event", "delete_event",
            "list_contacts", "create_contact", "update_contact", "delete_contact",
            "list_sieve", "install_sieve_script", "update_sieve_script", "delete_sieve_script", "activate_sieve_script",
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
                  "create_reply_draft", "update_email_flags", "move_email",
                  "delete_email", "restore_mail_dry_run", "restore_mail_apply"] {
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

    // ── Error wording ───────────────────────────────────────────────
    //
    // The rocky@hld.ca hermes agent used to read the old
    // "message X not found in backup tree" error as "pimsteward
    // infrastructure is down" and escalate with Telegram alerts. The
    // helper below is the authoritative source of that error string,
    // and these tests pin the wording so nobody regresses it without
    // also updating the callers that depend on the semantics.
    #[test]
    fn message_not_indexed_error_is_scoped_to_the_id_not_the_server() {
        let msg = message_not_indexed_error("c8e7c5ed006edf64");
        assert!(
            msg.contains("c8e7c5ed006edf64"),
            "must name the specific message so it reads as data, not infra: {msg}"
        );
        assert!(
            msg.contains("not yet indexed"),
            "should describe the state as transient/indexing: {msg}"
        );
        assert!(
            msg.contains("not an MCP server failure"),
            "must explicitly distinguish from MCP-down so agents don't overreact: {msg}"
        );
    }

    // ── search_email canonical_id enrichment ────────────────────────
    //
    // Every historical workaround — the watcher hashing header_message_id
    // in Python, spamguard reading .eml filenames off disk, rocky guessing
    // — exists because search_email didn't tell callers the canonical id.
    // These tests pin the new contract: search_email enriches every
    // message with a canonical_id computed identically to the puller's
    // derivation. If this invariant breaks, all three consumers regress
    // simultaneously.

    fn sample_message(message_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "69e2473cad75c10d44337093",
            "header_message_id": message_id,
            "subject": "probe",
            "folder_path": "INBOX",
        })
    }

    #[test]
    fn enrich_adds_canonical_id_to_bare_list_response() {
        let mut v = serde_json::json!([
            sample_message("<abc@example.com>"),
            sample_message("<def@example.com>"),
        ]);
        enrich_search_results_with_canonical_id(&mut v);
        let arr = v.as_array().expect("list");
        for item in arr {
            let cid = item.get("canonical_id").and_then(|c| c.as_str()).expect("canonical_id present");
            assert_eq!(cid.len(), 16, "canonical id is always 16 hex chars");
            assert!(cid.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // Distinct Message-IDs must hash to distinct canonical_ids.
        assert_ne!(arr[0]["canonical_id"], arr[1]["canonical_id"]);
    }

    #[test]
    fn enrich_handles_wrapped_messages_shape() {
        let mut v = serde_json::json!({
            "total": 1,
            "messages": [ sample_message("<wrapped@example.com>") ]
        });
        enrich_search_results_with_canonical_id(&mut v);
        let cid = v["messages"][0]["canonical_id"].as_str().expect("canonical_id");
        assert_eq!(cid.len(), 16);
    }

    #[test]
    fn enrich_matches_puller_derivation_byte_for_byte() {
        // The invariant that makes "search → get_email" actually work:
        // search_email must produce the same canonical_id that the
        // puller wrote on disk. Compute both paths and compare.
        let mid = "<roundtrip-check@hld.ca>";
        let mut v = serde_json::json!([ sample_message(mid) ]);
        enrich_search_results_with_canonical_id(&mut v);
        let from_search = v[0]["canonical_id"].as_str().unwrap().to_string();
        let from_puller = crate::pull::mail::hash_to_canonical(mid);
        assert_eq!(from_search, from_puller,
            "search_email canonical_id diverged from puller's — get_email will 404 and rocky will think MCP is down");
    }

    #[test]
    fn enrich_skips_messages_without_header_message_id() {
        // Drafts and malformed inbound mail may have no Message-ID. We
        // leave canonical_id off rather than inventing one — callers can
        // decide whether to skip or fall back.
        let mut v = serde_json::json!([{
            "id": "no-message-id-at-all",
            "subject": "draft"
        }]);
        enrich_search_results_with_canonical_id(&mut v);
        assert!(v[0].get("canonical_id").is_none());
    }

    #[test]
    fn enrich_is_a_no_op_on_non_list_responses() {
        // forwardemail 404s or error JSON must pass through unchanged.
        let mut v = serde_json::json!({"error": "boom"});
        let before = v.clone();
        enrich_search_results_with_canonical_id(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn message_not_indexed_error_avoids_infra_trigger_words() {
        let msg = message_not_indexed_error("deadbeefcafef00d");
        // These are the specific words/phrases that caused the
        // misdiagnosis. If a future refactor wants to use any of
        // them, it must update the rocky prompt guidance in
        // nixos/saturn/configuration.nix at the same time.
        for bad in [
            "backup tree",
            "server down",
            "unreachable",
            "connection",
        ] {
            assert!(
                !msg.to_lowercase().contains(bad),
                "error text must not say {bad:?}: {msg}"
            );
        }
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
    fn extract_body_text_single_part_plain_returns_body_verbatim() {
        let msg = "Subject: hi\r\n\r\nhello there";
        let out = extract_body_text(msg, Some("text/plain"));
        assert_eq!(out, "hello there");
    }

    #[test]
    fn extract_body_text_multipart_alternative_prefers_text_plain() {
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
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"abc\""));
        assert!(out.contains("plain version"), "got: {out:?}");
        assert!(!out.contains("<p>"), "should not contain html tags");
    }

    #[test]
    fn extract_body_text_html_only_strips_tags() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"xyz\"\r\n\r\n",
            "--xyz\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<html><body><p>hello <b>world</b></p></body></html>\r\n",
            "--xyz--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"xyz\""));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
        assert!(!out.contains("<b>"));
    }

    #[test]
    fn extract_body_text_decodes_quoted_printable_html_part() {
        // Real-world shape: Apple/Amazon-style emails send only a
        // text/html part with Content-Transfer-Encoding:
        // quoted-printable. Previously the agent saw =3D/=20/soft-break
        // garbage and had to reimplement a QP decoder in Python.
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n",
            "--b\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "<p>Order total: =E2=82=AC1,=\r\n429.00</p><br><p>Thanks!</p>\r\n",
            "--b--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"b\""));
        assert!(
            out.contains("Order total: €1,429.00"),
            "QP decode + soft-break handling failed: {out:?}"
        );
        assert!(out.contains("Thanks!"));
        assert!(!out.contains("=E2=82=AC"), "should have decoded =XX escapes");
        assert!(!out.contains("<p>"), "should have stripped tags");
    }

    #[test]
    fn extract_body_text_decodes_base64_plain_part() {
        use base64::Engine;
        let payload = "hello base64 world";
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let msg = format!(
            "Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
             --b\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n\
             {encoded}\r\n\
             --b--\r\n"
        );
        let out = extract_body_text(&msg, Some("multipart/alternative; boundary=\"b\""));
        assert_eq!(out.trim(), payload);
    }

    #[test]
    fn extract_body_text_html_turns_br_and_p_into_newlines() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"z\"\r\n\r\n",
            "--z\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>Line one</p><p>Line two</p>Line<br>three\r\n",
            "--z--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"z\""));
        let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        assert!(lines.iter().any(|l| *l == "Line one"), "got: {out:?}");
        assert!(lines.iter().any(|l| *l == "Line two"));
        // <br> also produces a newline, so "Line" and "three" land on
        // separate lines — just verify both survived.
        assert!(lines.iter().any(|l| *l == "Line"), "got: {out:?}");
        assert!(lines.iter().any(|l| *l == "three"), "got: {out:?}");
        assert!(
            !out.contains("Line oneLine two"),
            "block tags should insert newlines: {out:?}"
        );
    }

    #[test]
    fn extract_body_text_html_decodes_entities() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"e\"\r\n\r\n",
            "--e\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>Caf&eacute;&nbsp;&amp;&nbsp;bar &mdash; 5&#8217;ish, price &#x20AC;3.50</p>\r\n",
            "--e--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"e\""));
        assert!(out.contains("&") && out.contains("bar"), "got: {out:?}");
        assert!(out.contains("—"), "&mdash; should decode: {out:?}");
        // &#8217; is U+2019 RIGHT SINGLE QUOTATION MARK, not ASCII '.
        assert!(out.contains("\u{2019}ish"), "&#8217; should decode: {out:?}");
        assert!(out.contains("€3.50"), "&#x20AC; should decode: {out:?}");
    }

    #[test]
    fn extract_body_text_html_drops_script_and_style_blocks() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"s\"\r\n\r\n",
            "--s\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<style>.x{color:red}</style><p>Visible</p><script>var x=1;</script>Tail\r\n",
            "--s--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"s\""));
        assert!(out.contains("Visible"));
        assert!(out.contains("Tail"));
        assert!(!out.contains("color:red"), "style block should be dropped: {out:?}");
        assert!(!out.contains("var x=1"), "script block should be dropped: {out:?}");
    }

    #[test]
    fn extract_body_text_single_part_html_with_qp_decodes_top_level_cte() {
        let msg = concat!(
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "<p>Total =E2=82=AC42</p>\r\n",
        );
        let out = extract_body_text(msg, Some("text/html; charset=utf-8"));
        assert!(out.contains("Total €42"), "got: {out:?}");
        assert!(!out.contains("<p>"));
    }

    #[test]
    fn extract_body_text_collapses_excess_whitespace() {
        let msg = concat!(
            "Content-Type: multipart/alternative; boundary=\"w\"\r\n\r\n",
            "--w\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>a</p>    \n\n\n\n<p>b</p>\r\n",
            "--w--\r\n",
        );
        let out = extract_body_text(msg, Some("multipart/alternative; boundary=\"w\""));
        assert!(!out.contains("\n\n\n"), "got: {out:?}");
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

    // ── Authentication-Results surfacing ──────────────────────────────
    //
    // The watcher's instruction gate (hermes email watcher, data-mode
    // rocky) needs to verify SPF/DKIM/DMARC verdicts on inbound mail
    // before handing it to the LLM. If we drop or clobber AR headers
    // here, an attacker-controlled body can coast past a non-existent
    // gate and the LLM becomes the security boundary — which it isn't.
    // These tests pin every AR value through.

    #[test]
    fn parse_headers_retains_authentication_results() {
        let msg = concat!(
            "From: dan@hld.ca\r\n",
            "Authentication-Results: forwardemail.net;\r\n",
            "  spf=pass smtp.mailfrom=hld.ca;\r\n",
            "  dkim=pass header.i=@hld.ca;\r\n",
            "  dmarc=pass header.from=hld.ca\r\n",
            "\r\nbody",
        );
        let (hs, _) = parse_headers(msg);
        let ar = hs
            .get("authentication-results")
            .expect("AR header retained");
        let arr = ar.as_array().expect("AR stored as array");
        assert_eq!(arr.len(), 1);
        let v = arr[0].as_str().unwrap();
        assert!(v.contains("spf=pass"), "got {v:?}");
        assert!(v.contains("dkim=pass"), "got {v:?}");
        assert!(v.contains("dmarc=pass"), "got {v:?}");
    }

    #[test]
    fn parse_headers_accumulates_multiple_ar_hops() {
        // Real messages often carry several Authentication-Results
        // values — one per handling hop. Dropping the trailing ones
        // makes gate behavior depend on hop ordering, which is outside
        // the sender's control, so both must land in the output.
        let msg = concat!(
            "From: dan@hld.ca\r\n",
            "Authentication-Results: hop1; spf=pass\r\n",
            "Authentication-Results: hop2; dkim=pass\r\n",
            "ARC-Authentication-Results: i=1; arc.saturn; dmarc=pass\r\n",
            "\r\nbody",
        );
        let (hs, _) = parse_headers(msg);
        let ar = hs.get("authentication-results").unwrap().as_array().unwrap();
        assert_eq!(ar.len(), 2);
        assert!(ar[0].as_str().unwrap().contains("hop1"));
        assert!(ar[1].as_str().unwrap().contains("hop2"));

        let arc = hs
            .get("arc-authentication-results")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(arc.len(), 1);
        assert!(arc[0].as_str().unwrap().contains("dmarc=pass"));
    }

    #[test]
    fn parse_headers_absent_ar_means_no_key() {
        let msg = "From: spoofer@elsewhere\r\nSubject: x\r\n\r\nbody";
        let (hs, _) = parse_headers(msg);
        assert!(hs.get("authentication-results").is_none());
        assert!(hs.get("arc-authentication-results").is_none());
    }
}
