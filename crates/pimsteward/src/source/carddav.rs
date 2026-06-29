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
use crate::source::dav::{DavClient, DavConfig, DavPrecondition};
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

/// Deterministic per-contact object URL: `<addressbook>/<uid>.vcf`.
///
/// `addressbook` is the addressbook collection URL (or path), `uid` is the
/// vCard UID. The UID is path-segment-encoded (reusing the CalDAV writer's
/// encoder) so a UID with reserved characters maps to one safe segment. The
/// mapping is pure: re-PUTting the same UID overwrites the existing object,
/// so contacts writes are idempotent — no duplicate vCard is produced.
fn carddav_object_href(addressbook: &str, uid: &str) -> String {
    format!(
        "{}/{}.vcf",
        addressbook.trim_end_matches('/'),
        crate::source::caldav::encode_uid_segment(uid)
    )
}

// ─── Writer ─────────────────────────────────────────────────────────────

/// Generic, standards-compliant CardDAV writer.
///
/// The contacts side has no `ContactsWriter` trait (mirroring how the
/// forwardemail contacts writes are exposed as inherent `Client` methods),
/// so `DavCalendarWriter`'s counterpart for contacts exposes inherent
/// `create_contact` / `update_contact` / `delete_contact` methods that the
/// StalwartProvider calls directly. Each maps a vCard to a deterministic
/// `<addressbook>/<uid>.vcf` object written with HTTP PUT/DELETE via the
/// shared [`DavClient`].
///
/// `if_match` semantics match the CalDAV writer: empty means "no
/// precondition" (used for upserts / create), a non-empty etag is honored
/// strictly for optimistic concurrency (stale → [`Error::PreconditionFailed`]).
/// Create uses `If-None-Match: *`.
pub struct DavContactsWriter {
    client: DavClient,
    user: String,
}

impl std::fmt::Debug for DavContactsWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DavContactsWriter")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl DavContactsWriter {
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

    /// Create a new contact: PUT `<addressbook>/<uid>.vcf` with
    /// `If-None-Match: *`. Returns the synthesized post-write `Contact`.
    pub async fn create_contact(
        &self,
        addressbook: &str,
        uid: &str,
        vcard: &str,
    ) -> Result<Contact, Error> {
        self.put_contact(addressbook, uid, vcard, DavPrecondition::IfNoneMatch)
            .await
    }

    /// Update an existing contact: PUT `<addressbook>/<uid>.vcf`. A
    /// non-empty `if_match` is honored strictly; an empty one writes
    /// unconditionally (upsert).
    pub async fn update_contact(
        &self,
        addressbook: &str,
        uid: &str,
        vcard: &str,
        if_match: &str,
    ) -> Result<Contact, Error> {
        let precondition = if if_match.is_empty() {
            DavPrecondition::None
        } else {
            DavPrecondition::IfMatch(if_match.to_string())
        };
        self.put_contact(addressbook, uid, vcard, precondition).await
    }

    /// Delete a contact: DELETE `<addressbook>/<uid>.vcf`. 404/410 count as
    /// success (idempotent delete). A non-empty `if_match` is honored.
    pub async fn delete_contact(
        &self,
        addressbook: &str,
        uid: &str,
        if_match: &str,
    ) -> Result<(), Error> {
        let href = carddav_object_href(addressbook, uid);
        let precondition = if if_match.is_empty() {
            DavPrecondition::None
        } else {
            DavPrecondition::IfMatch(if_match.to_string())
        };
        self.client.delete_object(&href, precondition).await
    }

    async fn put_contact(
        &self,
        addressbook: &str,
        uid: &str,
        vcard: &str,
        precondition: DavPrecondition,
    ) -> Result<Contact, Error> {
        let href = carddav_object_href(addressbook, uid);
        let etag = self
            .client
            .put_object(
                &href,
                vcard.as_bytes(),
                "text/vcard; charset=utf-8",
                precondition,
            )
            .await?;
        Ok(synthesize_contact(uid, vcard, etag))
    }
}

/// Build a `Contact` from a successful CardDAV PUT. CardDAV PUT returns an
/// empty body, so the caller's vCard is canonical: UID/FN are extracted
/// from it (falling back to the request `uid`), `id` is the `.vcf` filename
/// tail, and `etag` is the response etag (empty string if the server
/// omitted it, matching the source-side `Contact.etag` String contract).
fn synthesize_contact(uid: &str, vcard: &str, etag: Option<String>) -> Contact {
    let encoded = crate::source::caldav::encode_uid_segment(uid);
    Contact {
        id: format!("{encoded}.vcf"),
        uid: extract_vcard_uid(vcard).unwrap_or_else(|| uid.to_string()),
        full_name: extract_vcard_fn(vcard).unwrap_or_default(),
        content: vcard.to_string(),
        etag: etag.unwrap_or_default(),
        is_group: false,
        created_at: None,
        updated_at: None,
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
    fn carddav_object_href_is_deterministic() {
        let base = "https://h:8443/dav/card/dan%40example.test/default";
        assert_eq!(
            carddav_object_href(base, "ABC-123"),
            "https://h:8443/dav/card/dan%40example.test/default/ABC-123.vcf"
        );
        assert_eq!(
            carddav_object_href(base, "ABC-123"),
            carddav_object_href(base, "ABC-123")
        );
    }

    #[test]
    fn carddav_object_href_trims_slash_and_encodes() {
        assert_eq!(
            carddav_object_href("https://h/card/default/", "a/b"),
            "https://h/card/default/a%2Fb.vcf"
        );
    }

    #[test]
    fn synthesize_contact_extracts_uid_and_fn() {
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nUID:abc-123\nFN:Alice Smith\nEND:VCARD";
        let c = synthesize_contact("abc-123", vcf, Some("\"e1\"".into()));
        assert_eq!(c.id, "abc-123.vcf");
        assert_eq!(c.uid, "abc-123");
        assert_eq!(c.full_name, "Alice Smith");
        assert_eq!(c.etag, "\"e1\"");
        assert_eq!(c.content, vcf);
    }

    #[test]
    fn extract_fields_from_vcard() {
        let vcf = "BEGIN:VCARD\nVERSION:3.0\nUID:abc-123\nFN:Alice Smith\nEMAIL:a@b.com\nEND:VCARD";
        assert_eq!(extract_vcard_uid(vcf), Some("abc-123".into()));
        assert_eq!(extract_vcard_fn(vcf), Some("Alice Smith".into()));
    }
}
