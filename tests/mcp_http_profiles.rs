//! Tests for per-profile MCP HTTP listeners.
//!
//! Regression target: rockycc and spamguard both need to talk to the
//! same dan@hld.ca daemon but with *different* permission matrices.
//! Before profiles, a single `--bearer-token-file` forced everyone into
//! the top-level `[permissions]` block — which caused spamguard to
//! silently fail to move scored messages because rockycc's read-only
//! posture denied `move_email`.
//!
//! These tests spin up a real daemon with a default listener (read-only)
//! plus one profile (read_write), then verify permission isolation and
//! cross-token rejection end-to-end.

use std::io::Write;
use tempfile::NamedTempFile;

const MCP_INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

const MCP_GET_PERMISSIONS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_permissions","arguments":{}}}"#;

/// Write a test config with a default `[permissions]` block that grants
/// email=read only, plus one `mcp_profiles` entry that grants full
/// email=read_write. Credentials are dummy files since we never hit
/// forwardemail.
fn write_profile_config(
    repo_dir: &std::path::Path,
    default_port: u16,
    profile_port: u16,
    profile_token_path: &std::path::Path,
) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    let user_file = repo_dir.join("user.txt");
    let pass_file = repo_dir.join("pass.txt");
    std::fs::write(&user_file, "test_alias@example.com").unwrap();
    std::fs::write(&pass_file, "fake-password").unwrap();

    // Two ports are baked into the config — the default_port is the one
    // passed on the CLI `--port`, and profile_port is what this
    // McpProfile binds for spamguard-style callers.
    let _ = default_port;

    write!(
        f,
        r#"
log_level = "error"

[forwardemail]
api_base = "https://api.forwardemail.net"
alias_user_file = "{user}"
alias_password_file = "{pass}"

[storage]
repo_path = "{repo}"

# Default listener — read-only, mirrors the rockycc-facing profile.
[permissions]
email_send = "denied"

[permissions.email]
default = "read"

[permissions.email.folders]
Drafts = "read_write"

# Profile listener — full mailbox write for the spam filter.
[[mcp_profiles]]
name = "spamguard"
port = {profile_port}
bearer_token_file = "{token}"
caller = "spamguard"

[mcp_profiles.permissions]
email_send = "denied"

[mcp_profiles.permissions.email]
default = "read_write"
"#,
        user = user_file.display(),
        pass = pass_file.display(),
        repo = repo_dir.display(),
        profile_port = profile_port,
        token = profile_token_path.display(),
    )
    .unwrap();
    f
}

/// Allocate a free TCP port by binding :0 and immediately releasing.
/// Racy in theory but fine for tests — tokio will re-grab it
/// microseconds later.
async fn pick_free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Start the daemon and wait until both HTTP listeners respond to POSTs.
async fn start_daemon_with_profile(
    config_path: &std::path::Path,
    default_port: u16,
    profile_port: u16,
    default_token_file: &std::path::Path,
) -> (String, String, tokio::process::Child) {
    let pimsteward = env!("CARGO_BIN_EXE_pimsteward");
    let child = tokio::process::Command::new(pimsteward)
        .arg("--config")
        .arg(config_path)
        .arg("daemon")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(default_port.to_string())
        .arg("--bearer-token-file")
        .arg(default_token_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let default_url = format!("http://127.0.0.1:{default_port}/mcp");
    let profile_url = format!("http://127.0.0.1:{profile_port}/mcp");

    // Wait for BOTH listeners to come up (up to 5s). Profiles spawn
    // after the default listener, so we poll the profile one too.
    let client = reqwest::Client::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let a = client.post(&default_url).body("{}").send().await.is_ok();
        let b = client.post(&profile_url).body("{}").send().await.is_ok();
        if a && b {
            return (default_url, profile_url, child);
        }
    }
    panic!("daemon did not bring up both listeners in time");
}

/// Extract a JSON body from an MCP SSE response. Strips `data: ` prefix
/// and parses the first JSON-RPC response line.
fn parse_sse(body: &str) -> serde_json::Value {
    for line in body.lines() {
        if let Some(json) = line.strip_prefix("data: ") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
                if v.get("id").is_some() {
                    return v;
                }
            }
        }
    }
    // Fall back to plain JSON
    serde_json::from_str(body).unwrap_or_else(|_| panic!("no JSON-RPC response in: {body}"))
}

/// Run a full initialize → tools/call(get_permissions) cycle against
/// `url` with the given bearer token. Returns the `result` field.
///
/// The daemon runs MCP in **stateless** mode (see daemon.rs's
/// `with_stateful_mode(false)` rationale), so there's no `Mcp-Session-Id`
/// header to track. Each HTTP POST is a self-contained JSON-RPC call.
async fn get_permissions(url: &str, token: &str) -> serde_json::Value {
    let client = reqwest::Client::new();

    // Initialize. Stateless mode returns plain JSON, no session header.
    let init = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(init.status(), 200, "initialize should succeed");
    let _ = init.text().await.unwrap();

    // tools/call get_permissions — also stateless; no session id needed.
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .body(MCP_GET_PERMISSIONS)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    parse_sse(&body)
}

#[tokio::test]
async fn profile_isolates_permissions_from_default_listener() {
    // Given: a daemon with a read-only default listener and a
    // read_write spamguard profile listener on a different port.
    let repo_dir = tempfile::tempdir().unwrap();
    let default_port = pick_free_port().await;
    let profile_port = pick_free_port().await;

    let mut default_token = NamedTempFile::new().unwrap();
    write!(default_token, "rockycc-token-aaa").unwrap();
    let mut profile_token = NamedTempFile::new().unwrap();
    write!(profile_token, "spamguard-token-bbb").unwrap();

    let config =
        write_profile_config(repo_dir.path(), default_port, profile_port, profile_token.path());
    let (default_url, profile_url, mut child) = start_daemon_with_profile(
        config.path(),
        default_port,
        profile_port,
        default_token.path(),
    )
    .await;

    // When: we call get_permissions on the default listener.
    // Then: we see the read-only matrix.
    let default_perms = get_permissions(&default_url, "rockycc-token-aaa").await;
    let default_email = &default_perms["result"]["content"][0]["text"];
    let default_text = default_email.as_str().unwrap_or("");
    assert!(
        default_text.contains("\"default\": \"read\""),
        "default listener must report email.default=read, got: {default_text}"
    );

    // When: we call get_permissions on the profile listener.
    // Then: we see the read_write matrix — different permissions on
    // the same alias, same daemon, different port and token.
    let profile_perms = get_permissions(&profile_url, "spamguard-token-bbb").await;
    let profile_email = &profile_perms["result"]["content"][0]["text"];
    let profile_text = profile_email.as_str().unwrap_or("");
    assert!(
        profile_text.contains("\"default\": \"read_write\""),
        "spamguard profile must report email.default=read_write, got: {profile_text}"
    );

    child.kill().await.ok();
}

#[tokio::test]
async fn profile_rejects_wrong_token_and_default_token() {
    // Given: the same two-listener daemon.
    let repo_dir = tempfile::tempdir().unwrap();
    let default_port = pick_free_port().await;
    let profile_port = pick_free_port().await;

    let mut default_token = NamedTempFile::new().unwrap();
    write!(default_token, "default-tok").unwrap();
    let mut profile_token = NamedTempFile::new().unwrap();
    write!(profile_token, "profile-tok").unwrap();

    let config =
        write_profile_config(repo_dir.path(), default_port, profile_port, profile_token.path());
    let (default_url, profile_url, mut child) = start_daemon_with_profile(
        config.path(),
        default_port,
        profile_port,
        default_token.path(),
    )
    .await;

    let client = reqwest::Client::new();

    // The profile listener must reject the default listener's token —
    // otherwise a compromised rockycc could escalate to spamguard's
    // permissions by pointing at the profile port.
    let resp = client
        .post(&profile_url)
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer default-tok")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "profile listener must not accept the default listener's token",
    );

    // And symmetrically the default listener must reject the profile
    // token — same attack in reverse.
    let resp = client
        .post(&default_url)
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer profile-tok")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        401,
        "default listener must not accept the profile listener's token",
    );

    child.kill().await.ok();
}

#[tokio::test]
async fn missing_profile_token_file_fails_daemon_startup() {
    // Profile with a bearer_token_file that doesn't exist. The daemon
    // must refuse to start rather than silently skipping the profile —
    // a missing token file could leave callers believing their ACL is
    // enforced when it is not.
    let repo_dir = tempfile::tempdir().unwrap();
    let default_port = pick_free_port().await;
    let profile_port = pick_free_port().await;

    let missing = repo_dir.path().join("does-not-exist");
    let mut default_token = NamedTempFile::new().unwrap();
    write!(default_token, "default-tok").unwrap();

    let config = write_profile_config(repo_dir.path(), default_port, profile_port, &missing);

    let pimsteward = env!("CARGO_BIN_EXE_pimsteward");
    let out = tokio::process::Command::new(pimsteward)
        .arg("--config")
        .arg(config.path())
        .arg("daemon")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(default_port.to_string())
        .arg("--bearer-token-file")
        .arg(default_token.path())
        .output()
        .await
        .unwrap();

    assert!(
        !out.status.success(),
        "daemon must fail when profile token file is missing"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bearer token file")
            || stderr.contains("does-not-exist")
            || stderr.contains("No such file"),
        "error should mention the missing token file, got: {stderr}"
    );
}
