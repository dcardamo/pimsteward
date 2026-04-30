//! RFC 6764 CalDAV discovery walk against iCloud.
//!
//! The walk:
//!
//! 1. PROPFIND `<base_url>/.well-known/caldav` for `current-user-principal`.
//!    iCloud responds with a redirect to the user's principal shard
//!    (`pNN-caldav.icloud.com`); reqwest's redirect policy follows it
//!    automatically. The `<href>` returned here points at the principal
//!    URL on that shard.
//! 2. PROPFIND on the principal URL for `calendar-home-set`.
//! 3. PROPFIND `Depth: 1` on the calendar-home-set, parsing each
//!    `<response>` for resourcetype, displayname, getctag, calendar-color,
//!    and supported-calendar-component-set. Filter to collections that
//!    are calendars AND advertise `VEVENT` in their component set.
//!
//! XML is parsed with `quick-xml` streaming events. Local element names
//! are matched case-insensitively, ignoring namespace prefix — mirrors
//! the pattern in `src/source/dav.rs`. iCloud mixes default namespaces
//! and `cal:`/`d:` prefixes across responses, so prefix-aware matching
//! would be brittle.
//!
//! User-Agent is set on every request — iCloud rejects empty UAs with 403.

use crate::error::Error;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use reqwest::{Method, Url};
use std::time::Duration;

/// One calendar collection discovered on iCloud. The `url` is the full
/// CalDAV URL of the collection — that's the stable id used in the
/// `_calendar.json` manifest and downstream MCP tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCalendar {
    pub url: String,
    pub displayname: String,
    pub ctag: Option<String>,
    pub color: Option<String>,
    /// e.g. `["VEVENT"]`, `["VEVENT", "VTODO"]`. iCloud's Reminders
    /// calendars expose only `VTODO`; pimsteward filters those out at
    /// `discover()` time because we only handle `VEVENT`.
    pub supported_components: Vec<String>,
}

const PROPFIND_PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:current-user-principal/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_HOME_SET_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <c:calendar-home-set/>
  </d:prop>
</d:propfind>"#;

const PROPFIND_CALENDAR_LIST_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"
            xmlns:cs="http://calendarserver.org/ns/"
            xmlns:ic="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <cs:getctag/>
    <ic:calendar-color/>
    <c:supported-calendar-component-set/>
  </d:prop>
</d:propfind>"#;

/// Walk well-known → principal → calendar-home-set → calendar list.
///
/// `base_url` is typically `"https://caldav.icloud.com/"`. `user_agent`
/// must be non-empty (iCloud 403s on empty UA). `user` is the Apple ID
/// email; `password` is an app-specific password from appleid.apple.com.
pub async fn discover(
    base_url: &str,
    user_agent: &str,
    user: &str,
    password: &str,
) -> Result<Vec<DiscoveredCalendar>, Error> {
    if user_agent.trim().is_empty() {
        return Err(Error::config(
            "iCloud CalDAV discovery requires a non-empty User-Agent",
        ));
    }

    let client = reqwest::Client::builder()
        .user_agent(user_agent.to_string())
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    // Step 1 — Principal URL via well-known.
    let well_known = join_url(base_url, ".well-known/caldav")?;
    let (principal_resp_url, principal_xml) =
        propfind(&client, well_known.clone(), 0, PROPFIND_PRINCIPAL_BODY, user, password).await?;
    let principal_href = parse_principal_href(&principal_xml)?
        .ok_or_else(|| Error::config("iCloud discovery: principal href missing in response"))?;
    let principal_url = principal_resp_url
        .join(&principal_href)
        .map_err(|e| Error::config(format!("iCloud discovery: bad principal href: {e}")))?;

    // Step 2 — Calendar home set on the principal URL.
    let (home_resp_url, home_xml) =
        propfind(&client, principal_url, 0, PROPFIND_HOME_SET_BODY, user, password).await?;
    let home_href = parse_calendar_home_set_href(&home_xml)?.ok_or_else(|| {
        Error::config("iCloud discovery: calendar-home-set href missing in response")
    })?;
    let home_url = home_resp_url
        .join(&home_href)
        .map_err(|e| Error::config(format!("iCloud discovery: bad home-set href: {e}")))?;

    // Step 3 — Calendar list. Depth: 1 to enumerate child collections.
    let (calendars_resp_url, calendars_xml) =
        propfind(&client, home_url, 1, PROPFIND_CALENDAR_LIST_BODY, user, password).await?;
    let calendars = parse_calendar_list(&calendars_xml, &calendars_resp_url)?;

    Ok(calendars)
}

/// Issue a PROPFIND and return `(final response URL after redirects, body)`.
/// The response URL is needed to resolve relative `<href>` values.
async fn propfind(
    client: &reqwest::Client,
    url: Url,
    depth: u8,
    body: &'static str,
    user: &str,
    password: &str,
) -> Result<(Url, Vec<u8>), Error> {
    let method = Method::from_bytes(b"PROPFIND")
        .map_err(|e| Error::config(format!("invalid HTTP method: {e}")))?;
    let resp = client
        .request(method, url.clone())
        .basic_auth(user, Some(password))
        .header("Depth", depth.to_string())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )
        .body(body)
        .send()
        .await?;
    let final_url = resp.url().clone();
    let status = resp.status();
    let bytes = resp.bytes().await?.to_vec();
    if !status.is_success() {
        return Err(Error::Api {
            status: status.as_u16(),
            message: format!(
                "iCloud PROPFIND {}: {}",
                url,
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        });
    }
    Ok((final_url, bytes))
}

fn join_url(base: &str, relative: &str) -> Result<Url, Error> {
    let base_with_slash = if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    let parsed = Url::parse(&base_with_slash)
        .map_err(|e| Error::config(format!("invalid iCloud base URL {base}: {e}")))?;
    parsed
        .join(relative)
        .map_err(|e| Error::config(format!("invalid iCloud base URL join: {e}")))
}

/// Lowercased local name of a possibly-namespaced XML element name.
/// `b"D:response"` → `"response"`, `b"cal:calendar"` → `"calendar"`.
fn local_name(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rfind(':') {
        Some(i) => s[i + 1..].to_lowercase(),
        None => s.to_lowercase(),
    }
}

/// Extract the first non-empty `<href>` text inside a
/// `<current-user-principal>` element. Returns `None` if the element is
/// absent.
pub fn parse_principal_href(xml: &[u8]) -> Result<Option<String>, Error> {
    parse_first_href_inside(xml, "current-user-principal")
}

/// Extract the first non-empty `<href>` text inside a
/// `<calendar-home-set>` element.
pub fn parse_calendar_home_set_href(xml: &[u8]) -> Result<Option<String>, Error> {
    parse_first_href_inside(xml, "calendar-home-set")
}

/// Walk a multistatus response and return the first `<href>` text that
/// appears nested inside an element whose local name matches `parent`.
/// Used by both Step 1 (principal) and Step 2 (calendar-home-set).
fn parse_first_href_inside(xml: &[u8], parent: &str) -> Result<Option<String>, Error> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text_buf = String::new();
    let mut found: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(Error::config(format!("iCloud XML parse: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                stack.push(local_name(e.name().as_ref()));
                text_buf.clear();
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.decode() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "href"
                    && found.is_none()
                    && stack.iter().rev().any(|n| n == parent)
                {
                    let trimmed = text_buf.trim();
                    if !trimmed.is_empty() {
                        found = Some(trimmed.to_string());
                    }
                }
                stack.pop();
                text_buf.clear();
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(found)
}

/// Parse a calendar-list multistatus response into discovered calendars.
/// Resolves relative hrefs against `request_url`. Filters out collections
/// that are not calendars and calendars whose component set lacks VEVENT.
pub fn parse_calendar_list(
    xml: &[u8],
    request_url: &Url,
) -> Result<Vec<DiscoveredCalendar>, Error> {
    #[derive(Default)]
    struct InProgress {
        href: String,
        displayname: Option<String>,
        ctag: Option<String>,
        color: Option<String>,
        is_calendar: bool,
        components: Vec<String>,
    }

    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut text_buf = String::new();
    let mut current: Option<InProgress> = None;
    let mut out: Vec<DiscoveredCalendar> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(Error::config(format!("iCloud XML parse: {e}"))),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    current = Some(InProgress::default());
                }
                stack.push(name);
                text_buf.clear();
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                if let Some(ref mut r) = current {
                    // `<calendar/>` inside `<resourcetype>` flags this
                    // collection as a calendar.
                    if name == "calendar"
                        && stack.iter().rev().any(|n| n == "resourcetype")
                    {
                        r.is_calendar = true;
                    }
                    // `<comp name="VEVENT"/>` inside
                    // `<supported-calendar-component-set>` lists the
                    // components this calendar advertises.
                    if name == "comp"
                        && stack
                            .iter()
                            .rev()
                            .any(|n| n == "supported-calendar-component-set")
                    {
                        if let Some(comp) = attr_value(&e, b"name") {
                            r.components.push(comp);
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.decode() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if let Some(ref mut r) = current {
                    match name.as_str() {
                        "href" => {
                            // Only the first <href> inside a <response> is the
                            // collection URL. Subsequent ones may appear inside
                            // nested props.
                            if r.href.is_empty()
                                && !stack
                                    .iter()
                                    .rev()
                                    .any(|n| n == "current-user-principal" || n == "calendar-home-set")
                            {
                                let trimmed = text_buf.trim();
                                if !trimmed.is_empty() {
                                    r.href = trimmed.to_string();
                                }
                            }
                        }
                        "displayname" => {
                            let trimmed = text_buf.trim();
                            if !trimmed.is_empty() && r.displayname.is_none() {
                                r.displayname = Some(trimmed.to_string());
                            }
                        }
                        "getctag" => {
                            let trimmed = text_buf.trim();
                            if !trimmed.is_empty() {
                                r.ctag = Some(trimmed.to_string());
                            }
                        }
                        "calendar-color" => {
                            let trimmed = text_buf.trim();
                            if !trimmed.is_empty() {
                                r.color = Some(trimmed.to_string());
                            }
                        }
                        "response" => {
                            if let Some(p) = current.take() {
                                if p.is_calendar
                                    && !p.href.is_empty()
                                    && p.components.iter().any(|c| c.eq_ignore_ascii_case("VEVENT"))
                                {
                                    let resolved = request_url
                                        .join(&p.href)
                                        .map_err(|e| {
                                            Error::config(format!(
                                                "iCloud discovery: bad calendar href {}: {e}",
                                                p.href
                                            ))
                                        })?;
                                    out.push(DiscoveredCalendar {
                                        url: resolved.to_string(),
                                        displayname: p.displayname.unwrap_or_default(),
                                        ctag: p.ctag,
                                        color: p.color,
                                        supported_components: p.components,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                stack.pop();
                text_buf.clear();
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// Pull a UTF-8 attribute value off an XML element, ignoring namespace
/// prefix on the attribute name. Returns `None` if absent or malformed.
fn attr_value(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    for attr in e.attributes().with_checks(false).flatten() {
        let raw = attr.key.as_ref();
        let local = match raw.iter().rposition(|b| *b == b':') {
            Some(i) => &raw[i + 1..],
            None => raw,
        };
        if local.eq_ignore_ascii_case(key) {
            if let Ok(decoded) = std::str::from_utf8(&attr.value) {
                return Some(decoded.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_name_strips_prefix() {
        assert_eq!(local_name(b"D:response"), "response");
        assert_eq!(local_name(b"cal:calendar"), "calendar");
        assert_eq!(local_name(b"href"), "href");
    }

    #[test]
    fn parse_principal_extracts_relative_href() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat>
      <prop>
        <current-user-principal>
          <href>/123456789/principal/</href>
        </current-user-principal>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;
        let got = parse_principal_href(xml).unwrap();
        assert_eq!(got.as_deref(), Some("/123456789/principal/"));
    }
}
