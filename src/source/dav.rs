//! Shared HTTP-over-WebDAV client used by both [`DavCalendarSource`] and
//! [`DavContactsSource`]. Keeps PROPFIND / REPORT request construction,
//! HTTP Basic auth, TLS, and response parsing in one place.
//!
//! Forwardemail's DAV servers are standard WebDAV with the
//! `urn:ietf:params:xml:ns:caldav` and `urn:ietf:params:xml:ns:carddav`
//! extensions. CalDAV lives at `caldav.forwardemail.net` and CardDAV at
//! `carddav.forwardemail.net` — different subdomains. Both accept the
//! alias email as Basic Auth username and the generated alias password as
//! the password.
//!
//! The request shapes used by pimsteward are small:
//!
//! - `PROPFIND` depth=1 on a home collection — enumerates calendars or
//!   addressbooks plus their metadata (displayname, resourcetype).
//! - `REPORT` with `calendar-query` or `addressbook-query` — returns
//!   every item in a collection with its etag and body in one round
//!   trip. This is the key efficiency win over REST: one HTTP call per
//!   collection rather than N calls per item.
//!
//! We parse the XML responses with `quick-xml` into loose structures
//! driven by the elements we actually care about (href, getetag,
//! calendar-data, address-data, displayname). Namespaces are tolerated
//! permissively — we match on the local name because forwardemail mixes
//! `D:` and `d:` prefixes across endpoints.

use crate::error::Error;
use reqwest::Method;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DavConfig {
    pub base_url: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct DavClient {
    http: reqwest::Client,
    config: DavConfig,
}

impl DavClient {
    pub fn new(config: DavConfig) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("pimsteward/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { http, config })
    }

    /// Send a PROPFIND request and return the parsed multistatus.
    pub async fn propfind(
        &self,
        path: &str,
        depth: u8,
        body: &str,
    ) -> Result<DavMultistatus, Error> {
        let method = Method::from_bytes(b"PROPFIND")
            .map_err(|e| Error::store(format!("invalid HTTP method: {e}")))?;
        self.send_dav(method, path, depth, body).await
    }

    /// Send a REPORT request and return the parsed multistatus.
    pub async fn report(&self, path: &str, depth: u8, body: &str) -> Result<DavMultistatus, Error> {
        let method = Method::from_bytes(b"REPORT")
            .map_err(|e| Error::store(format!("invalid HTTP method: {e}")))?;
        self.send_dav(method, path, depth, body).await
    }

    async fn send_dav(
        &self,
        method: Method,
        path: &str,
        depth: u8,
        body: &str,
    ) -> Result<DavMultistatus, Error> {
        let url = format!("{}{}", self.config.base_url, path);
        let resp = self
            .http
            .request(method, &url)
            .basic_auth(&self.config.user, Some(&self.config.password))
            .header("Depth", depth.to_string())
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(body.to_string())
            .send()
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(Error::Api {
                status: status.as_u16(),
                message: format!(
                    "DAV {} {}: {}",
                    method_name(&status),
                    path,
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
            });
        }
        DavMultistatus::parse(&bytes)
    }
}

fn method_name(status: &reqwest::StatusCode) -> String {
    format!("{}", status.as_u16())
}

/// A parsed multistatus DAV response. Each element corresponds to one
/// `<D:response>` in the XML, keyed by href.
#[derive(Debug, Clone, Default)]
pub struct DavMultistatus {
    pub responses: Vec<DavResponse>,
}

#[derive(Debug, Clone, Default)]
pub struct DavResponse {
    pub href: String,
    pub etag: Option<String>,
    pub displayname: Option<String>,
    /// True if this response advertised `<D:resourcetype>` containing
    /// `<CAL:calendar/>` — i.e. it's a CalDAV calendar collection.
    pub is_calendar: bool,
    /// True if `<CARD:addressbook/>` is present.
    pub is_addressbook: bool,
    /// `<CAL:calendar-data>` contents, if present.
    pub calendar_data: Option<String>,
    /// `<CARD:address-data>` contents, if present.
    pub address_data: Option<String>,
}

impl DavMultistatus {
    /// Parse a DAV multistatus XML response. Permissively matches on local
    /// element names (ignoring the namespace prefix) because forwardemail's
    /// responses mix `D:`, `d:`, `CAL:`, `cal:`, `card:` prefixes.
    pub fn parse(xml: &[u8]) -> Result<Self, Error> {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let mut reader = Reader::from_reader(xml);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut out = DavMultistatus::default();
        let mut stack: Vec<String> = Vec::new();
        let mut current: Option<DavResponse> = None;
        let mut text_buf = String::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Err(e) => {
                    return Err(Error::store(format!("DAV XML parse: {e}")));
                }
                Ok(Event::Eof) => break,
                Ok(Event::Start(e)) => {
                    let name = local_name(e.name().as_ref());
                    if name == "response" {
                        current = Some(DavResponse::default());
                    }
                    stack.push(name);
                    text_buf.clear();
                }
                Ok(Event::Empty(e)) => {
                    let name = local_name(e.name().as_ref());
                    if let Some(ref mut r) = current {
                        match name.as_str() {
                            "calendar" => r.is_calendar = true,
                            "addressbook" => r.is_addressbook = true,
                            _ => {}
                        }
                    }
                }
                Ok(Event::Text(t)) => {
                    if let Ok(s) = t.decode() {
                        text_buf.push_str(&s);
                    }
                }
                Ok(Event::CData(c)) => {
                    // Some servers wrap vCard/iCal data in CDATA sections.
                    let data = String::from_utf8_lossy(c.as_ref()).into_owned();
                    text_buf.push_str(&data);
                }
                Ok(Event::End(e)) => {
                    let name = local_name(e.name().as_ref());
                    let popped = stack.pop();
                    debug_assert_eq!(popped.as_deref(), Some(name.as_str()));
                    if let Some(ref mut r) = current {
                        match name.as_str() {
                            "href" => {
                                if r.href.is_empty() {
                                    r.href = text_buf.trim().to_string();
                                }
                            }
                            "getetag" => r.etag = Some(text_buf.trim().to_string()),
                            "displayname" => r.displayname = Some(text_buf.trim().to_string()),
                            "calendar-data" => r.calendar_data = Some(text_buf.trim().to_string()),
                            "address-data" => r.address_data = Some(text_buf.trim().to_string()),
                            "response" => {
                                if let Some(resp) = current.take() {
                                    out.responses.push(resp);
                                }
                            }
                            _ => {}
                        }
                    }
                    text_buf.clear();
                }
                _ => {}
            }
            buf.clear();
        }
        Ok(out)
    }
}

/// Return the local part of a possibly-namespaced XML element name.
/// `b"D:response"` → `"response"`, `b"card:address-data"` → `"address-data"`.
fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rfind(':') {
        Some(i) => s[i + 1..].to_lowercase(),
        None => s.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calendar_query_response() {
        let xml = br#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/dav/test/cal-1/event-uid.ics</D:href>
    <D:propstat>
      <D:status>HTTP/1.1 200 OK</D:status>
      <D:prop>
        <D:getetag>"abc123"</D:getetag>
        <CAL:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
BEGIN:VEVENT
UID:event-uid
SUMMARY:Test
END:VEVENT
END:VCALENDAR</CAL:calendar-data>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let ms = DavMultistatus::parse(xml).unwrap();
        assert_eq!(ms.responses.len(), 1);
        let r = &ms.responses[0];
        assert_eq!(r.href, "/dav/test/cal-1/event-uid.ics");
        assert_eq!(r.etag.as_deref(), Some("\"abc123\""));
        assert!(r.calendar_data.as_deref().unwrap().contains("SUMMARY:Test"));
    }

    #[test]
    fn parses_addressbook_query_response() {
        let xml = br#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:response>
    <d:href>/dav/test/addressbooks/default/uid.vcf</d:href>
    <d:propstat>
      <d:prop>
        <d:getetag>"vcf-etag"</d:getetag>
        <card:address-data>BEGIN:VCARD
VERSION:3.0
UID:uid
FN:Alice
END:VCARD</card:address-data>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let ms = DavMultistatus::parse(xml).unwrap();
        assert_eq!(ms.responses.len(), 1);
        let r = &ms.responses[0];
        assert_eq!(r.href, "/dav/test/addressbooks/default/uid.vcf");
        assert_eq!(r.etag.as_deref(), Some("\"vcf-etag\""));
        assert!(r.address_data.as_deref().unwrap().contains("FN:Alice"));
    }

    #[test]
    fn parses_propfind_collections() {
        let xml = br#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/dav/test/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
      </D:prop>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/test/cal-a/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/><CAL:calendar/></D:resourcetype>
        <D:displayname>Work</D:displayname>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let ms = DavMultistatus::parse(xml).unwrap();
        assert_eq!(ms.responses.len(), 2);
        assert!(!ms.responses[0].is_calendar);
        assert!(ms.responses[1].is_calendar);
        assert_eq!(ms.responses[1].displayname.as_deref(), Some("Work"));
    }

    #[test]
    fn local_name_strips_prefix() {
        assert_eq!(local_name(b"D:response"), "response");
        assert_eq!(local_name(b"card:address-data"), "address-data");
        assert_eq!(local_name(b"foo"), "foo");
    }
}
