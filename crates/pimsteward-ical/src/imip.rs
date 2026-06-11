//! Pure iMIP (iTIP-over-email) payload construction. No network or storage —
//! given a stored `.ics`, produce a METHOD:REQUEST / METHOD:CANCEL VCALENDAR
//! suitable for a `text/calendar` MIME part.

use crate::feed::extract_components;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcalAddress {
    pub email: String,
    pub cn: Option<String>,
}

/// Parse the `EMAIL=` param of a raw property line, falling back to a
/// `mailto:` value after the colon. Returns lowercased address.
fn address_of(line: &str) -> Option<String> {
    for part in line.split([';', ':']) {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("EMAIL=").or_else(|| p.strip_prefix("email=")) {
            return Some(rest.trim().to_ascii_lowercase());
        }
    }
    let val = line.split_once(':').map(|(_, v)| v).unwrap_or("");
    val.to_ascii_lowercase()
        .strip_prefix("mailto:")
        .map(|s| s.trim().to_string())
}

fn cn_of(line: &str) -> Option<String> {
    for part in line.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("CN=").or_else(|| p.strip_prefix("cn=")) {
            let v = rest.split(':').next().unwrap_or(rest).trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn raw_lines(ics: &str, name: &str) -> Vec<String> {
    crate::ical::vevent_raw_lines_named(ics, name)
}

pub fn organizer(ics: &str) -> Option<IcalAddress> {
    let line = raw_lines(ics, "ORGANIZER").into_iter().next()?;
    let email = address_of(&line)?;
    if email.is_empty() {
        return None;
    }
    Some(IcalAddress {
        email,
        cn: cn_of(&line),
    })
}

pub fn attendees(ics: &str) -> Vec<IcalAddress> {
    raw_lines(ics, "ATTENDEE")
        .iter()
        .filter_map(|l| {
            let email = address_of(l)?;
            if email.is_empty() {
                None
            } else {
                Some(IcalAddress {
                    email,
                    cn: cn_of(l),
                })
            }
        })
        .collect()
}

/// Build a METHOD:REQUEST or METHOD:CANCEL VCALENDAR payload from a stored
/// `.ics`. `method` is "REQUEST" or "CANCEL". Organizer/attendees are
/// rewritten to `mailto:` form; SEQUENCE is forced to `sequence`; a fresh
/// DTSTAMP is added. VTIMEZONE blocks from the source are preserved.
pub fn build_imip(
    ics: &str,
    method: &str,
    sequence: u32,
    organizer_email: &str,
    attendee_emails: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str("PRODID:-//pimsteward//scheduling//EN\r\n");
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str(&format!("METHOD:{method}\r\n"));

    for tz in extract_components(ics, "VTIMEZONE") {
        out.push_str(&tz);
    }

    let vevent = extract_components(ics, "VEVENT")
        .into_iter()
        .next()
        .unwrap_or_default();
    out.push_str("BEGIN:VEVENT\r\n");
    let mut wrote_dtstamp = false;
    let mut wrote_seq = false;
    for line in crate::ical::unfold(&vevent).lines() {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("BEGIN:VEVENT") || upper.starts_with("END:VEVENT") {
            continue;
        }
        if upper.starts_with("ORGANIZER") {
            out.push_str(&format!("ORGANIZER:mailto:{organizer_email}\r\n"));
            continue;
        }
        if upper.starts_with("ATTENDEE") {
            continue;
        }
        if upper.starts_with("SEQUENCE") {
            out.push_str(&format!("SEQUENCE:{sequence}\r\n"));
            wrote_seq = true;
            continue;
        }
        if upper.starts_with("DTSTAMP") {
            out.push_str(&format!("DTSTAMP:{}\r\n", dtstamp_now()));
            wrote_dtstamp = true;
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !wrote_seq {
        out.push_str(&format!("SEQUENCE:{sequence}\r\n"));
    }
    if !wrote_dtstamp {
        out.push_str(&format!("DTSTAMP:{}\r\n", dtstamp_now()));
    }
    for email in attendee_emails {
        out.push_str(&format!("ATTENDEE;RSVP=TRUE:mailto:{email}\r\n"));
    }
    out.push_str("END:VEVENT\r\n");
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn dtstamp_now() -> String {
    use chrono::Utc;
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//forwardemail.net//caldav//EN\r\nBEGIN:VEVENT\r\nUID:evt-1\r\nSEQUENCE:0\r\nDTSTART;TZID=America/Toronto:20260701T190000\r\nDTEND;TZID=America/Toronto:20260701T200000\r\nSUMMARY:Dinner\r\nLOCATION:Craft\r\nORGANIZER;CN=Dan Cardamore;EMAIL=dan@hld.ca:/aMTc5opaque\r\nATTENDEE;CN=Heather;CUTYPE=INDIVIDUAL;EMAIL=heather@hld.ca;PARTSTAT=NEEDS-ACTION:/aZZZ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn reads_organizer_email() {
        assert_eq!(organizer(SAMPLE).unwrap().email, "dan@hld.ca");
    }

    #[test]
    fn reads_attendee_emails() {
        let a = attendees(SAMPLE);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].email, "heather@hld.ca");
    }

    #[test]
    fn request_payload_normalizes_addresses_and_sets_sequence() {
        let out = build_imip(SAMPLE, "REQUEST", 2, "dan@hld.ca", &["heather@hld.ca".into()]);
        assert!(out.contains("METHOD:REQUEST\r\n"));
        assert!(out.contains("ORGANIZER:mailto:dan@hld.ca\r\n"));
        assert!(out.contains("ATTENDEE;RSVP=TRUE:mailto:heather@hld.ca\r\n"));
        assert!(out.contains("SEQUENCE:2\r\n"));
        assert!(out.contains("DTSTAMP:"));
        assert!(out.contains("DTSTART;TZID=America/Toronto:20260701T190000\r\n"));
        assert!(out.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(out.trim_end().ends_with("END:VCALENDAR"));
    }

    #[test]
    fn cancel_payload_uses_cancel_method() {
        let out = build_imip(SAMPLE, "CANCEL", 3, "dan@hld.ca", &["heather@hld.ca".into()]);
        assert!(out.contains("METHOD:CANCEL\r\n"));
        assert!(out.contains("SEQUENCE:3\r\n"));
    }
}
