//! ManageSieve client (RFC 5804) for forwardemail.net.
//!
//! Forwardemail's REST API treats `is_active` as read-only — the API
//! silently ignores it on both POST and PUT. The only way to activate or
//! deactivate a sieve script is through the ManageSieve protocol.
//!
//! Port 4190 on `imap.forwardemail.net` accepts implicit TLS connections
//! and supports AUTHENTICATE PLAIN + SETACTIVE.
//!
//! ManageSieve allows exactly ONE active script at a time. SETACTIVE
//! with a script name activates it and deactivates any previously active
//! script. SETACTIVE with an empty name deactivates all scripts.

use crate::error::Error;
use base64::Engine;
use socket2::SockRef;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// A connected, authenticated ManageSieve session.
pub struct ManageSieveSession {
    reader: BufReader<tokio::io::ReadHalf<TlsStream<TcpStream>>>,
    writer: tokio::io::WriteHalf<TlsStream<TcpStream>>,
}

/// Result of LISTSCRIPTS — script name and whether it's active.
#[derive(Debug, Clone)]
pub struct SieveEntry {
    pub name: String,
    pub active: bool,
}

impl ManageSieveSession {
    /// Connect and authenticate to the ManageSieve server.
    pub async fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self, Error> {
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|e| Error::config(format!("ManageSieve TCP connect {host}:{port}: {e}")))?;

        let sock = SockRef::from(&tcp);
        let ka = socket2::TcpKeepalive::new().with_time(TCP_KEEPALIVE);
        #[cfg(target_os = "linux")]
        let ka = ka.with_interval(TCP_KEEPALIVE);
        sock.set_tcp_keepalive(&ka)
            .map_err(|e| Error::config(format!("ManageSieve TCP keepalive: {e}")))?;

        // Implicit TLS — forwardemail's port 4190 speaks TLS directly.
        let mut root_store = RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls_config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(std::sync::Arc::new(tls_config));
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| Error::config(format!("ManageSieve server name: {e}")))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| Error::config(format!("ManageSieve TLS: {e}")))?;

        let (rd, wr) = tokio::io::split(tls);
        let mut session = Self {
            reader: BufReader::new(rd),
            writer: wr,
        };

        // Read and discard the server banner (capabilities + OK).
        session.read_response().await?;

        // AUTHENTICATE PLAIN
        let auth_str = base64::engine::general_purpose::STANDARD.encode(
            format!("\0{user}\0{password}"),
        );
        session
            .send_command(&format!("AUTHENTICATE \"PLAIN\" \"{auth_str}\""))
            .await?;
        let auth_resp = session.read_response().await?;
        if !auth_resp.starts_with("OK") {
            return Err(Error::config(format!("ManageSieve auth failed: {auth_resp}")));
        }

        Ok(session)
    }

    /// List all scripts and their active state.
    pub async fn list_scripts(&mut self) -> Result<Vec<SieveEntry>, Error> {
        self.send_command("LISTSCRIPTS").await?;
        let resp = self.read_response().await?;

        let mut entries = Vec::new();
        for line in resp.lines() {
            let line = line.trim();
            // Script lines look like: "name" or "name" ACTIVE
            if let Some(rest) = line.strip_prefix('"') {
                if let Some(eq) = rest.find('"') {
                    let name = rest[..eq].to_string();
                    let active = rest[eq + 1..].contains("ACTIVE");
                    entries.push(SieveEntry { name, active });
                }
            }
        }
        Ok(entries)
    }

    /// Activate a script by name. Deactivates any previously active script.
    /// Pass an empty string to deactivate all scripts.
    pub async fn set_active(&mut self, name: &str) -> Result<(), Error> {
        self.send_command(&format!("SETACTIVE \"{name}\"")).await?;
        let resp = self.read_response().await?;
        if resp.starts_with("OK") {
            Ok(())
        } else {
            Err(Error::Api {
                status: 500,
                message: format!("ManageSieve SETACTIVE failed: {resp}"),
            })
        }
    }

    async fn send_command(&mut self, cmd: &str) -> Result<(), Error> {
        self.writer
            .write_all(format!("{cmd}\r\n").as_bytes())
            .await
            .map_err(|e| Error::config(format!("ManageSieve send: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| Error::config(format!("ManageSieve flush: {e}")))?;
        Ok(())
    }

    /// Read lines until we get an OK, NO, or BYE response.
    async fn read_response(&mut self) -> Result<String, Error> {
        let mut buf = String::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(
                Duration::from_secs(10),
                self.reader.read_line(&mut line),
            )
            .await
            .map_err(|_| Error::config("ManageSieve read timeout"))?
            .map_err(|e| Error::config(format!("ManageSieve read: {e}")))?;

            if n == 0 {
                return Err(Error::config("ManageSieve: connection closed"));
            }
            buf.push_str(&line);
            let trimmed = line.trim();
            if trimmed.starts_with("OK") || trimmed.starts_with("NO") || trimmed.starts_with("BYE")
            {
                break;
            }
        }
        Ok(buf)
    }
}

/// One-shot helper: connect, activate a script, disconnect. Used by the
/// MCP write path where we don't need a long-lived session.
pub async fn activate_script(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    script_name: &str,
) -> Result<(), Error> {
    let mut session = ManageSieveSession::connect(host, port, user, password).await?;
    session.set_active(script_name).await?;
    Ok(())
}

/// One-shot helper: connect, list scripts, disconnect. Returns the name
/// of the active script (if any) so callers can overlay the real active
/// state onto REST API results (which report is_active incorrectly).
pub async fn get_active_script(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
) -> Result<Option<String>, Error> {
    let mut session = ManageSieveSession::connect(host, port, user, password).await?;
    let scripts = session.list_scripts().await?;
    Ok(scripts.into_iter().find(|s| s.active).map(|s| s.name))
}
