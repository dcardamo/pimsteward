//! `SieveBackend` implementations.
//!
//! Two backends share the [`SieveBackend`] trait (defined in
//! [`crate::source::traits`]):
//!
//! - [`ForwardemailSieveBackend`] — REST `/v1/sieve-scripts` for CRUD +
//!   ManageSieve (port 4190, implicit TLS) for activation. The REST API's
//!   `is_active` field is read-only, so activation must go through
//!   ManageSieve.
//! - [`StalwartSieveBackend`] — full CRUD + activation over ManageSieve
//!   (RFC 5804, STARTTLS on port 4190). No REST surface; Stalwart speaks
//!   only the ManageSieve protocol for script management.
//!
//! Both implementations are script-name keyed (see the trait docs for
//! why). The FE backend preserves the REST `id` in
//! [`SieveScriptMeta::id`] for the audit-trail meta.json; the Stalwart
//! backend uses the script name as `id`.

use crate::error::Error;
use crate::forwardemail::managesieve;
use crate::forwardemail::Client;
use crate::forwardemail::sieve::SieveScript;
use crate::source::traits::{SieveBackend, SieveScriptMeta};
use async_trait::async_trait;

// ── Forward Email (REST + ManageSieve) ──────────────────────────────

/// FE-shaped sieve backend. Holds the REST `Client` for CRUD and the
/// ManageSieve endpoint config for activation (REST `is_active` is
/// read-only — see [`crate::forwardemail::writes`] doc comments).
#[derive(Clone)]
pub struct ForwardemailSieveBackend {
    client: Client,
    ms: crate::mcp::ManageSieveConfig,
}

impl ForwardemailSieveBackend {
    pub fn new(client: Client, ms: crate::mcp::ManageSieveConfig) -> Self {
        Self { client, ms }
    }
}

fn fe_script_to_meta(s: SieveScript, active_name: Option<&str>) -> SieveScriptMeta {
    SieveScriptMeta {
        id: s.id,
        name: s.name.clone(),
        content: s.content,
        is_active: active_name.map(|n| n == s.name).unwrap_or(s.is_active),
        is_valid: s.is_valid,
        validation_errors: s.validation_errors,
    }
}

#[async_trait]
impl SieveBackend for ForwardemailSieveBackend {
    fn tag(&self) -> &'static str {
        "rest"
    }

    async fn list_scripts(&self) -> Result<Vec<SieveScriptMeta>, Error> {
        let list = self.client.list_sieve_scripts().await?;
        let active_name =
            managesieve::get_active_script(&self.ms.host, self.ms.port, &self.ms.user, &self.ms.password)
                .await
                .ok().flatten();
        Ok(list
            .into_iter()
            .map(|s| fe_script_to_meta(s, active_name.as_deref()))
            .collect())
    }

    async fn get_script(&self, name: &str) -> Result<SieveScriptMeta, Error> {
        let list = self.client.list_sieve_scripts().await?;
        let active_name =
            managesieve::get_active_script(&self.ms.host, self.ms.port, &self.ms.user, &self.ms.password)
                .await
                .ok().flatten();
        let s = list.into_iter().find(|s| s.name == name).ok_or_else(|| Error::Api {
            status: 404,
            message: format!("no sieve script named '{name}'"),
        })?;
        let id = s.id.clone();
        let full = self.client.get_sieve_script(&id).await?;
        Ok(fe_script_to_meta(full, active_name.as_deref()))
    }

    async fn put_script(
        &self,
        name: &str,
        content: &str,
    ) -> Result<SieveScriptMeta, Error> {
        // Upsert by name: update if a script with this name exists, else create.
        let list = self.client.list_sieve_scripts().await?;
        let active_name =
            managesieve::get_active_script(&self.ms.host, self.ms.port, &self.ms.user, &self.ms.password)
                .await
                .ok().flatten();
        let updated = if let Some(existing) = list.iter().find(|s| s.name == name) {
            self.client.update_sieve_script(&existing.id, content).await?
        } else {
            self.client.create_sieve_script(name, content).await?
        };
        if !updated.is_valid {
            return Err(Error::Api {
                status: 422,
                message: format!(
                    "sieve script '{name}' accepted by forwardemail but flagged as invalid: {:?}",
                    updated.validation_errors
                ),
            });
        }
        Ok(fe_script_to_meta(updated, active_name.as_deref()))
    }

    async fn delete_script(&self, name: &str) -> Result<(), Error> {
        let list = self.client.list_sieve_scripts().await?;
        if let Some(existing) = list.into_iter().find(|s| s.name == name) {
            self.client.delete_sieve_script(&existing.id).await?;
        }
        Ok(())
    }

    async fn activate_script(&self, name: &str) -> Result<(), Error> {
        managesieve::activate_script(&self.ms.host, self.ms.port, &self.ms.user, &self.ms.password, name)
            .await
    }

    async fn get_active(&self) -> Result<Option<String>, Error> {
        managesieve::get_active_script(&self.ms.host, self.ms.port, &self.ms.user, &self.ms.password).await
    }
}

// ── Stalwart (ManageSieve, STARTTLS) ────────────────────────────────

/// Stalwart sieve backend. Full CRUD + activation over ManageSieve
/// (RFC 5804) with STARTTLS. Stalwart has no REST surface for sieve
/// scripts, so every operation goes through the ManageSieve session.
///
/// `ManageSieveSession` already speaks `LISTSCRIPTS` + `SETACTIVE`;
/// this backend adds `GETSCRIPT`, `PUTSCRIPT`, `DELETESCRIPT`, and the
/// STARTTLS handshake (Stalwart's managesieve listener expects STARTTLS,
/// not the implicit TLS the FE listener uses).
#[derive(Clone)]
pub struct StalwartSieveBackend {
    ms: crate::mcp::ManageSieveConfig,
}

impl StalwartSieveBackend {
    pub fn new(ms: crate::mcp::ManageSieveConfig) -> Self {
        Self { ms }
    }

    async fn connect(&self) -> Result<managesieve::ManageSieveSession, Error> {
        managesieve::ManageSieveSession::connect_starttls(
            &self.ms.host,
            self.ms.port,
            &self.ms.user,
            &self.ms.password,
        )
        .await
    }
}

#[async_trait]
impl SieveBackend for StalwartSieveBackend {
    fn tag(&self) -> &'static str {
        "managesieve"
    }

    async fn list_scripts(&self) -> Result<Vec<SieveScriptMeta>, Error> {
        let mut session = self.connect().await?;
        let entries = session.list_scripts().await?;
        Ok(entries
            .into_iter()
            .map(|e| SieveScriptMeta {
                id: e.name.clone(),
                name: e.name.clone(),
                content: None,
                is_active: e.active,
                is_valid: true,
                validation_errors: Vec::new(),
            })
            .collect())
    }

    async fn get_script(&self, name: &str) -> Result<SieveScriptMeta, Error> {
        let mut session = self.connect().await?;
        let content = session.get_script(name).await?;
        let active = session
            .list_scripts()
            .await?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.active)
            .unwrap_or(false);
        Ok(SieveScriptMeta {
            id: name.to_string(),
            name: name.to_string(),
            content: Some(content),
            is_active: active,
            is_valid: true,
            validation_errors: Vec::new(),
        })
    }

    async fn put_script(
        &self,
        name: &str,
        content: &str,
    ) -> Result<SieveScriptMeta, Error> {
        let mut session = self.connect().await?;
        session.put_script(name, content).await?;
        // Stalwart's PUTSCRIPT accepts bytes without server-side parse,
        // so is_valid is true on a 200 and a NO is surfaced as a 422
        // Error::Api by put_script itself.
        Ok(SieveScriptMeta {
            id: name.to_string(),
            name: name.to_string(),
            content: Some(content.to_string()),
            is_active: false,
            is_valid: true,
            validation_errors: Vec::new(),
        })
    }

    async fn delete_script(&self, name: &str) -> Result<(), Error> {
        let mut session = self.connect().await?;
        // Idempotent: a NO for "no such script" is treated as success.
        match session.delete_script(name).await {
            Ok(()) => Ok(()),
            Err(Error::Api { status: 404, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn activate_script(&self, name: &str) -> Result<(), Error> {
        let mut session = self.connect().await?;
        session.set_active(name).await
    }

    async fn get_active(&self) -> Result<Option<String>, Error> {
        let mut session = self.connect().await?;
        let scripts = session.list_scripts().await?;
        Ok(scripts.into_iter().find(|s| s.active).map(|s| s.name))
    }
}