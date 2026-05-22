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
fn extract_components(ical_text: &str, name: &str) -> Vec<String> {
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
        let Some(ical_text) = ev.ical.as_deref() else {
            continue;
        };
        if ical_text.trim().is_empty() {
            continue;
        }
        for tz in extract_components(ical_text, "VTIMEZONE") {
            if let Some(id) = scan_field(&tz, "TZID") {
                tzs.entry(id).or_insert(tz);
            }
        }
        vevents.extend(extract_components(ical_text, "VEVENT"));
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
}
