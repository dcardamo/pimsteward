//! Thin typed wrapper around forwardemail.net's REST API.
//!
//! The client holds credentials privately and exposes one method per endpoint
//! we care about. Rate-limit headers are parsed and surfaced via
//! [`Client::rate_limit_remaining`] so pull loops can back off.
//!
//! v1 only needs the read-side surface (list/get) to support the pull loop.
//! The mutation side (POST/PUT/DELETE) lands with the write tools; this
//! keeps the initial binary lean and the dead-code warnings honest.

use crate::error::Error;
use reqwest::{Method, Response};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    api_base: String,
    alias_user: String,
    alias_password: String,
    rate_remaining: Arc<AtomicI64>,
}

impl Client {
    /// Build a client. Uses rustls and a pimsteward-branded User-Agent.
    pub fn new(
        api_base: impl Into<String>,
        alias_user: impl Into<String>,
        alias_password: impl Into<String>,
    ) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("pimsteward/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            api_base: api_base.into(),
            alias_user: alias_user.into(),
            alias_password: alias_password.into(),
            rate_remaining: Arc::new(AtomicI64::new(-1)),
        })
    }

    /// Most recently observed `X-RateLimit-Remaining`. Returns -1 if no
    /// request has been made yet.
    pub fn rate_limit_remaining(&self) -> i64 {
        self.rate_remaining.load(Ordering::Relaxed)
    }

    /// Cheap keepalive / auth probe.
    pub async fn account(&self) -> Result<serde_json::Value, Error> {
        self.get_json("/v1/account").await
    }

    /// GET + decode JSON. Internal helper used by per-resource modules.
    pub(crate) async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        let bytes = self.send(Method::GET, path).await?;
        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    /// Same as `get_json` but returns a [`serde_json::Value`] — used by the
    /// MCP `search_email` tool which passes through arbitrary query strings
    /// and doesn't benefit from typed deserialization.
    pub async fn raw_get_json(&self, path: &str) -> Result<serde_json::Value, Error> {
        self.get_json(path).await
    }

    /// Send a request, consume the response, and return the body bytes on
    /// success. Non-2xx responses are converted to [`Error::Api`] with the
    /// full response body included for debugging.
    async fn send(&self, method: Method, path: &str) -> Result<Vec<u8>, Error> {
        self.backoff_if_throttled().await;
        let url = format!("{}{}", self.api_base, path);
        let resp = self
            .http
            .request(method, &url)
            .basic_auth(&self.alias_user, Some(&self.alias_password))
            .send()
            .await?;
        self.capture_rate_limit(&resp);
        self.consume_and_check(resp).await
    }

    /// POST JSON body, decode a typed response.
    pub(crate) async fn post_json<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp, Error>
    where
        Req: serde::Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let bytes = self.send_json(Method::POST, path, body, None).await?;
        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    /// PUT JSON body with optional `If-Match` header, decode a typed response.
    pub(crate) async fn put_json<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        if_match: Option<&str>,
    ) -> Result<Resp, Error>
    where
        Req: serde::Serialize + ?Sized,
        Resp: DeserializeOwned,
    {
        let bytes = self.send_json(Method::PUT, path, body, if_match).await?;
        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    /// DELETE a path. Ignores the response body on success.
    pub(crate) async fn delete_path(&self, path: &str) -> Result<(), Error> {
        let _ = self.send(Method::DELETE, path).await?;
        Ok(())
    }

    async fn send_json<Req>(
        &self,
        method: Method,
        path: &str,
        body: &Req,
        if_match: Option<&str>,
    ) -> Result<Vec<u8>, Error>
    where
        Req: serde::Serialize + ?Sized,
    {
        let url = format!("{}{}", self.api_base, path);
        let mut req = self
            .http
            .request(method, &url)
            .basic_auth(&self.alias_user, Some(&self.alias_password))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(body);
        if let Some(etag) = if_match {
            req = req.header("If-Match", etag);
        }
        self.backoff_if_throttled().await;
        let resp = req.send().await?;
        self.capture_rate_limit(&resp);
        self.consume_and_check(resp).await
    }

    /// If the most recently observed `X-RateLimit-Remaining` is low, sleep
    /// before issuing the next request. Tiered thresholds keep pull loops
    /// polite without serializing every call. -1 (unknown) is a no-op so the
    /// first request proceeds immediately.
    async fn backoff_if_throttled(&self) {
        let remaining = self.rate_remaining.load(Ordering::Relaxed);
        if let Some(delay) = backoff_for_remaining(remaining) {
            tracing::warn!(
                remaining,
                delay_ms = delay.as_millis() as u64,
                "forwardemail rate limit low, sleeping before next request"
            );
            tokio::time::sleep(delay).await;
        }
    }

    fn capture_rate_limit(&self, resp: &Response) {
        if let Some(v) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
        {
            self.rate_remaining.store(v, Ordering::Relaxed);
        }
    }

    /// Consume a response, returning the body bytes on success. Non-2xx
    /// responses become [`Error::Api`] with the response body included
    /// (truncated to 500 chars) for actionable error messages from
    /// forwardemail (e.g. "alias not found", "rate limit exceeded").
    async fn consume_and_check(&self, resp: Response) -> Result<Vec<u8>, Error> {
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::store(format!("reading response body: {e}")))?;
        if status.is_success() {
            return Ok(bytes.to_vec());
        }
        let body_str: String = String::from_utf8_lossy(&bytes).chars().take(500).collect();
        Err(Error::Api {
            status: status.as_u16(),
            message: if body_str.is_empty() {
                status
                    .canonical_reason()
                    .unwrap_or("unknown")
                    .to_string()
            } else {
                body_str
            },
        })
    }
}

/// Pure helper: given a most-recently-observed `X-RateLimit-Remaining` value,
/// return the duration to sleep before the next request. `None` means no
/// backoff. -1 signals "unknown" (no request made yet) and is also a no-op.
///
/// Tiers:
/// - 0 or less → 30s (fully exhausted; give the bucket time to refill)
/// - 1..=9   → 10s
/// - 10..=49 → 2s
/// - 50..=99 → 500ms
/// - ≥100     → no backoff
fn backoff_for_remaining(remaining: i64) -> Option<Duration> {
    if remaining < 0 {
        return None;
    }
    if remaining == 0 {
        return Some(Duration::from_secs(30));
    }
    if remaining < 10 {
        return Some(Duration::from_secs(10));
    }
    if remaining < 50 {
        return Some(Duration::from_secs(2));
    }
    if remaining < 100 {
        return Some(Duration::from_millis(500));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_unknown_is_noop() {
        assert_eq!(backoff_for_remaining(-1), None);
    }

    #[test]
    fn backoff_plenty_is_noop() {
        assert_eq!(backoff_for_remaining(100), None);
        assert_eq!(backoff_for_remaining(500), None);
        assert_eq!(backoff_for_remaining(i64::MAX), None);
    }

    #[test]
    fn backoff_tiered_thresholds() {
        assert_eq!(backoff_for_remaining(99), Some(Duration::from_millis(500)));
        assert_eq!(backoff_for_remaining(50), Some(Duration::from_millis(500)));
        assert_eq!(backoff_for_remaining(49), Some(Duration::from_secs(2)));
        assert_eq!(backoff_for_remaining(10), Some(Duration::from_secs(2)));
        assert_eq!(backoff_for_remaining(9), Some(Duration::from_secs(10)));
        assert_eq!(backoff_for_remaining(1), Some(Duration::from_secs(10)));
        assert_eq!(backoff_for_remaining(0), Some(Duration::from_secs(30)));
    }
}
