//! Generic SMTP-submission sender.
//!
//! Providers that speak standard SMTP submission (currently Stalwart) have
//! no forwardemail REST surface, so outbound mail can't go through
//! [`crate::forwardemail::Client::send_email`]. This module sends a
//! pre-built RFC822 message over a STARTTLS submission connection (port
//! 587) with AUTH PLAIN, using the maintained [`mail_send`] async SMTP
//! client.
//!
//! Message construction is shared with the REST path: the raw bytes come
//! from [`crate::forwardemail::writes::build_plain_rfc822`] (plain/threaded
//! mail) and [`crate::forwardemail::writes::build_imip_mime`] (calendar
//! iMIP), so headers, threading, and MIME structure are identical no matter
//! which provider sends.
//!
//! TLS validation is normal (the server presents a valid certificate for
//! its hostname) — no `allow_invalid_certs`, and the configured host is
//! passed as the TLS/SNI name. STARTTLS is required: `implicit_tls(false)`
//! makes the client upgrade the plaintext :587 connection and error out if
//! the server doesn't advertise STARTTLS, never falling back to cleartext.

use std::time::Duration;

use async_trait::async_trait;
use mail_send::smtp::message::Message as SmtpMessage;
use mail_send::{Credentials, SmtpClientBuilder};

use crate::error::Error;
use crate::forwardemail::writes::{build_imip_mime, build_plain_rfc822, NewMessage};
use crate::provider::stalwart::StalwartEndpoint;
use crate::scheduling::Sender;

const SMTP_TIMEOUT: Duration = Duration::from_secs(60);

/// An SMTP-submission sender bound to one endpoint + credentials. Cheap to
/// clone (just config); each `send` opens a fresh authenticated connection.
#[derive(Debug, Clone)]
pub struct SmtpSender {
    endpoint: StalwartEndpoint,
    /// The header `From:` / envelope MAIL FROM address — the authenticated
    /// alias. Kept separate from `endpoint.user` only for clarity; for
    /// Stalwart they're the same address.
    from: String,
}

impl SmtpSender {
    /// Build a sender from a provider's SMTP endpoint. The endpoint's
    /// `user` doubles as the From address (the alias we authenticate as).
    pub fn new(endpoint: StalwartEndpoint) -> Self {
        let from = endpoint.user.clone();
        Self { endpoint, from }
    }

    /// The From / envelope-sender address.
    pub fn from_address(&self) -> &str {
        &self.from
    }

    /// Open an authenticated STARTTLS submission connection and send one
    /// raw RFC822 message with the given envelope. Returns once the server
    /// has accepted the message for delivery (2xx after DATA); any non-2xx
    /// or transport failure surfaces as an [`Error`].
    pub async fn send_raw(
        &self,
        from: &str,
        recipients: &[String],
        raw: &[u8],
    ) -> Result<(), Error> {
        if recipients.is_empty() {
            return Err(Error::config("SMTP send: no recipients"));
        }

        // STARTTLS on the submission port: start plaintext, upgrade via
        // STARTTLS, then AUTH PLAIN. `implicit_tls(false)` errors out if the
        // server doesn't offer STARTTLS rather than sending in the clear.
        let builder = SmtpClientBuilder::new(self.endpoint.host.clone(), self.endpoint.port)
            .map_err(|e| Error::config(format!("SMTP TLS connector init: {e}")))?
            .implicit_tls(false)
            .timeout(SMTP_TIMEOUT)
            .credentials(Credentials::Plain {
                username: self.endpoint.user.clone(),
                secret: self.endpoint.password.clone(),
            });

        let mut client = builder
            .connect()
            .await
            .map_err(|e| smtp_err("connect/auth", &e))?;

        let message = SmtpMessage::new(from.to_string(), recipients.to_vec(), raw.to_vec());
        client
            .send(message)
            .await
            .map_err(|e| smtp_err("send", &e))?;
        Ok(())
    }

    /// Send a structured [`NewMessage`] over SMTP submission. Recipients are
    /// the union of To/Cc/Bcc (envelope-level — Bcc isn't echoed into the
    /// header block by [`build_plain_rfc822`]). Returns a small JSON value
    /// shaped like the REST `send_email` result so the daemon's audit/log
    /// path can read an `id` field uniformly across providers.
    pub async fn send_message(&self, msg: &NewMessage) -> Result<serde_json::Value, Error> {
        let raw = build_plain_rfc822(&self.from, msg);
        let mut recipients: Vec<String> = Vec::new();
        recipients.extend(msg.to.iter().cloned());
        recipients.extend(msg.cc.iter().cloned());
        recipients.extend(msg.bcc.iter().cloned());
        self.send_raw(&self.from, &recipients, raw.as_bytes()).await?;
        // SMTP submission returns no message id; synthesize a stable marker
        // so callers logging `result["id"]` don't print "unknown".
        Ok(serde_json::json!({ "id": "smtp-submitted", "transport": "smtp" }))
    }
}

#[async_trait]
impl Sender for SmtpSender {
    async fn send_imip(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        payload: &str,
        method: &str,
    ) -> Result<String, Error> {
        let raw = build_imip_mime(&self.from, to, subject, text_body, payload, method);
        self.send_raw(&self.from, &[to.to_string()], raw.as_bytes())
            .await?;
        // No server-assigned id over SMTP submission.
        Ok("smtp-submitted".to_string())
    }

    async fn notify(&self, subject: &str, body: &str) -> Result<(), Error> {
        // Notify the alias owner — the From address (self).
        let msg = NewMessage {
            folder: String::new(),
            to: vec![self.from.clone()],
            cc: vec![],
            bcc: vec![],
            subject: subject.to_string(),
            text: Some(body.to_string()),
            html: None,
            in_reply_to: None,
            references: vec![],
        };
        self.send_message(&msg).await?;
        Ok(())
    }
}

/// Map a `mail_send::Error` into our `Error::Api`, walking its source chain
/// (TLS/IO causes) the same way [`crate::error::fmt_error_chain`] does for
/// reqwest. Never logs credentials — `mail_send::Error`'s `Display` carries
/// only protocol/transport text.
fn smtp_err(stage: &str, e: &mail_send::Error) -> Error {
    let chain = crate::error::fmt_error_chain(e);
    Error::Api {
        status: 502,
        message: format!("SMTP submission {stage} failed: {chain}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint() -> StalwartEndpoint {
        StalwartEndpoint {
            host: "stalwart.example.test".to_string(),
            port: 587,
            user: "dan@example.test".to_string(),
            password: "secret".to_string(),
        }
    }

    #[test]
    fn sender_uses_user_as_from() {
        let s = SmtpSender::new(endpoint());
        assert_eq!(s.from_address(), "dan@example.test");
    }

    #[tokio::test]
    async fn send_raw_rejects_empty_recipients() {
        let s = SmtpSender::new(endpoint());
        let err = s.send_raw("dan@example.test", &[], b"x").await.unwrap_err();
        assert!(err.to_string().contains("no recipients"), "{err}");
    }

    /// LIVE submission against the real Stalwart server. Gated on
    /// `STALWART_LIVE_SEND=1`; reads the alias password from
    /// `~/.config/secrets/stalwart-password` (or `STALWART_LIVE_PASSWORD`).
    /// Sends dan@→dan@ (external destination → Stalwart relays out via
    /// Forward Email). Asserts the SMTP transaction is accepted (2xx after
    /// DATA — `send_raw`/`send` only return Ok on a positive completion).
    #[tokio::test]
    async fn live_send_relays_via_forward_email() {
        if std::env::var("STALWART_LIVE_SEND").as_deref() != Ok("1") {
            eprintln!("skipping: set STALWART_LIVE_SEND=1 to run the live submission test");
            return;
        }
        let host =
            std::env::var("STALWART_LIVE_HOST").unwrap_or_else(|_| "stalwart.example.test".into());
        let user = std::env::var("STALWART_LIVE_USER").unwrap_or_else(|_| "dan@example.test".into());
        let password = std::env::var("STALWART_LIVE_PASSWORD").ok().unwrap_or_else(|| {
            let home = std::env::var("HOME").expect("HOME");
            std::fs::read_to_string(format!("{home}/.config/secrets/stalwart-password"))
                .expect("read stalwart-password")
                .trim()
                .to_string()
        });

        let sender = SmtpSender::new(StalwartEndpoint {
            host,
            port: 587,
            user: user.clone(),
            password,
        });

        let stamp = chrono::Utc::now().to_rfc3339();
        let msg = NewMessage {
            folder: String::new(),
            to: vec![user.clone()],
            cc: vec![],
            bcc: vec![],
            subject: format!("pimsteward STALWART_LIVE_SEND {stamp}"),
            text: Some(format!(
                "Live SMTP-submission test from pimsteward at {stamp}. \
                 External destination → Stalwart relays out via Forward Email."
            )),
            html: None,
            in_reply_to: None,
            references: vec![],
        };
        let res = sender.send_message(&msg).await.expect("live SMTP send must succeed (250 queued)");
        assert_eq!(res["transport"], "smtp");
        eprintln!("live send accepted: {res}");
    }
}
