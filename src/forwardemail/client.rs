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
        let resp = self.send(Method::GET, path).await?;
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    /// Same as `get_json` but returns a [`serde_json::Value`] — used by the
    /// MCP `search_email` tool which passes through arbitrary query strings
    /// and doesn't benefit from typed deserialization.
    pub async fn raw_get_json(&self, path: &str) -> Result<serde_json::Value, Error> {
        self.get_json(path).await
    }

    async fn send(&self, method: Method, path: &str) -> Result<Response, Error> {
        let url = format!("{}{}", self.api_base, path);
        let resp = self
            .http
            .request(method, &url)
            .basic_auth(&self.alias_user, Some(&self.alias_password))
            .send()
            .await?;
        self.capture_rate_limit(&resp);
        self.check_status(&resp)?;
        Ok(resp)
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

    /// Convert non-2xx responses to [`Error::Api`]. The body is not read
    /// here (reqwest's `Response` doesn't let us peek without consuming);
    /// callers that need the body should decode the error separately.
    fn check_status(&self, resp: &Response) -> Result<(), Error> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        Err(Error::Api {
            status: status.as_u16(),
            message: status.canonical_reason().unwrap_or("unknown").to_string(),
        })
    }
}
