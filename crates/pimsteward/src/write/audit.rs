//! Attribution + commit-message helpers for write operations.
//!
//! Every commit made by a write tool embeds a structured YAML header in the
//! git commit message so `git log` can be grep'd for tool, caller, or
//! session. The human-readable summary goes on the commit subject line.

use serde::{Deserialize, Serialize};

/// Who is making a write. Includes the caller's identity (MCP session name
/// for AI writes, "manual" for CLI writes, "pimsteward-pull" for the
/// polling loop) and an optional free-text reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attribution {
    pub caller: String,
    /// Email-like identifier used as the git commit author email.
    pub caller_email: String,
    /// Free-text justification the caller attached to this write. This is
    /// what the AI explains to its user for the record.
    pub reason: Option<String>,
}

impl Attribution {
    pub fn new(caller: impl Into<String>, reason: Option<String>) -> Self {
        let c = caller.into();
        let email = format!("{}@pimsteward.local", c);
        Self {
            caller: c,
            caller_email: email,
            reason,
        }
    }
}

/// Data gathered during a write call, formatted into the commit message
/// body on success.
pub struct WriteAudit<'a> {
    pub attribution: &'a Attribution,
    pub tool: &'static str,
    pub resource: &'static str,
    pub resource_id: String,
    pub args: serde_json::Value,
    pub summary: String,
}

impl WriteAudit<'_> {
    /// Build the full commit message (subject + body).
    pub fn commit_message(&self) -> String {
        let mut body = String::new();
        body.push_str(&self.summary);
        body.push_str("\n\n---\n");
        body.push_str(&format!("tool: {}\n", self.tool));
        body.push_str(&format!("resource: {}\n", self.resource));
        body.push_str(&format!("resource_id: {}\n", self.resource_id));
        body.push_str(&format!("caller: {}\n", self.attribution.caller));
        if let Some(reason) = &self.attribution.reason {
            // quote to keep yaml valid even with colons in the reason
            body.push_str(&format!("reason: {}\n", yaml_quote(reason)));
        }
        body.push_str(&format!("args: {}\n", self.args));
        body.push_str("---\n");
        body
    }
}

fn yaml_quote(s: &str) -> String {
    // Safe single-line quoted string. Double any embedded double-quotes.
    format!("\"{}\"", s.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_includes_all_attribution_fields() {
        let attr = Attribution::new("rockycc", Some("user asked me to".into()));
        let audit = WriteAudit {
            attribution: &attr,
            tool: "update_contact_name",
            resource: "contacts",
            resource_id: "abc123".into(),
            args: serde_json::json!({"full_name": "New Name"}),
            summary: "contacts: update_contact_name abc123".into(),
        };
        let msg = audit.commit_message();
        assert!(msg.contains("tool: update_contact_name"));
        assert!(msg.contains("resource: contacts"));
        assert!(msg.contains("caller: rockycc"));
        assert!(msg.contains("user asked me to"));
        assert!(msg.contains("contacts: update_contact_name abc123"));
    }

    #[test]
    fn attribution_email_derived_from_caller() {
        let a = Attribution::new("ai-session-42", None);
        assert_eq!(a.caller_email, "ai-session-42@pimsteward.local");
    }
}
