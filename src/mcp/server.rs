//! rmcp-based MCP server. The tool implementations live here; most are thin
//! wrappers around `crate::forwardemail::Client` methods with a permission
//! check on the front and a JSON-ready return value on the back.

use crate::forwardemail::Client;
use crate::permission::{Permissions, Resource};
use crate::store::Repo;
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
}

impl std::fmt::Debug for PimstewardServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PimstewardServer")
            .field("alias", &self.inner.alias)
            .finish_non_exhaustive()
    }
}

impl PimstewardServer {
    pub fn new(client: Client, repo: Repo, permissions: Permissions, alias: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                client,
                repo,
                permissions,
                alias,
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

    fn api_error(&self, e: crate::Error) -> McpError {
        McpError::internal_error(format!("forwardemail: {e}"), None)
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

#[tool_router]
impl PimstewardServer {
    #[tool(
        name = "search_email",
        description = "Search email messages via forwardemail's native search. Filter by folder, date range, subject, from, or free-text. Returns message summaries without bodies — use get_email for the full content (not yet implemented in v1)."
    )]
    async fn search_email(
        &self,
        Parameters(p): Parameters<SearchEmailParams>,
    ) -> Result<String, McpError> {
        self.check(Resource::Email)?;

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
                 This server exposes read-only tools for email, calendar, contacts, \
                 and sieve scripts, gated by the configured permission matrix. \
                 Every mutation (not yet in v1) will produce an attributed git commit.",
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
