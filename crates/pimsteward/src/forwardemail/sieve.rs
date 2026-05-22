//! Sieve scripts endpoint wrapper (`/v1/sieve-scripts`).
//!
//! Smoke test findings: create field is `content` (not `script`), server
//! parses and returns `is_valid`, `required_capabilities`, `security_warnings`.

use crate::error::Error;
use crate::forwardemail::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SieveScript {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Raw sieve script text. Present on single-get responses; may be absent
    /// on list responses.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_valid: bool,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub security_warnings: Vec<String>,
    #[serde(default)]
    pub validation_errors: Vec<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Client {
    /// GET /v1/sieve-scripts
    pub async fn list_sieve_scripts(&self) -> Result<Vec<SieveScript>, Error> {
        self.get_json("/v1/sieve-scripts?limit=50").await
    }

    /// GET /v1/sieve-scripts/:id
    pub async fn get_sieve_script(&self, id: &str) -> Result<SieveScript, Error> {
        self.get_json(&format!("/v1/sieve-scripts/{id}")).await
    }
}
