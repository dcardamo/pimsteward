//! `.eml` → `MessageRow` conversion.
//!
//! Separated from `mod.rs` so the MIME/mailparse side is testable in
//! isolation from the SQL side.  The parser takes raw RFC822 bytes and a
//! `MessageMeta` sidecar and produces a `MessageRow` ready for
//! `SearchIndex::upsert_message`.

use mailparse::{MailHeaderMap, ParsedMail, addrparse, dateparse};

use crate::error::Error;

/// A single row destined for `messages` + `messages_body`, derived from
/// the raw RFC822 bytes plus the on-disk `MessageMeta` sidecar.  The
/// envelope parser populates everything; the pull loop and rebuild call
/// site decide what `folder` to attribute it to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageRow {
    pub canonical_id: String,
    pub folder: String,
    pub source_id: String,
    pub message_id: Option<String>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub to_addrs: Option<String>,
    pub cc_addrs: Option<String>,
    pub subject: Option<String>,
    /// Unix timestamp, seconds.  Pulled from the `Date:` header when
    /// present; otherwise from the `internal_date` field in MessageMeta.
    pub date_unix: Option<i64>,
    pub size: Option<i64>,
    pub flags: Vec<String>,
    pub has_attachments: bool,
    /// Extracted plain-text body, truncated to [`super::BODY_CAP_BYTES`].
    pub body_text: Option<String>,
}

/// Envelope fields supplied by the caller from `<id>.meta.json`.  Kept
/// as a plain struct (rather than borrowing `crate::pull::mail::MessageMeta`
/// directly) so `parse_eml` can be unit-tested without dragging in the
/// whole pull module.  The pull loop and rebuild build this struct from
/// the real `MessageMeta`.
#[derive(Debug, Clone, Default)]
pub struct MetaFacts<'a> {
    pub canonical_id: &'a str,
    pub folder: &'a str,
    pub source_id: &'a str,
    pub flags: &'a [String],
    /// Optional ISO8601 fallback date when the `.eml` has no parseable
    /// `Date:` header.
    pub internal_date: Option<&'a str>,
    /// Optional size from meta.  If None, `parse_eml` falls back to the
    /// byte length of the raw input.
    pub size: Option<u64>,
}

/// Parse the RFC822 bytes and build a `MessageRow`.
///
/// Returns an error only for fundamentally unusable inputs — right now
/// that's just "mailparse rejected the bytes" and "no Message-ID header
/// and none derivable."  Callers (pull loop, rebuild) should log and
/// skip rather than abort the whole run on a single bad message.
pub fn parse_eml(raw: &[u8], meta: &MetaFacts<'_>) -> Result<MessageRow, Error> {
    let parsed = mailparse::parse_mail(raw)
        .map_err(|e| Error::index(format!("parse_mail: {e}")))?;

    let message_id = parsed
        .headers
        .get_first_value("Message-ID")
        .or_else(|| parsed.headers.get_first_value("Message-Id"))
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty());

    let subject = parsed.headers.get_first_value("Subject");

    let (from_addr, from_name) = parse_single_from(&parsed);
    let to_addrs = parse_address_list(&parsed, "To");
    let cc_addrs = parse_address_list(&parsed, "Cc");

    let date_unix = parsed
        .headers
        .get_first_value("Date")
        .and_then(|d| dateparse(&d).ok())
        .or_else(|| meta.internal_date.and_then(parse_iso8601_to_unix));

    let has_attachments = walk_has_attachments(&parsed);
    let body_text = extract_body_text(&parsed);

    let size = meta.size.map(|s| s as i64).or(Some(raw.len() as i64));

    Ok(MessageRow {
        canonical_id: meta.canonical_id.to_string(),
        folder: meta.folder.to_string(),
        source_id: meta.source_id.to_string(),
        message_id,
        from_addr,
        from_name,
        to_addrs,
        cc_addrs,
        subject,
        date_unix,
        size,
        flags: meta.flags.to_vec(),
        has_attachments,
        body_text,
    })
}

// ── From / To / Cc extraction ────────────────────────────────────────────

/// Extract the first `From:` address and its display name.  Both come
/// back lowercased in the address part (to match the `messages.from_addr`
/// index's case semantics); the display name is preserved verbatim for
/// FTS tokenization.
fn parse_single_from(parsed: &ParsedMail) -> (Option<String>, Option<String>) {
    let raw = match parsed.headers.get_first_value("From") {
        Some(v) => v,
        None => return (None, None),
    };
    let list = match addrparse(&raw) {
        Ok(l) => l,
        Err(_) => return (None, None),
    };
    for addr in list.iter() {
        if let mailparse::MailAddr::Single(s) = addr {
            let addr_lc = s.addr.to_lowercase();
            let name = s.display_name.clone().filter(|n| !n.is_empty());
            return (Some(addr_lc), name);
        }
    }
    (None, None)
}

/// Flatten a `To:` / `Cc:` header into a comma-joined lowercased string.
/// Group addresses (`Undisclosed-recipients:;`) are walked as well.
/// Returns `None` if the header is missing or yields no addresses.
fn parse_address_list(parsed: &ParsedMail, header: &str) -> Option<String> {
    let raw = parsed.headers.get_first_value(header)?;
    let list = addrparse(&raw).ok()?;
    let mut out = Vec::new();
    for entry in list.iter() {
        match entry {
            mailparse::MailAddr::Single(s) => out.push(s.addr.to_lowercase()),
            mailparse::MailAddr::Group(g) => {
                for inner in &g.addrs {
                    out.push(inner.addr.to_lowercase());
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(", "))
    }
}

// ── Body + attachment detection ──────────────────────────────────────────

/// Walk the MIME tree and return true if any leaf has
/// `Content-Disposition: attachment` OR a multipart/mixed container
/// holds a non-text leaf.
fn walk_has_attachments(parsed: &ParsedMail) -> bool {
    if is_attachment_leaf(parsed) {
        return true;
    }
    parsed.subparts.iter().any(walk_has_attachments)
}

fn is_attachment_leaf(parsed: &ParsedMail) -> bool {
    if !parsed.subparts.is_empty() {
        return false;
    }
    if let Some(cd) = parsed.headers.get_first_value("Content-Disposition") {
        let lc = cd.to_ascii_lowercase();
        if lc.contains("attachment") {
            return true;
        }
        // An explicit filename= parameter on an inline part is still a
        // downloadable blob from the user's perspective.
        if lc.contains("filename=") && !lc.contains("inline;") && !lc.starts_with("inline") {
            return true;
        }
    }
    // text/* leaves without an attachment disposition are the body.
    let ct = parsed.ctype.mimetype.to_ascii_lowercase();
    !ct.starts_with("text/") && !ct.starts_with("multipart/")
}

/// Extract the best plain-text body we can from the MIME tree.  Prefers
/// the first `text/plain` part at any depth.  Falls back to the first
/// `text/html` with tags stripped.  Truncated to `super::BODY_CAP_BYTES`.
fn extract_body_text(parsed: &ParsedMail) -> Option<String> {
    if let Some(t) = find_first_mime(parsed, "text/plain") {
        return Some(cap_bytes(decode_body(t), super::BODY_CAP_BYTES));
    }
    if let Some(h) = find_first_mime(parsed, "text/html") {
        let html = decode_body(h);
        return Some(cap_bytes(html_to_text(&html), super::BODY_CAP_BYTES));
    }
    // No MIME parts at all (single-part message with no Content-Type
    // declared, or just headers).  Fall back to the root body.
    if parsed.subparts.is_empty() && parsed.ctype.mimetype.is_empty() {
        let body = decode_body(parsed);
        if !body.is_empty() {
            return Some(cap_bytes(body, super::BODY_CAP_BYTES));
        }
    }
    None
}

fn find_first_mime<'a>(parsed: &'a ParsedMail<'a>, want: &str) -> Option<&'a ParsedMail<'a>> {
    if parsed.ctype.mimetype.eq_ignore_ascii_case(want) {
        return Some(parsed);
    }
    for sub in &parsed.subparts {
        if let Some(found) = find_first_mime(sub, want) {
            return Some(found);
        }
    }
    None
}

fn decode_body(p: &ParsedMail) -> String {
    // mailparse's get_body() decodes transfer-encoding + charset but
    // leaves the trailing CRLF that separates the body from the next
    // MIME boundary.  Trim it so the body_text we store is the
    // semantic body, not the transport framing.
    p.get_body().unwrap_or_default().trim().to_string()
}

/// Truncate a String to at most `cap` bytes, careful not to split a
/// multi-byte UTF-8 code point.  The resulting body may be slightly
/// shorter than `cap` if the last char-boundary falls earlier.
fn cap_bytes(mut s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut cut = cap;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s
}

// ── Minimal HTML → text for FTS ──────────────────────────────────────────
//
// Scope: "give FTS something useful to tokenize."  Not "render HTML."
// Strips <script>/<style> blocks, strips all other tags, decodes the
// five standard entities plus numeric references, collapses runs of
// whitespace to a single space.

fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // Are we entering <script> or <style>?  If so, skip until
            // the matching closing tag.
            let lower_tail = lower_peek(&html[i..], 16);
            if lower_tail.starts_with("<script") {
                i = skip_until(html, i, "</script>");
                continue;
            }
            if lower_tail.starts_with("<style") {
                i = skip_until(html, i, "</style>");
                continue;
            }
            // Otherwise: skip to the next '>'.  HTML parsing in the
            // wild: a bare '<' with no '>' means the rest of the doc
            // is one long "tag" — we just drop it, which is safer
            // than emitting HTML syntax into the FTS index.
            match html[i..].find('>') {
                Some(end) => {
                    i += end + 1;
                    out.push(' ');
                    continue;
                }
                None => break,
            }
        }
        if b == b'&' {
            if let Some((decoded, consumed)) = decode_entity(&html[i..]) {
                out.push_str(&decoded);
                i += consumed;
                continue;
            }
        }
        out.push(b as char);
        i += 1;
    }
    collapse_whitespace(&out)
}

fn lower_peek(s: &str, n: usize) -> String {
    let take = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    s[..take].to_ascii_lowercase()
}

fn skip_until(s: &str, from: usize, needle: &str) -> usize {
    match s[from..].to_ascii_lowercase().find(needle) {
        Some(rel) => from + rel + needle.len(),
        None => s.len(),
    }
}

fn decode_entity(s: &str) -> Option<(String, usize)> {
    // Named entities: only the five core HTML entities are worth
    // hand-coding.  Anything else passes through as a space via the
    // tag-stripper's fallthrough (entities outside these five rarely
    // matter for search indexing).
    let named = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&nbsp;", " "),
    ];
    for (pat, rep) in named {
        if s.starts_with(pat) {
            return Some((rep.to_string(), pat.len()));
        }
    }
    // Numeric references: &#NN; or &#xHH;.
    if let Some(rest) = s.strip_prefix("&#") {
        if let Some(semi) = rest.find(';') {
            let body = &rest[..semi];
            let cp = if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                body.parse::<u32>().ok()
            };
            if let Some(cp) = cp.and_then(char::from_u32) {
                return Some((cp.to_string(), 2 + semi + 1));
            }
        }
    }
    None
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = true; // drop leading whitespace
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim_end().to_string()
}

// ── ISO8601 fallback parsing ─────────────────────────────────────────────

fn parse_iso8601_to_unix(s: &str) -> Option<i64> {
    // MessageMeta.internal_date is serialized in RFC3339 form by the
    // source traits (forwardemail and IMAP both normalize to it).
    // mailparse's dateparse only speaks RFC2822, so use chrono.
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>() -> MetaFacts<'a> {
        MetaFacts {
            canonical_id: "abcd1234abcd1234",
            folder: "INBOX",
            source_id: "src-1",
            flags: &[],
            internal_date: None,
            size: None,
        }
    }

    #[test]
    fn plain_text_basic() {
        let raw = b"From: alice@example.com\r\n\
                    To: bob@example.com\r\n\
                    Subject: hi there\r\n\
                    Message-ID: <m1@example.com>\r\n\
                    Date: Mon, 20 Apr 2026 11:02:18 +0000\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    hello world";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.from_addr.as_deref(), Some("alice@example.com"));
        assert_eq!(row.to_addrs.as_deref(), Some("bob@example.com"));
        assert_eq!(row.subject.as_deref(), Some("hi there"));
        assert_eq!(row.message_id.as_deref(), Some("<m1@example.com>"));
        // 2026-04-20T11:02:18Z → Unix ts.  Anchor on mailparse's result
        // since that's what the pipeline uses; just check the round-trip
        // lands on a timestamp that reformats to the same date string.
        let ts = row.date_unix.expect("date parsed");
        let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "2026-04-20T11:02:18Z");
        assert_eq!(row.body_text.as_deref(), Some("hello world"));
        assert!(!row.has_attachments);
    }

    #[test]
    fn multipart_alternative_prefers_plain() {
        let raw = b"From: a@x\r\n\
                    To: b@y\r\n\
                    Subject: s\r\n\
                    Message-ID: <m2@x>\r\n\
                    Content-Type: multipart/alternative; boundary=BOUND\r\n\
                    \r\n\
                    --BOUND\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    plain body here\r\n\
                    --BOUND\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <p>html body here</p>\r\n\
                    --BOUND--\r\n";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.body_text.as_deref(), Some("plain body here"));
    }

    #[test]
    fn html_only_strips_tags_and_entities() {
        let raw = b"From: a@x\r\n\
                    Subject: s\r\n\
                    Message-ID: <m3@x>\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\
                    \r\n\
                    <html><body><p>Twenty&#45;five &amp; counting</p>\
                    <script>alert(1)</script><style>.a{color:red}</style>\
                    <p>more&nbsp;text</p></body></html>";
        let row = parse_eml(raw, &meta()).unwrap();
        let body = row.body_text.unwrap();
        assert!(body.contains("Twenty-five & counting"), "got: {body:?}");
        assert!(body.contains("more text"), "got: {body:?}");
        assert!(!body.contains("alert"), "script body leaked: {body:?}");
        assert!(!body.contains("color:red"), "style body leaked: {body:?}");
    }

    #[test]
    fn quoted_printable_decodes() {
        let raw = b"From: a@x\r\n\
                    Subject: s\r\n\
                    Message-ID: <m4@x>\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    Content-Transfer-Encoding: quoted-printable\r\n\
                    \r\n\
                    hello=20=E2=98=83 snowman";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.body_text.as_deref(), Some("hello ☃ snowman"));
    }

    #[test]
    fn base64_decodes() {
        // "Base64 body\n" is aGVsbG8gYm9keQo= ... actually use the real text.
        // echo -n "base64 body" | base64 → YmFzZTY0IGJvZHk=
        let raw = b"From: a@x\r\n\
                    Subject: s\r\n\
                    Message-ID: <m5@x>\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    YmFzZTY0IGJvZHk=";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.body_text.as_deref(), Some("base64 body"));
    }

    #[test]
    fn mime_encoded_subject_decodes() {
        let raw = b"From: a@x\r\n\
                    Subject: =?UTF-8?B?SGVsbG8gV29ybGQg4pi4?=\r\n\
                    Message-ID: <m6@x>\r\n\
                    Content-Type: text/plain; charset=utf-8\r\n\
                    \r\n\
                    body";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.subject.as_deref(), Some("Hello World ☸"));
    }

    #[test]
    fn missing_date_falls_back_to_internal_date() {
        let raw = b"From: a@x\r\n\
                    Subject: s\r\n\
                    Message-ID: <m7@x>\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body";
        let m = MetaFacts {
            internal_date: Some("2024-01-02T03:04:05Z"),
            ..meta()
        };
        let row = parse_eml(raw, &m).unwrap();
        let ts = row.date_unix.expect("internal_date parsed");
        let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "2024-01-02T03:04:05Z");
    }

    #[test]
    fn missing_message_id_is_not_fatal() {
        // Upstream policy: callers (pull, rebuild) log and skip when
        // canonical_id can't be derived from Message-ID.  This parser
        // still returns a row — the empty message_id surfaces the
        // condition without crashing.
        let raw = b"From: a@x\r\n\
                    Subject: no message id\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body";
        let row = parse_eml(raw, &meta()).unwrap();
        assert!(row.message_id.is_none());
        assert_eq!(row.subject.as_deref(), Some("no message id"));
    }

    #[test]
    fn multipart_mixed_with_attachment_detected() {
        let raw = b"From: a@x\r\n\
                    Subject: s\r\n\
                    Message-ID: <m8@x>\r\n\
                    Content-Type: multipart/mixed; boundary=BOUND\r\n\
                    \r\n\
                    --BOUND\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body text\r\n\
                    --BOUND\r\n\
                    Content-Type: application/pdf; name=\"x.pdf\"\r\n\
                    Content-Disposition: attachment; filename=\"x.pdf\"\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    UERG\r\n\
                    --BOUND--\r\n";
        let row = parse_eml(raw, &meta()).unwrap();
        assert!(row.has_attachments);
        assert_eq!(row.body_text.as_deref(), Some("body text"));
    }

    #[test]
    fn cc_populated_when_present() {
        let raw = b"From: a@x.com\r\n\
                    To: b@y.com, c@z.com\r\n\
                    Cc: d@w.com\r\n\
                    Subject: s\r\n\
                    Message-ID: <m9@x>\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.to_addrs.as_deref(), Some("b@y.com, c@z.com"));
        assert_eq!(row.cc_addrs.as_deref(), Some("d@w.com"));
    }

    #[test]
    fn from_display_name_preserved() {
        let raw = b"From: Alice Smith <alice@example.com>\r\n\
                    Subject: s\r\n\
                    Message-ID: <m10@x>\r\n\
                    Content-Type: text/plain\r\n\
                    \r\n\
                    body";
        let row = parse_eml(raw, &meta()).unwrap();
        assert_eq!(row.from_addr.as_deref(), Some("alice@example.com"));
        assert_eq!(row.from_name.as_deref(), Some("Alice Smith"));
    }

    #[test]
    fn body_cap_enforced() {
        let big = "x".repeat(super::super::BODY_CAP_BYTES * 2);
        let mut raw = b"From: a@x\r\n\
                        Subject: s\r\n\
                        Message-ID: <m11@x>\r\n\
                        Content-Type: text/plain\r\n\
                        \r\n"
            .to_vec();
        raw.extend_from_slice(big.as_bytes());
        let row = parse_eml(&raw, &meta()).unwrap();
        assert!(row.body_text.unwrap().len() <= super::super::BODY_CAP_BYTES);
    }

    #[test]
    fn cap_bytes_respects_char_boundary() {
        // Snowman is 3 bytes in UTF-8.  Capping at 4 should yield one
        // snowman plus one 'a' (4 bytes) or just one snowman (3 bytes)
        // — never a split in the middle of the 3-byte sequence.
        let s = "a☃a☃a".to_string();
        let out = super::cap_bytes(s, 4);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.len() <= 4);
    }
}
