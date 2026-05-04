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

/// Name pimsteward uses for the canonical sieve script when bootstrapping
/// from nothing. The rule-centric tool surface treats this script as the
/// only one — every rule lives here.
pub const CANONICAL_SCRIPT_NAME: &str = "main";

/// Append a rule to the canonical sieve script.
///
/// If no script is currently active, bootstraps `main` with this rule as
/// its only content and activates it. If a script is active but not named
/// `main`, the active script is used as-is (callers should consolidate
/// manually before relying on the rule-centric tools — see
/// `CANONICAL_SCRIPT_NAME`).
///
/// Atomic from the caller's perspective: fetches the active script,
/// merges `require [...]` capabilities, appends the rule body, and
/// updates in place.
pub async fn add_sieve_rule(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    ms: &crate::mcp::ManageSieveConfig,
    rule_text: &str,
    comment: Option<&str>,
) -> Result<SieveScript, Error> {
    // Validate every fileinto target in the new rule against the alias's
    // real folder list. Auto-correct case-only mismatches; reject anything
    // else with a helpful error so the caller can ask a clarifying
    // question instead of silently filing mail into a non-existent folder.
    let folders = client.list_folders().await?;
    let folder_paths: Vec<String> = folders.into_iter().map(|f| f.path).collect();
    let rule_text_owned = match validate_fileinto_targets(rule_text, &folder_paths) {
        Ok(t) => t,
        Err(mismatch) => {
            return Err(Error::Api {
                status: 422,
                message: mismatch.message(&folder_paths),
            });
        }
    };
    let rule_text = rule_text_owned.as_str();

    let active = resolve_or_bootstrap_active(client, ms, rule_text).await?;

    // If we just bootstrapped, the script's content already contains the
    // rule — nothing to append. Audit + commit and return.
    if active.bootstrapped {
        let audit = WriteAudit {
            attribution,
            tool: "add_sieve_rule",
            resource: "sieve",
            resource_id: active.script.id.clone(),
            args: serde_json::json!({
                "rule_bytes": rule_text.len(),
                "merged_bytes": rule_text.len(),
                "comment": comment,
                "active_script": active.script.name,
                "bootstrapped": true,
            }),
            summary: format!("sieve: bootstrap {} with first rule", active.script.name),
        };
        refresh(client, repo, alias, attribution, &audit).await?;
        return Ok(active.script);
    }

    // Existing active script — fetch content, append, update.
    let full = client.get_sieve_script(&active.script.id).await?;
    let existing = full.content.unwrap_or_default();
    let merged = merge_sieve_with_rule(&existing, rule_text, comment);
    let updated = client
        .update_sieve_script(&active.script.id, &merged)
        .await?;
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
        resource_id: active.script.id.clone(),
        args: serde_json::json!({
            "rule_bytes": rule_text.len(),
            "merged_bytes": merged.len(),
            "comment": comment,
            "active_script": active.script.name,
        }),
        summary: format!("sieve: add rule to {}", active.script.name),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

/// Remove a rule from the active sieve script by its name (the first
/// comment line above the rule body, e.g. "RASCals mailing list ..." for
/// a rule prefixed with `# RASCals mailing list ...`). Errors with HTTP
/// 404 if no rule with that name is found.
pub async fn remove_sieve_rule(
    client: &Client,
    repo: &Repo,
    alias: &str,
    attribution: &Attribution,
    ms: &crate::mcp::ManageSieveConfig,
    rule_name: &str,
) -> Result<SieveScript, Error> {
    let active_name = managesieve::get_active_script(&ms.host, ms.port, &ms.user, &ms.password)
        .await?
        .ok_or_else(|| Error::Api {
            status: 404,
            message: "no active sieve script — nothing to remove".to_string(),
        })?;

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
    let full = client.get_sieve_script(&active.id).await?;
    let existing = full.content.unwrap_or_default();

    let new_content = remove_rule_from_content(&existing, rule_name).ok_or_else(|| Error::Api {
        status: 404,
        message: format!("no rule named '{rule_name}' in active sieve script '{active_name}'"),
    })?;

    let updated = client.update_sieve_script(&active.id, &new_content).await?;
    if !updated.is_valid {
        return Err(Error::Api {
            status: 422,
            message: format!(
                "sieve script after rule removal accepted by forwardemail but flagged as invalid: {:?}",
                updated.validation_errors
            ),
        });
    }
    let audit = WriteAudit {
        attribution,
        tool: "remove_sieve_rule",
        resource: "sieve",
        resource_id: active.id.clone(),
        args: serde_json::json!({
            "rule_name": rule_name,
            "active_script": active_name,
            "merged_bytes": new_content.len(),
        }),
        summary: format!("sieve: remove rule '{rule_name}' from {active_name}"),
    };
    refresh(client, repo, alias, attribution, &audit).await?;
    Ok(updated)
}

struct ResolvedActive {
    script: SieveScript,
    /// True if we just created + activated this script in this call.
    bootstrapped: bool,
}

/// Find the alias's active sieve script, or create the canonical `main`
/// script + activate it if no script is currently active.
///
/// On the bootstrap path we install `main` with `bootstrap_content` as
/// its body — the caller's rule text — and activate it via ManageSieve.
async fn resolve_or_bootstrap_active(
    client: &Client,
    ms: &crate::mcp::ManageSieveConfig,
    bootstrap_content: &str,
) -> Result<ResolvedActive, Error> {
    if let Some(active_name) =
        managesieve::get_active_script(&ms.host, ms.port, &ms.user, &ms.password).await?
    {
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
        return Ok(ResolvedActive {
            script: active,
            bootstrapped: false,
        });
    }

    // No active script. Create or replace `main` and activate it.
    let normalized = if bootstrap_content.trim().is_empty() {
        String::new()
    } else {
        // Re-emit through the merge function so the bootstrap content
        // gets the same require-line normalization as appended rules.
        merge_sieve_with_rule("", bootstrap_content, None)
    };

    // If a `main` script already exists (perhaps deactivated), update
    // it; otherwise create. This avoids the "Sieve script with that name
    // already exists" 422 from forwardemail.
    let scripts = client.list_sieve_scripts().await?;
    let script = if let Some(existing) = scripts
        .into_iter()
        .find(|s| s.name == CANONICAL_SCRIPT_NAME)
    {
        client
            .update_sieve_script(&existing.id, &normalized)
            .await?
    } else {
        client
            .create_sieve_script(CANONICAL_SCRIPT_NAME, &normalized)
            .await?
    };
    if !script.is_valid {
        return Err(Error::Api {
            status: 422,
            message: format!(
                "bootstrap sieve script accepted by forwardemail but flagged as invalid: {:?}",
                script.validation_errors
            ),
        });
    }
    managesieve::activate_script(
        &ms.host,
        ms.port,
        &ms.user,
        &ms.password,
        CANONICAL_SCRIPT_NAME,
    )
    .await?;
    Ok(ResolvedActive {
        script,
        bootstrapped: true,
    })
}

/// Parsed view of one rule inside a sieve script.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SieveRule {
    /// 1-based position within the script (excluding the require block).
    pub index: usize,
    /// First comment line above the rule, with the leading `# ` stripped.
    /// Empty if the rule has no comment header.
    pub name: String,
    /// All lines of the rule (comment header + body), exactly as they
    /// appear in the script.
    pub text: String,
}

/// Parse the rules out of a sieve script body. The require declaration
/// (if any) is dropped; everything else is split on blank-line
/// boundaries into rule blocks. Blocks that contain no executable
/// statement (e.g. orphan comments) are skipped.
pub fn parse_sieve_rules(content: &str) -> Vec<SieveRule> {
    let (_, body) = extract_requires(content);
    let mut rules = Vec::new();
    for block in body.split("\n\n") {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let has_action = trimmed.lines().any(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        });
        if !has_action {
            continue;
        }
        let name = trimmed
            .lines()
            .find_map(|l| {
                let t = l.trim_start();
                t.strip_prefix('#').map(|rest| rest.trim().to_string())
            })
            .unwrap_or_default();
        rules.push(SieveRule {
            index: rules.len() + 1,
            name,
            text: trimmed.to_string(),
        });
    }
    rules
}

/// Remove the rule named `rule_name` from `content` and return the new
/// content. Returns `None` if no rule with that name exists.
///
/// Preserves the require declaration and the surviving rules' order.
fn remove_rule_from_content(content: &str, rule_name: &str) -> Option<String> {
    let rules = parse_sieve_rules(content);
    let target_idx = rules.iter().position(|r| r.name == rule_name)?;
    let (caps, _) = extract_requires(content);
    let caps = filter_known_extensions(caps);

    let mut out = String::new();
    if !caps.is_empty() {
        let joined = caps
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("require [{joined}];\n\n"));
    }
    for (i, rule) in rules.iter().enumerate() {
        if i == target_idx {
            continue;
        }
        out.push_str(&rule.text);
        out.push_str("\n\n");
    }
    // Strip the trailing extra blank.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Reported when a `fileinto` target in a new rule doesn't match any
/// folder on the alias (and isn't a case-only difference from one).
#[derive(Debug, Clone)]
pub struct FolderMismatch {
    /// The mailbox literal as written in the rule (e.g. `"Archve/2024"`).
    pub target: String,
    /// Folder paths that share a case-insensitive substring with the
    /// target — best effort hint for the caller.
    pub suggestions: Vec<String>,
}

impl FolderMismatch {
    pub fn message(&self, folder_paths: &[String]) -> String {
        let suggestion_str = if self.suggestions.is_empty() {
            String::new()
        } else {
            format!(" Did you mean one of: {:?}?", self.suggestions)
        };
        format!(
            "fileinto target {:?} does not match any existing folder.{} \
             Available folders: {:?}. Either correct the target (case-only \
             differences are auto-corrected) or create the folder first.",
            self.target, suggestion_str, folder_paths
        )
    }
}

/// Extract the mailbox argument of every `fileinto "..."` action in
/// `rule_text`. Skips fileinto inside `# ` comment lines and handles
/// optional tag arguments (`:copy`, `:create`, `:flags [...]`) by
/// taking the last quoted string before the statement-terminating `;`.
pub fn extract_fileinto_targets(rule_text: &str) -> Vec<String> {
    let stripped: String = rule_text
        .lines()
        .map(|l| {
            if l.trim_start().starts_with('#') {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let stmt_re = Regex::new(r"(?s)\bfileinto\b([^;]*);").unwrap();
    let str_re = Regex::new(r#""([^"]*)""#).unwrap();
    let mut out = Vec::new();
    for cap in stmt_re.captures_iter(&stripped) {
        let segment = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Some(last) = str_re.captures_iter(segment).last() {
            out.push(last[1].to_string());
        }
    }
    out
}

/// Validate every `fileinto` mailbox in `rule_text` against
/// `folder_paths` (canonical paths from the server).
///
/// - Exact match → kept as-is.
/// - Case-insensitive exact match → the rule text is rewritten to use
///   the canonical case, transparently.
/// - No match → returns `Err(FolderMismatch)` with the offending target
///   and a list of similar folder paths.
///
/// Returns the (possibly corrected) rule text on success.
pub fn validate_fileinto_targets(
    rule_text: &str,
    folder_paths: &[String],
) -> Result<String, FolderMismatch> {
    let targets = extract_fileinto_targets(rule_text);
    let mut corrected = rule_text.to_string();
    for target in targets {
        if folder_paths.iter().any(|f| f == &target) {
            continue;
        }
        if let Some(canonical) = folder_paths
            .iter()
            .find(|f| f.eq_ignore_ascii_case(&target))
        {
            let from = format!("\"{target}\"");
            let to = format!("\"{canonical}\"");
            corrected = corrected.replace(&from, &to);
            continue;
        }
        let lower_target = target.to_ascii_lowercase();
        let mut suggestions: Vec<String> = folder_paths
            .iter()
            .filter(|f| {
                let l = f.to_ascii_lowercase();
                l.contains(&lower_target) || lower_target.contains(&l)
            })
            .cloned()
            .collect();
        suggestions.sort();
        suggestions.dedup();
        return Err(FolderMismatch {
            target,
            suggestions,
        });
    }
    Ok(corrected)
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
    let caps = filter_known_extensions(caps);

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

/// Sieve extensions advertised by Forward Email's ManageSieve server, per
/// https://forwardemail.net/en/faq#do-you-support-sieve-email-filtering.
///
/// `require [...]` is an extension declaration — it must list only true
/// extensions. RFC 5228 §3.2 says an unknown capability is an error and the
/// runtime must not begin execution. Forward Email follows this correctly:
/// scripts that require a non-extension (e.g. the base RFC 5228 action
/// `discard`) are accepted by their validator but silently skipped at
/// delivery, so mail falls through to default INBOX. We filter the merged
/// require list against this allowlist on every rebuild so a stale `discard`
/// (or any other non-extension) gets stripped automatically.
pub const KNOWN_SIEVE_EXTENSIONS: &[&str] = &[
    "fileinto",
    "reject",
    "ereject",
    "vacation",
    "vacation-seconds",
    "imap4flags",
    "envelope",
    "body",
    "variables",
    "relational",
    "comparator-i;ascii-numeric",
    "copy",
    "editheader",
    "date",
    "index",
    "regex",
    "enotify",
    "environment",
    "mailbox",
    "special-use",
    "duplicate",
    "ihave",
    "subaddress",
];

/// Drop any capability that isn't in `KNOWN_SIEVE_EXTENSIONS`. Order is
/// preserved for the survivors.
fn filter_known_extensions(caps: Vec<String>) -> Vec<String> {
    caps.into_iter()
        .filter(|c| KNOWN_SIEVE_EXTENSIONS.contains(&c.as_str()))
        .collect()
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
        let rule = r#"require ["envelope"];

if envelope :is "to" "x@y.z" { fileinto "Junk"; }
"#;
        let merged = merge_sieve_with_rule(existing, rule, Some("env rule"));
        assert!(
            merged.starts_with("require [\"fileinto\", \"envelope\"];\n"),
            "merged output should open with the unioned require: {merged}"
        );
        assert!(merged.contains("\"old\""));
        assert!(merged.contains("\"x@y.z\""));
        assert!(merged.contains("# env rule"));
        // The original require lines from each input should be gone.
        let body_after_first_require = &merged[merged.find("];").unwrap() + 2..];
        assert!(!body_after_first_require.contains("require ["));
    }

    #[test]
    fn merge_drops_non_extension_capabilities_from_require() {
        // `discard` is a base RFC 5228 action, not an extension. Forward
        // Email's runtime silently refuses to execute scripts that require
        // unknown caps, so we must never emit `discard` in require.
        let merged = merge_sieve_with_rule(
            r#"require ["fileinto", "discard"];

if true { keep; }
"#,
            r#"if header :contains "subject" "spam" { discard; stop; }"#,
            None,
        );
        assert!(
            merged.starts_with("require [\"fileinto\"];\n"),
            "discard must be filtered out of require: {merged}"
        );
        // The discard *action* in the rule body is preserved — only the
        // bogus require entry is dropped.
        assert!(merged.contains("discard;"));
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
            r#"require ["fileinto"]; if false { fileinto "X"; }"#,
            None,
        );
        assert!(merged.starts_with("require [\"fileinto\"];"));
        assert!(merged.contains("keep"));
        assert!(merged.contains("fileinto \"X\""));
    }

    #[test]
    fn parse_rules_splits_blocks_and_extracts_names() {
        let script = r#"require ["fileinto", "discard"];

# rule alpha
if header :contains "subject" "alpha" { fileinto "Trash"; stop; }

# rule beta
# (continuation of beta's comment)
if header :contains "subject" "beta" { discard; stop; }
"#;
        let rules = parse_sieve_rules(script);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "rule alpha");
        assert_eq!(rules[0].index, 1);
        assert!(rules[0].text.contains("\"alpha\""));
        assert_eq!(rules[1].name, "rule beta");
        assert_eq!(rules[1].index, 2);
        assert!(rules[1].text.contains("\"beta\""));
    }

    #[test]
    fn parse_rules_skips_orphan_comments() {
        let script = r#"require ["fileinto"];

# this is a header comment with no rule

# rule one
if true { fileinto "X"; }
"#;
        let rules = parse_sieve_rules(script);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "rule one");
    }

    #[test]
    fn parse_rules_handles_unnamed_rules() {
        let script = "require [\"fileinto\"];\n\nif true { fileinto \"X\"; }\n";
        let rules = parse_sieve_rules(script);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "");
    }

    #[test]
    fn remove_rule_drops_named_block_and_preserves_others() {
        let script = r#"require ["fileinto", "discard"];

# rule alpha
if header :contains "subject" "alpha" { fileinto "Trash"; stop; }

# rule beta
if header :contains "subject" "beta" { discard; stop; }
"#;
        let after = remove_rule_from_content(script, "rule alpha").expect("alpha removed");
        assert!(
            !after.contains("\"alpha\""),
            "alpha rule should be gone: {after}"
        );
        assert!(
            after.contains("\"beta\""),
            "beta rule should remain: {after}"
        );
        // `discard` is a base RFC 5228 action (not an extension) — even if
        // a stale `require` listed it, the rebuild filters it out.
        assert!(
            after.starts_with("require [\"fileinto\"];"),
            "require should keep only known extensions: {after}"
        );
    }

    #[test]
    fn remove_rule_returns_none_when_name_missing() {
        let script = "require [\"fileinto\"];\n\n# only rule\nif true { fileinto \"X\"; }\n";
        assert!(remove_rule_from_content(script, "nonexistent").is_none());
    }

    #[test]
    fn extract_fileinto_targets_simple_rule() {
        let rule = r#"if header :contains "subject" "x" { fileinto "Trash"; stop; }"#;
        assert_eq!(extract_fileinto_targets(rule), vec!["Trash"]);
    }

    #[test]
    fn extract_fileinto_targets_skips_comments() {
        let rule = r#"# previously: fileinto "Old";
if true { fileinto "New"; }"#;
        assert_eq!(extract_fileinto_targets(rule), vec!["New"]);
    }

    #[test]
    fn extract_fileinto_targets_handles_tag_arguments() {
        // :flags has its own quoted strings — the mailbox is the last one.
        let rule = r#"if true { fileinto :copy :flags ["\\Seen"] "Archive/2024"; }"#;
        assert_eq!(extract_fileinto_targets(rule), vec!["Archive/2024"]);
    }

    #[test]
    fn extract_fileinto_targets_multiple_actions() {
        let rule = r#"if header :is "from" "a" { fileinto "A"; }
if header :is "from" "b" { fileinto "B/Sub"; }"#;
        assert_eq!(extract_fileinto_targets(rule), vec!["A", "B/Sub"]);
    }

    #[test]
    fn validate_passes_through_exact_match() {
        let folders = vec!["Trash".to_string(), "Groups/RASC".to_string()];
        let rule = r#"if true { fileinto "Groups/RASC"; }"#;
        let corrected = validate_fileinto_targets(rule, &folders).unwrap();
        assert_eq!(corrected, rule);
    }

    #[test]
    fn validate_auto_corrects_case_only_difference() {
        let folders = vec!["Trash".to_string(), "Groups/RASC".to_string()];
        let rule = r#"# files into groups folder
if true { fileinto "groups/rasc"; }"#;
        let corrected = validate_fileinto_targets(rule, &folders).unwrap();
        assert!(
            corrected.contains("\"Groups/RASC\""),
            "case-only mismatch should be rewritten to canonical: {corrected}"
        );
        assert!(
            !corrected.contains("\"groups/rasc\""),
            "old casing should be gone from rule body: {corrected}"
        );
        // Comments are descriptive prose — leave them alone.
        assert!(corrected.contains("# files into groups folder"));
    }

    #[test]
    fn validate_rejects_unknown_folder_with_suggestions() {
        let folders = vec![
            "Trash".to_string(),
            "Archive".to_string(),
            "Archive/2024".to_string(),
        ];
        let rule = r#"if true { fileinto "Archve"; }"#; // typo, no case fix
        let err = validate_fileinto_targets(rule, &folders).unwrap_err();
        assert_eq!(err.target, "Archve");
        // No suggestion expected for "Archve" → "Archive" with substring match,
        // but it should at least not panic and message() should be helpful.
        let msg = err.message(&folders);
        assert!(msg.contains("Archve"));
        assert!(msg.contains("Available folders"));
    }

    #[test]
    fn validate_rejects_unknown_folder_with_substring_suggestions() {
        let folders = vec![
            "Trash".to_string(),
            "Groups".to_string(),
            "Groups/RASC".to_string(),
        ];
        let rule = r#"if true { fileinto "groups"; }"#;
        // case-insensitive exact for "groups" → "Groups" should auto-correct,
        // not error.
        let corrected = validate_fileinto_targets(rule, &folders).unwrap();
        assert!(corrected.contains("\"Groups\""));
    }

    #[test]
    fn validate_suggests_substring_neighbours() {
        let folders = vec!["Trash".to_string(), "Groups/RASC".to_string()];
        let rule = r#"if true { fileinto "rasc"; }"#;
        let err = validate_fileinto_targets(rule, &folders).unwrap_err();
        assert_eq!(err.target, "rasc");
        assert!(
            err.suggestions.iter().any(|s| s == "Groups/RASC"),
            "expected Groups/RASC in suggestions, got {:?}",
            err.suggestions
        );
    }

    #[test]
    fn validate_handles_multiple_targets_one_bad() {
        let folders = vec!["Trash".to_string(), "Archive".to_string()];
        let rule = r#"if header :is "from" "a" { fileinto "Trash"; }
if header :is "from" "b" { fileinto "Bogus"; }"#;
        let err = validate_fileinto_targets(rule, &folders).unwrap_err();
        assert_eq!(err.target, "Bogus");
    }
}
