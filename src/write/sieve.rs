//! Sieve script write operations.

use crate::error::Error;
use crate::forwardemail::managesieve;
use crate::forwardemail::sieve::SieveScript;
use crate::forwardemail::Client;
use crate::pull::sieve::pull_sieve;
use crate::store::Repo;
use crate::write::audit::{Attribution, WriteAudit};
use regex::Regex;

pub async fn install_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    name: &str,
    content: &str,
) -> Result<SieveScript, Error> {
    let created = client.create_sieve_script(name, content).await?;
    // Early warning: surface server-side validation issues before committing.
    if !created.is_valid {
        return Err(Error::Api {
            status: 422,
            message: format!(
                "sieve script '{name}' was accepted by forwardemail but flagged as invalid: {:?}",
                created.validation_errors
            ),
        });
    }
    let audit = WriteAudit {
        attribution,
        tool: "install_sieve_script",
        resource: "sieve",
        resource_id: created.id.clone(),
        args: serde_json::json!({"name": name, "content_bytes": content.len()}),
        summary: format!("sieve: install {name}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(created)
}

pub async fn update_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
    content: &str,
) -> Result<SieveScript, Error> {
    let updated = client.update_sieve_script(id, content).await?;
    let audit = WriteAudit {
        attribution,
        tool: "update_sieve_script",
        resource: "sieve",
        resource_id: id.to_string(),
        args: serde_json::json!({"content_bytes": content.len()}),
        summary: format!("sieve: update {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

pub async fn delete_sieve_script(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    id: &str,
) -> Result<(), Error> {
    client.delete_sieve_script(id).await?;
    let audit = WriteAudit {
        attribution,
        tool: "delete_sieve_script",
        resource: "sieve",
        resource_id: id.to_string(),
        args: serde_json::json!({}),
        summary: format!("sieve: delete {id}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(())
}

/// Append a rule to the currently active sieve script. High-level
/// alternative to `install_sieve_script` for the common case of "add one
/// more rule to my filters". Atomic: fetches the active script, merges
/// `require [...]` capabilities, appends the rule body, and updates in
/// place. Errors if no script is currently active.
pub async fn add_sieve_rule(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    ms: &crate::mcp::ManageSieveConfig,
    rule_text: &str,
    comment: Option<&str>,
) -> Result<SieveScript, Error> {
    let active_name = managesieve::get_active_script(&ms.host, ms.port, &ms.user, &ms.password)
        .await?
        .ok_or_else(|| Error::Api {
            status: 409,
            message: "no active sieve script — call install_sieve_script + activate_sieve_script first to create one".to_string(),
        })?;

    // Find the active script's id via REST list.
    let scripts = client.list_sieve_scripts().await?;
    let active = scripts
        .into_iter()
        .find(|s| s.name == active_name)
        .ok_or_else(|| Error::Api {
            status: 500,
            message: format!(
                "ManageSieve reports active script '{active_name}' but it is not in the REST list"
            ),
        })?;

    // Fetch full content (list response omits `content`).
    let full = client.get_sieve_script(&active.id).await?;
    let existing = full.content.unwrap_or_default();

    let merged = merge_sieve_with_rule(&existing, rule_text, comment);
    let updated = client.update_sieve_script(&active.id, &merged).await?;
    if !updated.is_valid {
        return Err(Error::Api {
            status: 422,
            message: format!(
                "merged sieve script accepted by forwardemail but flagged as invalid: {:?}",
                updated.validation_errors
            ),
        });
    }
    let audit = WriteAudit {
        attribution,
        tool: "add_sieve_rule",
        resource: "sieve",
        resource_id: active.id.clone(),
        args: serde_json::json!({
            "rule_bytes": rule_text.len(),
            "merged_bytes": merged.len(),
            "comment": comment,
            "active_script": active_name,
        }),
        summary: format!("sieve: add rule to {active_name}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

/// Merge a new sieve rule into an existing sieve script body.
///
/// Combines all `require [...]` capability lists from both inputs into a
/// single `require [...]` line at the top, preserving order (existing
/// caps first, then any new ones from the rule), then appends the rule
/// body (with its own require stripped) after the existing body, with an
/// optional `# comment` header above the new rule.
pub fn merge_sieve_with_rule(existing: &str, rule_text: &str, comment: Option<&str>) -> String {
    let (mut caps, existing_body) = extract_requires(existing);
    let (rule_caps, rule_body) = extract_requires(rule_text);
    for cap in rule_caps {
        if !caps.contains(&cap) {
            caps.push(cap);
        }
    }

    let mut out = String::new();
    if !caps.is_empty() {
        let joined = caps
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("require [{joined}];\n\n"));
    }
    let trimmed_existing = existing_body.trim();
    if !trimmed_existing.is_empty() {
        out.push_str(trimmed_existing);
        out.push_str("\n\n");
    }
    if let Some(c) = comment {
        // Treat each line as a separate comment line for safety.
        for line in c.lines() {
            out.push_str(&format!("# {line}\n"));
        }
    }
    out.push_str(rule_body.trim());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Extract every `require [...]` capability list from a sieve script.
/// Returns `(unique_caps_in_order, script_with_require_lines_stripped)`.
fn extract_requires(s: &str) -> (Vec<String>, String) {
    // (?s) so `.` and `[^]]` work across newlines for multi-line require
    // declarations like:
    //   require [
    //       "fileinto",
    //       "mailbox"
    //   ];
    let re = Regex::new(r#"(?s)require\s*\[\s*((?:"[^"]+"\s*,?\s*)+)\s*\]\s*;"#).unwrap();
    let mut caps: Vec<String> = Vec::new();
    for m in re.captures_iter(s) {
        for piece in m[1].split(',') {
            let trimmed = piece.trim();
            if let Some(stripped) = trimmed.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                let cap = stripped.to_string();
                if !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
        }
    }
    let stripped = re.replace_all(s, "").into_owned();
    (caps, stripped)
}

async fn refresh(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    audit: &WriteAudit<'_>,
) -> Result<(), Error> {
    let _ = pull_sieve(
        client,
        repo,
        alias,
        &attribution.caller,
        &attribution.caller_email,
    )
    .await?;
    let msg = audit.commit_message();
    let sha = repo.commit_all(&attribution.caller, &attribution.caller_email, &msg)?;
    if sha.is_none() {
        repo.empty_commit(&attribution.caller, &attribution.caller_email, &msg)?;
    }
    Ok(())
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn extracts_single_line_requires() {
        let (caps, body) = extract_requires(
            r#"require ["fileinto", "mailbox"];

if header :contains "subject" "x" { fileinto "Junk"; }"#,
        );
        assert_eq!(caps, vec!["fileinto", "mailbox"]);
        assert!(!body.contains("require"));
        assert!(body.contains("fileinto \"Junk\""));
    }

    #[test]
    fn extracts_multiline_requires() {
        let (caps, _) = extract_requires("require [\n    \"fileinto\",\n    \"mailbox\"\n];\n");
        assert_eq!(caps, vec!["fileinto", "mailbox"]);
    }

    #[test]
    fn dedupes_capabilities_across_blocks() {
        let (caps, _) = extract_requires(
            r#"require ["fileinto"];
require ["fileinto", "discard"];"#,
        );
        assert_eq!(caps, vec!["fileinto", "discard"]);
    }

    #[test]
    fn merge_combines_requires_and_appends_body() {
        let existing = r#"require ["fileinto"];

if header :contains "subject" "old" { fileinto "Trash"; }
"#;
        let rule = r#"require ["discard"];

if header :contains "subject" "spam" { discard; }
"#;
        let merged = merge_sieve_with_rule(existing, rule, Some("spam rule"));
        assert!(
            merged.starts_with("require [\"fileinto\", \"discard\"];\n"),
            "merged output should open with the unioned require: {merged}"
        );
        assert!(merged.contains("\"old\""));
        assert!(merged.contains("\"spam\""));
        assert!(merged.contains("# spam rule"));
        // The original require lines from each input should be gone.
        let body_after_first_require = &merged[merged.find("];").unwrap() + 2..];
        assert!(!body_after_first_require.contains("require ["));
    }

    #[test]
    fn merge_preserves_comment_lines() {
        let merged = merge_sieve_with_rule("", r#"if true { keep; }"#, Some("line one\nline two"));
        assert!(merged.contains("# line one\n# line two\n"));
    }

    #[test]
    fn merge_handles_empty_existing() {
        let rule = r#"require ["fileinto"]; if true { fileinto "X"; }"#;
        let merged = merge_sieve_with_rule("", rule, None);
        assert!(merged.starts_with("require [\"fileinto\"];"));
        assert!(merged.contains("\"X\""));
    }

    #[test]
    fn merge_handles_existing_without_requires() {
        let merged = merge_sieve_with_rule(
            r#"if true { keep; }"#,
            r#"require ["discard"]; if false { discard; }"#,
            None,
        );
        assert!(merged.starts_with("require [\"discard\"];"));
        assert!(merged.contains("keep"));
        assert!(merged.contains("discard"));
    }
}
