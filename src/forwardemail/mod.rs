//! forwardemail.net REST client.
//!
//! Everything in this module is a typed wrapper around the HTTP API. No
//! business logic — that lives in `pull`, `write`, and `restore`. The shape
//! of the types is driven by the live behaviour documented in
//! `docs/api-findings.md`.
//!
//! Auth: HTTP Basic, alias email + generated alias password. Credentials
//! come from [`crate::Config::load_credentials`], never hardcoded.
//!
//! Rate limiting: responses include `X-RateLimit-Remaining` which this
//! client parses and surfaces via [`Client::rate_limit_remaining`] so a
//! calling loop can back off when it's running low.

pub mod client;
pub mod contacts;
pub mod sieve;

// Shared error helpers / response wrapping lives on Client itself.
pub use client::Client;
