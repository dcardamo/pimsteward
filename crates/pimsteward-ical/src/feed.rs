//! Calendar feed assembly — pure functions that trim event history and
//! merge many single-event `VCALENDAR`s into one subscribable feed.
//!
//! pimsteward stores each event as a single-event `VCALENDAR` (carrying
//! its own `VTIMEZONE`). To publish a `.ics` feed a host-side builder
//! fetches a calendar's events and folds them into one `VCALENDAR`:
//! `VEVENT` blocks are preserved as unfolded logical lines (RFC 5545 fold
//! continuations are removed; clients accept long unfolded lines), so
//! `RRULE` and `VALARM` survive for the client to expand/alert on. The
//! repeated `VTIMEZONE` blocks are collapsed to one per `TZID`.
//!
//! Everything here is pure: no network, storage, or credentials. The
//! cutoff-based trimming keeps recurring masters forever (the client
//! expands the series) and drops only stale one-off events.

use crate::ical::{self, unfold};
use crate::CalendarEvent;
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::BTreeMap;

/// Extract each top-level `BEGIN:<name> … END:<name>` block (inclusive),
/// CRLF-joined as unfolded logical lines (RFC 5545 fold continuations are
/// removed; clients accept long unfolded lines). Inner sub-components
/// (VALARM, STANDARD, DAYLIGHT) use distinct END tokens and are passed
/// through as content, so a flat scan over unfolded logical lines
/// correctly captures the whole block — nested children come along.
pub(crate) fn extract_components(ical_text: &str, name: &str) -> Vec<String> {
    let begin = format!("BEGIN:{name}");
    let end = format!("END:{name}");
    let mut out = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for raw in unfold(ical_text).lines() {
        let line = raw.trim_end_matches('\r');
        if line == begin {
            cur = Some(vec![line.to_string()]);
        } else if line == end {
            if let Some(mut block) = cur.take() {
                block.push(line.to_string());
                out.push(block.join("\r\n") + "\r\n");
            }
        } else if let Some(b) = cur.as_mut() {
            b.push(line.to_string());
        }
    }
    out
}

/// Windows/Exchange zone name → IANA. Exchange-origin calendars emit
/// Windows TZIDs ("Eastern Standard Time") which strict RFC 5545 clients
/// reject; map the common ones to IANA so the published feed is portable.
/// Source: CLDR windowsZones, primary ("001") territory mapping.
fn windows_to_iana(win: &str) -> Option<&'static str> {
    Some(match win {
        // North America (the zones this calendar actually uses + neighbours)
        "Dateline Standard Time" => "Etc/GMT+12",
        "Aleutian Standard Time" => "America/Adak",
        "Hawaiian Standard Time" => "Pacific/Honolulu",
        "Alaskan Standard Time" => "America/Anchorage",
        "Pacific Standard Time (Mexico)" => "America/Tijuana",
        "Pacific Standard Time" => "America/Los_Angeles",
        "US Mountain Standard Time" => "America/Phoenix",
        "Mountain Standard Time (Mexico)" => "America/Mazatlan",
        "Mountain Standard Time" => "America/Denver",
        "Central America Standard Time" => "America/Guatemala",
        "Central Standard Time (Mexico)" => "America/Mexico_City",
        "Central Standard Time" => "America/Chicago",
        "Canada Central Standard Time" => "America/Regina",
        "SA Pacific Standard Time" => "America/Bogota",
        "Eastern Standard Time (Mexico)" => "America/Cancun",
        "US Eastern Standard Time" => "America/Indiana/Indianapolis",
        "Eastern Standard Time" => "America/New_York",
        "Venezuela Standard Time" => "America/Caracas",
        "Atlantic Standard Time" => "America/Halifax",
        "Newfoundland Standard Time" => "America/St_Johns",
        "SA Eastern Standard Time" => "America/Cayenne",
        "Argentina Standard Time" => "America/Buenos_Aires",
        "E. South America Standard Time" => "America/Sao_Paulo",
        "Greenland Standard Time" => "America/Godthab",
        "Pacific SA Standard Time" => "America/Santiago",
        "SA Western Standard Time" => "America/La_Paz",
        // Atlantic / Europe / Africa
        "Azores Standard Time" => "Atlantic/Azores",
        "Cape Verde Standard Time" => "Atlantic/Cape_Verde",
        "UTC" => "Etc/UTC",
        "GMT Standard Time" => "Europe/London",
        "Greenwich Standard Time" => "Atlantic/Reykjavik",
        "W. Europe Standard Time" => "Europe/Berlin",
        "Central Europe Standard Time" => "Europe/Budapest",
        "Romance Standard Time" => "Europe/Paris",
        "Central European Standard Time" => "Europe/Warsaw",
        "W. Central Africa Standard Time" => "Africa/Lagos",
        "GTB Standard Time" => "Europe/Bucharest",
        "FLE Standard Time" => "Europe/Kiev",
        "E. Europe Standard Time" => "Europe/Chisinau",
        "Egypt Standard Time" => "Africa/Cairo",
        "South Africa Standard Time" => "Africa/Johannesburg",
        "Israel Standard Time" => "Asia/Jerusalem",
        "Russian Standard Time" => "Europe/Moscow",
        "E. Africa Standard Time" => "Africa/Nairobi",
        // Asia / Pacific
        "Arabic Standard Time" => "Asia/Baghdad",
        "Arab Standard Time" => "Asia/Riyadh",
        "Iran Standard Time" => "Asia/Tehran",
        "Arabian Standard Time" => "Asia/Dubai",
        "Azerbaijan Standard Time" => "Asia/Baku",
        "India Standard Time" => "Asia/Kolkata",
        "Nepal Standard Time" => "Asia/Kathmandu",
        "Central Asia Standard Time" => "Asia/Almaty",
        "Bangladesh Standard Time" => "Asia/Dhaka",
        "Myanmar Standard Time" => "Asia/Yangon",
        "SE Asia Standard Time" => "Asia/Bangkok",
        "China Standard Time" => "Asia/Shanghai",
        "Singapore Standard Time" => "Asia/Singapore",
        "Taipei Standard Time" => "Asia/Taipei",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "Korea Standard Time" => "Asia/Seoul",
        "W. Australia Standard Time" => "Australia/Perth",
        "Cen. Australia Standard Time" => "Australia/Adelaide",
        "AUS Central Standard Time" => "Australia/Darwin",
        "E. Australia Standard Time" => "Australia/Brisbane",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        "Tasmania Standard Time" => "Australia/Hobart",
        "New Zealand Standard Time" => "Pacific/Auckland",
        _ => return None,
    })
}

/// Rewrite a single logical line's TZID (property `TZID:` or parameter
/// `;TZID=`) from a Windows zone name to IANA. Preserves any trailing
/// CRLF/LF. Lines without a known Windows TZID are returned unchanged.
fn rewrite_tzid_line(line: &str) -> String {
    let core = line.trim_end_matches(['\r', '\n']);
    let suffix = &line[core.len()..];
    // VTIMEZONE property form: the whole value after `TZID:`
    if let Some(val) = core.strip_prefix("TZID:") {
        if let Some(iana) = windows_to_iana(val) {
            return format!("TZID:{iana}{suffix}");
        }
        return line.to_string();
    }
    // Parameter form: `...;TZID=<value>` where value ends at `;` or `:`.
    // (Windows zone names contain spaces but never `;`/`:`.) Only the
    // property HEAD (before the first `:`) holds parameters — restrict the
    // search there so a literal `;TZID=` inside a TEXT value (e.g. a
    // DESCRIPTION) is never rewritten. Exchange emits TZID uppercase and
    // unquoted, which is what this matches.
    let head_end = core.find(':').unwrap_or(core.len());
    if let Some(pos) = core[..head_end].find(";TZID=") {
        let vstart = pos + ";TZID=".len();
        let rel_end = core[vstart..head_end]
            .find(';')
            .map(|i| vstart + i)
            .unwrap_or(head_end);
        if let Some(iana) = windows_to_iana(&core[vstart..rel_end]) {
            return format!("{}{}{}{}", &core[..vstart], iana, &core[rel_end..], suffix);
        }
    }
    line.to_string()
}

/// Rewrite all Windows TZIDs in an iCal payload to IANA. Operates on
/// unfolded logical lines (so a TZID value isn't split across a fold).
fn rewrite_windows_tzids(ical: &str) -> String {
    let unfolded = unfold(ical);
    unfolded
        .split_inclusive('\n')
        .map(rewrite_tzid_line)
        .collect()
}

/// Plain (non-VEVENT-scoped) single-field value scan, params stripped.
/// Used for `VTIMEZONE`'s `TZID`, which lives outside any `VEVENT` and so
/// can't go through the VEVENT-scoped helpers in [`crate::ical`].
fn scan_field(block: &str, name: &str) -> Option<String> {
    for raw in unfold(block).lines() {
        let line = raw.trim_end_matches('\r');
        let head = line.split(':').next().unwrap_or("");
        let prop = head.split(';').next().unwrap_or("");
        if prop.eq_ignore_ascii_case(name) {
            return line.split_once(':').map(|(_, v)| v.to_string());
        }
    }
    None
}

/// Cutoff comparison parses only the YYYYMMDD date head — intra-day
/// precision is irrelevant at a 365-day granularity, and this handles
/// DATE, DATE-TIME, and `…Z` forms uniformly.
fn dtstart_date(ev: &CalendarEvent) -> Option<NaiveDate> {
    let ical_text = ev.ical.as_deref()?;
    let v = ical::vevent_field(ical_text, "DTSTART")?;
    let head: String = v.chars().take(8).collect();
    NaiveDate::parse_from_str(&head, "%Y%m%d").ok()
}

/// Decide whether an event belongs in the trimmed feed.
///
/// Recurring masters (any `RRULE` on the VEVENT) are kept regardless of
/// `DTSTART` age — the client expands the series forward, so dropping an
/// old master would silently delete all of its future occurrences.
/// One-off events are kept only if their `DTSTART` is on/after `cutoff`.
/// Missing or unparseable `DTSTART` (and a missing `ical`) fail open and
/// are kept, so a parsing quirk never silently drops a real event.
pub fn keep_for_feed(ev: &CalendarEvent, cutoff: DateTime<Utc>) -> bool {
    let Some(ical_text) = ev.ical.as_deref() else {
        return true;
    };
    if !ical::vevent_raw_lines_named(ical_text, "RRULE").is_empty() {
        return true; // recurring series — keep regardless of age
    }
    match dtstart_date(ev) {
        Some(d) => d.and_hms_opt(0, 0, 0).unwrap().and_utc() >= cutoff,
        None => true, // fail-open
    }
}

/// Merge per-event single-event `VCALENDAR`s into one feed `VCALENDAR`.
///
/// Every `VEVENT` (with any nested `VALARM`) is preserved as unfolded
/// logical lines (RFC 5545 fold continuations are removed; clients accept
/// long unfolded lines). A single stored payload may contain more than one
/// `VEVENT` (a recurring-series master plus `RECURRENCE-ID` overrides) —
/// all of them are collected. `VTIMEZONE` blocks are de-duplicated by
/// `TZID` — real stored events each carry a multi-decade
/// `America/Toronto` definition, so without this the feed would repeat
/// thousands of identical copies. Events with a missing or empty `ical`
/// are skipped.
pub fn merge_calendar(events: &[&CalendarEvent], cal_name: &str, prodid: &str) -> String {
    let mut tzs: BTreeMap<String, String> = BTreeMap::new();
    let mut vevents: Vec<String> = Vec::new();
    for ev in events {
        let Some(raw) = ev.ical.as_deref() else {
            continue;
        };
        if raw.trim().is_empty() {
            continue;
        }
        // Rewrite Windows/Exchange TZIDs to IANA before extraction so the
        // dedup key (the VTIMEZONE TZID) collapses Windows and IANA spellings
        // of the same zone into one block.
        let ical_text = rewrite_windows_tzids(raw);
        for tz in extract_components(&ical_text, "VTIMEZONE") {
            if let Some(id) = scan_field(&tz, "TZID") {
                tzs.entry(id).or_insert(tz);
            }
        }
        vevents.extend(extract_components(&ical_text, "VEVENT"));
    }
    let mut out = String::new();
    out.push_str("BEGIN:VCALENDAR\r\n");
    out.push_str("VERSION:2.0\r\n");
    out.push_str(&format!("PRODID:{prodid}\r\n"));
    out.push_str("CALSCALE:GREGORIAN\r\n");
    out.push_str("METHOD:PUBLISH\r\n");
    out.push_str(&format!("X-WR-CALNAME:{cal_name}\r\n"));
    for tz in tzs.values() {
        out.push_str(tz);
    }
    for ve in &vevents {
        out.push_str(ve);
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

/// Build a subscribable feed: trim history with [`keep_for_feed`] then
/// fold the survivors into one `VCALENDAR` with [`merge_calendar`].
pub fn build_feed(
    events: &[CalendarEvent],
    cutoff: DateTime<Utc>,
    cal_name: &str,
    prodid: &str,
) -> String {
    let kept: Vec<&CalendarEvent> = events.iter().filter(|e| keep_for_feed(e, cutoff)).collect();
    merge_calendar(&kept, cal_name, prodid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CalendarEvent;
    use chrono::{TimeZone, Utc};

    fn ev(ical: &str) -> CalendarEvent {
        CalendarEvent {
            ical: Some(ical.to_string()),
            ..Default::default()
        }
    }

    const TORONTO_TZ: &str = "BEGIN:VTIMEZONE\r\nTZID:America/Toronto\r\nEND:VTIMEZONE\r\n";

    fn single(uid: &str, dtstart: &str, extra: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//EN\r\n{tz}BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:{dt}\r\nSUMMARY:{uid}\r\n{extra}END:VEVENT\r\nEND:VCALENDAR\r\n",
            tz = TORONTO_TZ,
            uid = uid,
            dt = dtstart,
            extra = extra,
        )
    }

    /// Build a single-event VCALENDAR whose VTIMEZONE carries `tzid` as its
    /// `TZID:` property AND whose DTSTART carries it as a `;TZID=` parameter.
    /// The VTIMEZONE keeps a STANDARD sub-component so we can assert the
    /// embedded rules survive (only the label is rewritten).
    fn single_tzid(uid: &str, tzid: &str, dtstart: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//EN\r\n\
             BEGIN:VTIMEZONE\r\nTZID:{tzid}\r\n\
             BEGIN:STANDARD\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\n\
             END:VTIMEZONE\r\n\
             BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART;TZID={tzid}:{dt}\r\nSUMMARY:{uid}\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n",
            tzid = tzid,
            uid = uid,
            dt = dtstart,
        )
    }

    #[test]
    fn tzid_inside_a_text_value_is_not_rewritten() {
        // A literal ";TZID=<zone>:" inside a DESCRIPTION value must be left
        // alone — only the property head (before the first `:`) holds real
        // TZID parameters.
        let a = ev(&single(
            "d",
            "20260601T120000",
            "DESCRIPTION:re ;TZID=Eastern Standard Time:see below\r\n",
        ));
        let refs = vec![&a];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert!(
            out.contains("DESCRIPTION:re ;TZID=Eastern Standard Time:see below"),
            "TEXT value must not be rewritten:\n{out}"
        );
    }

    #[test]
    fn merge_collects_all_vevents_and_dedupes_vtimezone() {
        let a = ev(&single("a", "20260601T120000", ""));
        let b = ev(&single("b", "20260602T120000", ""));
        let refs = vec![&a, &b];
        let out = merge_calendar(&refs, "Dan", "-//hld.ca//ics-feed//EN");
        assert_eq!(out.matches("BEGIN:VEVENT").count(), 2);
        assert_eq!(out.matches("BEGIN:VTIMEZONE").count(), 1, "TZID deduped");
        assert!(out.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(out.trim_end().ends_with("END:VCALENDAR"));
        assert!(out.contains("UID:a") && out.contains("UID:b"));
    }

    #[test]
    fn merge_preserves_rrule_and_valarm() {
        let a = ev(&single(
            "r",
            "20210101T090000",
            "RRULE:FREQ=WEEKLY;BYDAY=MO\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\n",
        ));
        let refs = vec![&a];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert!(out.contains("RRULE:FREQ=WEEKLY;BYDAY=MO"));
        assert!(out.contains("BEGIN:VALARM") && out.contains("END:VALARM"));
        assert_eq!(out.matches("END:VEVENT").count(), 1);
    }

    #[test]
    fn merge_skips_events_without_ical() {
        let a = CalendarEvent {
            ical: None,
            ..Default::default()
        };
        let b = ev(&single("b", "20260602T120000", ""));
        let refs = vec![&a, &b];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert_eq!(out.matches("BEGIN:VEVENT").count(), 1);
    }

    #[test]
    fn keep_for_feed_rules() {
        let cutoff = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let recurring = ev(&single("r", "20210101T090000", "RRULE:FREQ=WEEKLY\r\n"));
        assert!(keep_for_feed(&recurring, cutoff));
        let old = ev(&single("o", "20240101T090000", ""));
        assert!(!keep_for_feed(&old, cutoff));
        let recent = ev(&single("n", "20260101T090000", ""));
        assert!(keep_for_feed(&recent, cutoff));
        let allday = ev(&single("d", "20260115", ""));
        assert!(keep_for_feed(&allday, cutoff));
        let weird = ev(&single("w", "garbage", ""));
        assert!(keep_for_feed(&weird, cutoff));
        let none = CalendarEvent {
            ical: None,
            ..Default::default()
        };
        assert!(keep_for_feed(&none, cutoff));
    }

    #[test]
    fn build_feed_filters_then_merges() {
        let cutoff = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let old = ev(&single("o", "20240101T090000", ""));
        let recent = ev(&single("n", "20260101T090000", ""));
        let events = vec![old, recent];
        let out = build_feed(&events, cutoff, "Dan", "-//x//EN");
        assert!(out.contains("UID:n"));
        assert!(!out.contains("UID:o"));
    }

    /// A single stored `.ics` payload that contains a master VEVENT (with
    /// RRULE) plus a RECURRENCE-ID override VEVENT (same UID) — both must
    /// appear in the merged feed. `keep_for_feed` must keep the whole
    /// payload because the FIRST VEVENT carries the RRULE.
    #[test]
    fn multi_vevent_payload_master_and_override_both_emitted() {
        // Build a single VCALENDAR with one VTIMEZONE, one master VEVENT
        // (with RRULE), and one override VEVENT (with RECURRENCE-ID).
        let payload = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "PRODID:-//x//EN\r\n",
            "BEGIN:VTIMEZONE\r\n",
            "TZID:America/Toronto\r\n",
            "END:VTIMEZONE\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:series-1\r\n",
            "DTSTART:20260101T090000\r\n",
            "RRULE:FREQ=WEEKLY\r\n",
            "SUMMARY:Weekly\r\n",
            "END:VEVENT\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:series-1\r\n",
            "RECURRENCE-ID:20260108T090000\r\n",
            "DTSTART:20260108T100000\r\n",
            "SUMMARY:Weekly (moved)\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );
        let e = ev(payload);

        // keep_for_feed inspects the FIRST VEVENT only (master with RRULE)
        // and must return true — the override VEVENTs must not be confused
        // with separate non-recurring events.
        let cutoff = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        assert!(
            keep_for_feed(&e, cutoff),
            "master-first multi-VEVENT payload must be kept (RRULE on master)"
        );

        // merge_calendar must emit BOTH VEVENTs and deduplicate the VTIMEZONE.
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert_eq!(
            out.matches("BEGIN:VEVENT").count(),
            2,
            "both master and override VEVENT must appear in merged output"
        );
        assert!(
            out.contains("RECURRENCE-ID:20260108T090000"),
            "RECURRENCE-ID line must survive in merged output"
        );
        assert_eq!(
            out.matches("BEGIN:VTIMEZONE").count(),
            1,
            "shared VTIMEZONE must appear exactly once"
        );
    }

    /// RFC 5545 §3.1 fold continuations are stripped by `extract_components`
    /// (it runs `unfold()` before collecting lines). A DESCRIPTION folded
    /// across two physical lines must appear as one long unfolded line in
    /// the merged feed — clients accept long unfolded lines.
    #[test]
    fn folded_description_is_unfolded_in_merged_output() {
        // Physical fold: `\r\n ` is the continuation marker per RFC 5545.
        // The space is part of the fold, NOT part of the value — unfold()
        // strips both the CRLF and the leading space of the next line.
        let folded_payload = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "PRODID:-//x//EN\r\n",
            "BEGIN:VTIMEZONE\r\n",
            "TZID:America/Toronto\r\n",
            "END:VTIMEZONE\r\n",
            "BEGIN:VEVENT\r\n",
            "UID:fold-test\r\n",
            "DTSTART:20260601T090000\r\n",
            "SUMMARY:Fold test\r\n",
            "DESCRIPTION:first part\r\n",
            " second part\r\n",      // RFC 5545 fold continuation
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );
        let e = ev(folded_payload);
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        // The two physical lines collapse to one unfolded logical line.
        assert!(
            out.contains("DESCRIPTION:first partsecond part"),
            "folded DESCRIPTION must be unfolded in merged output (got: {:?})",
            out.lines()
                .find(|l| l.contains("DESCRIPTION"))
                .unwrap_or("<not found>"),
        );
        // The fold marker (bare leading space on a continuation line) must
        // not appear as a separate line in the output.
        assert!(
            !out.contains("\r\n second part"),
            "fold continuation must not survive as a physical-fold line"
        );
    }

    /// Exchange-origin events carry Windows TZIDs in BOTH the VTIMEZONE
    /// `TZID:` property and the `;TZID=` parameter on DTSTART. The merged
    /// feed must rewrite both forms to IANA and leave no Windows name behind.
    #[test]
    fn windows_tzid_property_and_param_rewritten_to_iana() {
        let e = ev(&single_tzid(
            "exch-1",
            "Eastern Standard Time",
            "20260305T180000",
        ));
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        // Property form (VTIMEZONE) rewritten.
        assert!(
            out.contains("TZID:America/New_York"),
            "VTIMEZONE TZID property must be rewritten to IANA"
        );
        // Parameter form (DTSTART) rewritten.
        assert!(
            out.contains("DTSTART;TZID=America/New_York:20260305T180000"),
            "DTSTART ;TZID= parameter must be rewritten to IANA"
        );
        // No Windows spelling survives anywhere.
        assert!(
            !out.contains("Eastern Standard Time"),
            "no Windows zone name must remain in the merged output"
        );
        // Embedded STANDARD rules survive — only the label changed.
        assert!(
            out.contains("BEGIN:STANDARD") && out.contains("TZOFFSETTO:-0500"),
            "embedded VTIMEZONE rules must be preserved"
        );
    }

    #[test]
    fn newfoundland_maps_to_st_johns() {
        let e = ev(&single_tzid(
            "nf-1",
            "Newfoundland Standard Time",
            "20260305T180000",
        ));
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert!(out.contains("TZID:America/St_Johns"));
        assert!(out.contains("DTSTART;TZID=America/St_Johns:20260305T180000"));
        assert!(!out.contains("Newfoundland Standard Time"));
    }

    #[test]
    fn iana_tzid_left_unchanged() {
        let e = ev(&single_tzid("iana-1", "America/Toronto", "20260305T180000"));
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert!(out.contains("TZID:America/Toronto"));
        assert!(out.contains("DTSTART;TZID=America/Toronto:20260305T180000"));
        // Present exactly once (one VTIMEZONE, one DTSTART param).
        assert_eq!(out.matches("TZID:America/Toronto").count(), 1);
        assert_eq!(out.matches("BEGIN:VTIMEZONE").count(), 1);
    }

    #[test]
    fn unknown_zone_left_unchanged() {
        let e = ev(&single_tzid(
            "narnia-1",
            "Narnia Standard Time",
            "20260305T180000",
        ));
        let refs = vec![&e];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        // Unknown name passes through verbatim — no panic, both forms intact.
        assert!(out.contains("TZID:Narnia Standard Time"));
        assert!(out.contains("DTSTART;TZID=Narnia Standard Time:20260305T180000"));
    }

    /// An event using `Eastern Standard Time` and another using the IANA
    /// `America/New_York` are the same zone — after rewrite they share a
    /// TZID and dedupe to ONE VTIMEZONE while both VEVENTs survive.
    #[test]
    fn windows_and_iana_same_zone_dedupe_to_one() {
        let win = ev(&single_tzid(
            "win-1",
            "Eastern Standard Time",
            "20260305T180000",
        ));
        let iana = ev(&single_tzid("iana-1", "America/New_York", "20260306T180000"));
        let refs = vec![&win, &iana];
        let out = merge_calendar(&refs, "Dan", "-//x//EN");
        assert_eq!(
            out.matches("BEGIN:VTIMEZONE").count(),
            1,
            "Windows + IANA spellings of the same zone must collapse to one VTIMEZONE"
        );
        assert_eq!(
            out.matches("BEGIN:VEVENT").count(),
            2,
            "both events must survive the merge"
        );
        assert!(
            !out.contains("Eastern Standard Time"),
            "no Windows zone name must remain after rewrite"
        );
        assert!(out.contains("TZID:America/New_York"));
    }
}
