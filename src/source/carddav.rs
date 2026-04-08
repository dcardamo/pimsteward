//! CardDAV-backed ContactsSource.
//!
//! Discovers all addressbooks via PROPFIND on
//! `/dav/<user>/addressbooks/`, then queries each via REPORT
//! `addressbook-query`. Forwardemail may expose multiple addressbooks
//! per alias (e.g. `default` and `card`), so we enumerate all of them
//! and merge the results — same pattern CalDAV uses for calendars.
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

    /// PROPFIND path for discovering all addressbooks.
    fn addressbooks_home_path(&self) -> String {
        format!("/dav/{}/addressbooks/", self.user)
    }

    /// Discover all addressbook collections via PROPFIND on the
    /// addressbooks home. Returns the href of each collection that
    /// advertises `<card:addressbook/>` in its resourcetype.
    async fn discover_addressbooks(&self) -> Result<Vec<String>, Error> {
        let body = r#"<?xml version="1.0"?>
<D:propfind xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
  </D:prop>
</D:propfind>"#;
        let ms = self
            .client
            .propfind(&self.addressbooks_home_path(), 1, body)
            .await?;

        let books: Vec<String> = ms
            .responses
            .into_iter()
            .filter(|r| r.is_addressbook)
            .map(|r| r.href)
            .collect();

        tracing::debug!(count = books.len(), ?books, "discovered CardDAV addressbooks");
        Ok(books)
    }

    /// Fetch all contacts from a single addressbook collection.
    async fn list_contacts_in(&self, addressbook_href: &str) -> Result<Vec<Contact>, Error> {
        let body = r#"<?xml version="1.0"?>
<CR:addressbook-query xmlns:D="DAV:" xmlns:CR="urn:ietf:params:xml:ns:carddav">
  <D:prop>
    <D:getetag/>
    <CR:address-data/>
  </D:prop>
</CR:addressbook-query>"#;
        let ms = self.client.report(addressbook_href, 1, body).await?;

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

#[async_trait]
impl ContactsSource for DavContactsSource {
    fn tag(&self) -> &'static str {
        "carddav"
    }

    async fn list_contacts(&self) -> Result<Vec<Contact>, Error> {
        let books = self.discover_addressbooks().await?;
        let mut all = Vec::new();
        for href in &books {
            let contacts = self.list_contacts_in(href).await?;
            tracing::debug!(addressbook = %href, count = contacts.len(), "fetched contacts");
            all.extend(contacts);
        }
        Ok(all)
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
