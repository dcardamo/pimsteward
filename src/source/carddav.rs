//! CardDAV-backed ContactsSource.
//!
//! Queries the default addressbook at
//! `/dav/<user>/addressbooks/default/` via REPORT `addressbook-query`.
//! Forwardemail exposes one addressbook per alias, so there's no need to
//! enumerate multiple collections.
//!
//! Live-tested against `carddav.forwardemail.net` with a forwardemail
//! alias. Note the different subdomain vs CalDAV — forwardemail runs the
//! two servers as separate processes.

use crate::error::Error;
use crate::forwardemail::contacts::Contact;
use crate::source::dav::{DavClient, DavConfig};
use crate::source::traits::ContactsSource;
use async_trait::async_trait;

pub struct DavContactsSource {
    client: DavClient,
    user: String,
}

impl std::fmt::Debug for DavContactsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavContactsSource")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl DavContactsSource {
    pub fn new(
        base_url: impl Into<String>,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, Error> {
        let user = user.into();
        let client = DavClient::new(DavConfig {
            base_url: base_url.into(),
            user: user.clone(),
            password: password.into(),
        })?;
        Ok(Self { client, user })
    }

    fn default_addressbook_path(&self) -> String {
        format!("/dav/{}/addressbooks/default/", self.user)
    }
}

#[async_trait]
impl ContactsSource for DavContactsSource {
    fn tag(&self) -> &'static str {
        "carddav"
    }

    async fn list_contacts(&self) -> Result<Vec<Contact>, Error> {
        let body = r#"<?xml version="1.0"?>
<CR:addressbook-query xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
  <D:prop>
    <D:getetag/>
    <CR:address-data/>
  </D:prop>
</CR:addressbook-query>"#;
        let ms = self
            .client
            .report(&self.default_addressbook_path(), 1, body)
            .await?;

        Ok(ms
            .responses
            .into_iter()
            .filter_map(|r| {
                let content = r.address_data?;
                let uid = extract_vcard_uid(&content).unwrap_or_default();
                // Forwardemail id = the last path segment of the href,
                // stripping the .vcf extension. Same scheme as the REST API's
                // `id` field.
                let id = r
                    .href
                    .rsplit('/')
                    .next()
                    .map(|s| s.trim_end_matches(".vcf").to_string())
                    .unwrap_or_default();
                let full_name = extract_vcard_fn(&content).unwrap_or_default();
                Some(Contact {
                    id,
                    uid,
                    full_name,
                    content,
                    etag: r.etag.unwrap_or_default(),
                    is_group: false,
                    created_at: None,
                    updated_at: None,
                })
            })
            .collect())
    }
}

fn extract_vcard_uid(vcf: &str) -> Option<String> {
    for line in vcf.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("UID:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn extract_vcard_fn(vcf: &str) -> Option<String> {
    for line in vcf.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("FN:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_fields_from_vcard() {
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nUID:abc-123\nFN:Alice Smith\nEMAIL:a@b.com\nEND:VCARD";
        assert_eq!(extract_vcard_uid(vcf), Some("abc-123".into()));
        assert_eq!(extract_vcard_fn(vcf), Some("Alice Smith".into()));
    }
}
