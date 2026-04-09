//! Tests for pimsteward daemon's MCP HTTP bearer token authentication.
//!
//! Spins up the daemon with `--port` and optional `--bearer-token-file`
//! and verifies that requests are accepted/rejected based on the
//! Authorization header. Uses a temp config so no real credentials are
//! needed.

use std::io::Write;
use tempfile::NamedTempFile;

const MCP_INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

/// Write a minimal pimsteward config pointing at a fake API and a temp repo.
fn write_test_config(api_base: &str, repo_dir: &std::path::Path) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    // Credentials — dummy files that the factory reads at session creation.
    let user_file = repo_dir.join("user.txt");
    let pass_file = repo_dir.join("pass.txt");
    std::fs::write(&user_file, "test_alias@example.com").unwrap();
    std::fs::write(&pass_file, "fake-password").unwrap();

    write!(
        f,
        r#"
log_level = "error"

[forwardemail]
api_base = "{api_base}"
alias_user_file = "{}"
alias_password_file = "{}"

[storage]
repo_path = "{}"

[permissions]
email = "read"
calendar = "none"
contacts = "none"
sieve = "none"
"#,
        user_file.display(),
        pass_file.display(),
        repo_dir.display(),
    )
    .unwrap();
    f
}

/// Start pimsteward daemon with --port on a random port and optional
/// bearer token. Returns the base URL and a handle to kill the process.
async fn start_server(
    config_path: &std::path::Path,
    bearer_token_file: Option<&std::path::Path>,
) -> (String, tokio::process::Child) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Drop the listener so pimsteward can bind the same port.
    drop(listener);

    let pimsteward = env!("CARGO_BIN_EXE_pimsteward");
    let mut cmd = tokio::process::Command::new(pimsteward);
    cmd.arg("--config")
        .arg(config_path)
        .arg("daemon")
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Some(path) = bearer_token_file {
        cmd.arg("--bearer-token-file").arg(path);
    }

    let child = cmd.spawn().unwrap();
    let url = format!("http://127.0.0.1:{port}/mcp");

    // Wait for the server to be ready (up to 3 seconds).
    let client = reqwest::Client::new();
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if client.post(&url).body("{}").send().await.is_ok() {
            return (url, child);
        }
    }
    panic!("daemon mcp-http server didn't start in time");
}

#[tokio::test]
async fn auth_rejects_missing_token() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config = write_test_config("https://api.forwardemail.net", repo_dir.path());

    let mut token_file = NamedTempFile::new().unwrap();
    write!(token_file, "test-secret-token-abc123").unwrap();

    let (url, mut child) = start_server(config.path(), Some(token_file.path())).await;
    let client = reqwest::Client::new();

    // No auth header → 401
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "missing token should get 401");

    child.kill().await.ok();
}

#[tokio::test]
async fn auth_rejects_wrong_token() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config = write_test_config("https://api.forwardemail.net", repo_dir.path());

    let mut token_file = NamedTempFile::new().unwrap();
    write!(token_file, "correct-token").unwrap();

    let (url, mut child) = start_server(config.path(), Some(token_file.path())).await;
    let client = reqwest::Client::new();

    // Wrong token → 401
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer wrong-token")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "wrong token should get 401");

    child.kill().await.ok();
}

#[tokio::test]
async fn auth_accepts_correct_token() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config = write_test_config("https://api.forwardemail.net", repo_dir.path());

    let mut token_file = NamedTempFile::new().unwrap();
    writeln!(token_file, "correct-token").unwrap(); // trailing newline should be trimmed

    let (url, mut child) = start_server(config.path(), Some(token_file.path())).await;
    let client = reqwest::Client::new();

    // Correct token → 200 (MCP initialize response)
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", "Bearer correct-token")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "correct token should get 200");
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("serverInfo"),
        "response should contain MCP server info, got: {body}"
    );

    child.kill().await.ok();
}

#[tokio::test]
async fn no_auth_required_without_token_file() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config = write_test_config("https://api.forwardemail.net", repo_dir.path());

    let (url, mut child) = start_server(config.path(), None).await;
    let client = reqwest::Client::new();

    // No token file configured → requests work without auth
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "without token file, requests should succeed"
    );

    child.kill().await.ok();
}

#[tokio::test]
async fn auth_rejects_basic_auth_scheme() {
    let repo_dir = tempfile::tempdir().unwrap();
    let config = write_test_config("https://api.forwardemail.net", repo_dir.path());

    let mut token_file = NamedTempFile::new().unwrap();
    write!(token_file, "secret-token").unwrap();

    let (url, mut child) = start_server(config.path(), Some(token_file.path())).await;
    let client = reqwest::Client::new();

    // Basic auth scheme with the right value → still 401 (must be Bearer)
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", "Basic secret-token")
        .body(MCP_INIT)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "Basic scheme should be rejected");

    child.kill().await.ok();
}
