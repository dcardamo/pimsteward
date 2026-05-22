//! Host-side ICS feed builder.
//!
//! Fetches the Dan calendar *windowless* over pimsteward's MCP (Streamable
//! HTTP), builds a subscribable feed via [`pimsteward_ical::build_feed`],
//! and writes it to disk only when the content changed.
//!
//! Windowless is deliberate: a windowed `list_events` expands recurring
//! events and would duplicate the master iCal. Windowless returns each
//! stored event once with its master `ical` (RRULE intact); history
//! trimming happens in `build_feed` via the `ICS_HISTORY_DAYS` cutoff.
//!
//! SECURITY: this crate depends only on `pimsteward-ical`, never on the
//! `pimsteward` crate — that compiler-enforced isolation keeps email
//! credentials and the MCP server surface out of the host binary.

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use pimsteward_ical::{build_feed, CalendarEvent};
use sha2::{Digest, Sha256};
use std::path::Path;

fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Write `contents` to `path` atomically (temp file in the SAME dir, then
/// rename), but only if it differs from the current file. Returns true
/// when it actually wrote.
///
/// The temp file is created in the destination directory so the final
/// `rename(2)` stays within one filesystem and is therefore atomic — a
/// reader either sees the old complete file or the new complete file,
/// never a half-written one.
pub fn write_if_changed(path: &Path, contents: &str) -> Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if sha(&existing) == sha(contents) {
            return Ok(false);
        }
    }
    let dir = path.parent().context("output path has no parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create temp file in {}", dir.display()))?;
    use std::io::Write;
    tmp.write_all(contents.as_bytes())?;
    tmp.flush()?;
    // Atomic rename within the same directory.
    tmp.persist(path)
        .with_context(|| format!("persist temp file to {}", path.display()))?;
    Ok(true)
}

struct Config {
    mcp_url: String,
    bearer_file: String,
    calendar_id: String,
    output: String,
    history_days: i64,
    cal_name: String,
}

fn config_from_env() -> Result<Config> {
    let get = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("missing env {k}"));
    Ok(Config {
        mcp_url: get("ICS_MCP_URL")?,
        bearer_file: get("ICS_BEARER_FILE")?,
        calendar_id: get("ICS_CALENDAR_ID")?,
        output: get("ICS_OUTPUT")?,
        history_days: std::env::var("ICS_HISTORY_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(365),
        cal_name: std::env::var("ICS_CAL_NAME").unwrap_or_else(|_| "Dan".into()),
    })
}

/// Fetch events for one calendar, windowless, with raw iCal, over MCP.
///
/// rmcp 1.3.0 client glue: the reqwest-backed Streamable HTTP transport is
/// built via `StreamableHttpClientTransport::from_config`, the client is an
/// empty handler `().serve(transport)`, and the tool result's text content
/// is reached through `content[..].raw.as_text()`.
async fn fetch_events(cfg: &Config, bearer: &str) -> Result<Vec<CalendarEvent>> {
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };
    use rmcp::ServiceExt;

    // `auth_header` wants the bare token WITHOUT the "Bearer " prefix —
    // the transport adds the scheme itself.
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(cfg.mcp_url.clone()).auth_header(bearer),
    );
    let client = ().serve(transport).await.context("mcp serve/initialize")?;

    let arguments = serde_json::json!({
        "calendar_id": cfg.calendar_id,
        "include_ical": true,
        "include_cancelled": false
    })
    .as_object()
    .cloned()
    .context("build list_events arguments object")?;

    let result = client
        .call_tool(CallToolRequestParams::new("list_events").with_arguments(arguments))
        .await
        .context("call_tool list_events")?;

    let text = result
        .content
        .iter()
        .find_map(|c| c.raw.as_text().map(|t| t.text.clone()))
        .context("no text content in list_events result")?;

    let events: Vec<CalendarEvent> =
        serde_json::from_str(&text).context("parse list_events JSON")?;

    // Best-effort graceful shutdown of the MCP session.
    let _ = client.cancel().await;
    Ok(events)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cfg = config_from_env()?;
    let bearer = std::fs::read_to_string(&cfg.bearer_file)
        .with_context(|| format!("read bearer {}", cfg.bearer_file))?
        .trim()
        .to_string();

    let events = fetch_events(&cfg, &bearer).await?;
    let cutoff = Utc::now() - Duration::days(cfg.history_days);
    let prodid = "-//hld.ca//ics-feedbuilder//EN";
    let feed = build_feed(&events, cutoff, &cfg.cal_name, prodid);
    let wrote = write_if_changed(Path::new(&cfg.output), &feed)?;
    tracing::info!(
        events = events.len(),
        wrote,
        output = %cfg.output,
        "ics feed built"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_if_changed;

    #[test]
    fn writes_then_skips_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dan.ics");
        assert!(write_if_changed(&path, "A").unwrap(), "first write");
        let m1 = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(
            !write_if_changed(&path, "A").unwrap(),
            "no rewrite when same"
        );
        let m2 = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(m1, m2, "mtime stable when unchanged");
        assert!(write_if_changed(&path, "B").unwrap(), "rewrite on change");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "B");
    }
}
