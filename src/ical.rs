//! Shared iCalendar (RFC 5545) line-level helpers.
//!
//! pimsteward stores raw iCal payloads from upstream calendar providers
//! and surfaces a few derived fields (`SUMMARY`, `DTSTART`, `DTEND`,
//! `RRULE`, …) on `CalendarEvent`. These helpers do the cheap
//! line-grep extraction needed for that surface — they are **not** a
//! real iCalendar parser.
//!
//! Two properties matter for correctness:
//!
//! 1. **Scoped to the first `VEVENT`**. iCalendar payloads embed a
//!    `VTIMEZONE` block alongside the `VEVENT`, and that block has its
//!    own `DTSTART` / `RRULE` lines (one per `STANDARD`/`DAYLIGHT`
//!    subcomponent describing DST transitions). A flat line-grep over
//!    the whole payload is order-dependent: Apple iCloud puts `VEVENT`
//!    first so it works by accident, but Fastmail-style payloads put
//!    `VTIMEZONE` first and the unscoped grep returns timezone
//!    transition timestamps from 1895 instead of the event's real
//!    `DTSTART`. We track `BEGIN:VEVENT` / `END:VEVENT` and ignore
//!    everything outside.
//!
//! 2. **Parameter-aware**. iCal lets typed properties carry parameters
//!    after the name: `DTSTART;TZID=America/Toronto:20260305T180000`
//!    means "the value is `20260305T180000` in zone Toronto". Naive
//!    `starts_with("DTSTART:")` matching misses these — we split the
//!    head on `;` and compare the property name only.
//!
//! Lossy: parameters (`TZID`, `VALUE`, `LANGUAGE`, …) are dropped on
//! the floor. For `DTSTART;TZID=America/New_York:20270115T100000` the
//! returned value is the string `"20270115T100000"` and the timezone
//! is gone. Callers that need TZID fidelity must run a real iCalendar
//! parse (the `rrule` crate, for instance, is fed full iCal blocks
//! rather than the value-portion that comes out of here).

/// Unfold an iCalendar payload per RFC 5545 §3.1: a `CRLF` (or bare
/// `LF`) followed by a single space or tab is a line continuation and
/// must be stripped. `VEVENT` extraction relies on logical lines, so
/// we unfold up front and walk the result.
fn unfold(ical: &str) -> String {
    let mut out = String::with_capacity(ical.len());
    let mut chars = ical.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
            if matches!(chars.peek(), Some(' ') | Some('\t')) {
                chars.next();
                continue;
            }
            out.push('\r');
            out.push('\n');
        } else if c == '\n' {
            if matches!(chars.peek(), Some(' ') | Some('\t')) {
                chars.next();
                continue;
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a single iCal property line into (`name`, `value`). Returns
/// `None` if the line has no `:` (which makes it not a property).
/// Strips parameters: `DTSTART;TZID=America/Toronto:value` returns
/// `("DTSTART", "value")`.
fn split_property(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_end_matches('\r');
    let colon = line.find(':')?;
    let head = &line[..colon];
    let name = head.split(';').next().unwrap_or(head);
    Some((name, &line[colon + 1..]))
}

/// Iterator over `(name, value)` for every property line inside the
/// first `BEGIN:VEVENT…END:VEVENT` block of `ical`. Lines outside the
/// VEVENT (notably `VTIMEZONE` content) are skipped. The `BEGIN:` and
/// `END:` markers themselves are not yielded.
pub fn vevent_properties(ical: &str) -> impl Iterator<Item = (String, String)> {
    let unfolded = unfold(ical);
    // Collect into a Vec so the iterator can outlive `unfolded`. Iter
    // alternative would need a self-referential struct; not worth it.
    let mut out = Vec::new();
    let mut in_vevent = false;
    for line in unfolded.lines() {
        let upper = line.trim_end_matches('\r').to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            in_vevent = true;
            continue;
        }
        if upper == "END:VEVENT" {
            // Stop after the first VEVENT so RECURRENCE-ID overrides
            // (which appear as additional VEVENT siblings) don't pollute
            // the master event's field set. The override-handling layer
            // walks the whole payload separately when it cares.
            break;
        }
        if !in_vevent {
            continue;
        }
        if let Some((name, value)) = split_property(line) {
            out.push((name.to_string(), value.to_string()));
        }
    }
    out.into_iter()
}

/// Return the first value of property `name` inside the first
/// `VEVENT` block. Property name match is case-insensitive.
pub fn vevent_field(ical: &str, name: &str) -> Option<String> {
    vevent_properties(ical)
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// Return every value of property `name` inside the first `VEVENT`
/// block, in document order. Used for properties that legally appear
/// more than once — `EXDATE` is the canonical example (RFC 5545
/// §3.8.5.1, "MAY occur more than once" — Apple Calendar even emits
/// multiple `EXDATE` lines for distinct excluded occurrences instead
/// of using the comma-separated list form).
pub fn vevent_field_all(ical: &str, name: &str) -> Vec<String> {
    vevent_properties(ical)
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .collect()
}

/// Return every full property line inside the first `VEVENT` whose
/// property name (the bit before any `;` or `:`) matches `name`. The
/// returned strings preserve parameters: `DTSTART;TZID=America/Toronto:value`
/// comes back as the whole line, not just `value`.
///
/// This exists because the RRULE expander needs to feed entire
/// property lines to `rrule::RRuleSet::from_str`, which is itself a
/// real iCal parser and uses `TZID=` to anchor the recurrence in a
/// concrete zone. The lossy [`vevent_field`] would strip those
/// parameters and the expander would be left with floating local
/// times that can't be ordered against a UTC window.
pub fn vevent_raw_lines_named(ical: &str, name: &str) -> Vec<String> {
    let unfolded = unfold(ical);
    let upper = name.to_ascii_uppercase();
    let mut out = Vec::new();
    let mut in_vevent = false;
    for line in unfolded.lines() {
        let line = line.trim_end_matches('\r');
        let upper_line = line.to_ascii_uppercase();
        if upper_line == "BEGIN:VEVENT" {
            in_vevent = true;
            continue;
        }
        if upper_line == "END:VEVENT" {
            break;
        }
        if !in_vevent {
            continue;
        }
        let Some(colon) = line.find(':') else { continue };
        let head = &line[..colon];
        let prop_name = head.split(';').next().unwrap_or(head);
        if prop_name.eq_ignore_ascii_case(&upper) {
            out.push(line.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apple iCloud: VEVENT comes first. The flat extractor works on
    /// these by accident; the scoped extractor must too.
    #[test]
    fn vevent_first_layout_returns_vevent_dtstart() {
        let ical = "BEGIN:VCALENDAR\r\n\
                    BEGIN:VEVENT\r\n\
                    DTSTART;TZID=America/Toronto:20260305T180000\r\n\
                    DTEND;TZID=America/Toronto:20260305T190000\r\n\
                    SUMMARY:Erica Tutoring\r\n\
                    RRULE:FREQ=WEEKLY\r\n\
                    UID:abc-123\r\n\
                    END:VEVENT\r\n\
                    BEGIN:VTIMEZONE\r\n\
                    BEGIN:STANDARD\r\n\
                    DTSTART:18950101T000000\r\n\
                    RRULE:FREQ=YEARLY\r\n\
                    END:STANDARD\r\n\
                    END:VTIMEZONE\r\n\
                    END:VCALENDAR";
        assert_eq!(vevent_field(ical, "DTSTART").as_deref(), Some("20260305T180000"));
        assert_eq!(vevent_field(ical, "DTEND").as_deref(), Some("20260305T190000"));
        assert_eq!(vevent_field(ical, "SUMMARY").as_deref(), Some("Erica Tutoring"));
        assert_eq!(vevent_field(ical, "RRULE").as_deref(), Some("FREQ=WEEKLY"));
        assert_eq!(vevent_field(ical, "UID").as_deref(), Some("abc-123"));
    }

    /// Fastmail / forwardemail.net: VTIMEZONE comes first. The unscoped
    /// extractor returns 1895 timestamps and a yearly RRULE here —
    /// that's the bug. The scoped extractor must skip the VTIMEZONE
    /// block entirely.
    #[test]
    fn vtimezone_first_layout_skips_vtimezone_dtstart() {
        let ical = "BEGIN:VCALENDAR\r\n\
                    BEGIN:VTIMEZONE\r\n\
                    BEGIN:STANDARD\r\n\
                    DTSTART:18950101T000000\r\n\
                    RRULE:FREQ=YEARLY;UNTIL=19230513T070000Z;BYMONTH=5\r\n\
                    END:STANDARD\r\n\
                    BEGIN:DAYLIGHT\r\n\
                    DTSTART:19180414T020000\r\n\
                    END:DAYLIGHT\r\n\
                    END:VTIMEZONE\r\n\
                    BEGIN:VEVENT\r\n\
                    DTSTART;TZID=America/Toronto:20260214T131000\r\n\
                    DTEND;TZID=America/Toronto:20260214T133000\r\n\
                    SUMMARY:Rivian Key Drop-off\r\n\
                    UID:b28741c0\r\n\
                    END:VEVENT\r\n\
                    END:VCALENDAR";
        assert_eq!(vevent_field(ical, "DTSTART").as_deref(), Some("20260214T131000"));
        assert_eq!(vevent_field(ical, "DTEND").as_deref(), Some("20260214T133000"));
        assert_eq!(vevent_field(ical, "SUMMARY").as_deref(), Some("Rivian Key Drop-off"));
        // No RRULE on the VEVENT — the VTIMEZONE's RRULE must NOT bleed through.
        assert_eq!(vevent_field(ical, "RRULE"), None);
    }

    /// Property name match is case-insensitive (RFC 5545 §3.7 says
    /// names are case-insensitive on read, even though most clients
    /// uppercase on write).
    #[test]
    fn property_name_match_is_case_insensitive() {
        let ical = "BEGIN:VEVENT\nSummary:hello\nEND:VEVENT";
        assert_eq!(vevent_field(ical, "summary").as_deref(), Some("hello"));
        assert_eq!(vevent_field(ical, "SUMMARY").as_deref(), Some("hello"));
        assert_eq!(vevent_field(ical, "SuMmArY").as_deref(), Some("hello"));
    }

    /// Multiple `EXDATE` lines are common in Apple-emitted iCal —
    /// `vevent_field_all` returns every one in document order.
    #[test]
    fn exdate_multiple_lines_collected_in_order() {
        let ical = "BEGIN:VCALENDAR\n\
                    BEGIN:VEVENT\n\
                    DTSTART;TZID=America/Toronto:20230907T190000\n\
                    EXDATE;TZID=America/Toronto:20231221T190000\n\
                    EXDATE;TZID=America/Toronto:20231228T190000\n\
                    EXDATE;TZID=America/Toronto:20240104T190000\n\
                    RRULE:FREQ=WEEKLY\n\
                    END:VEVENT\n\
                    END:VCALENDAR";
        assert_eq!(
            vevent_field_all(ical, "EXDATE"),
            vec![
                "20231221T190000".to_string(),
                "20231228T190000".to_string(),
                "20240104T190000".to_string(),
            ]
        );
    }

    /// RFC 5545 §3.1 line folding: a CRLF (or bare LF) followed by a
    /// space or tab continues the previous logical line. Real iCal
    /// payloads from Apple Calendar fold long DESCRIPTION values at 75
    /// octets — extraction must collapse the fold before matching.
    #[test]
    fn folded_lines_are_unfolded_before_matching() {
        // The leading space immediately after each `\r\n` is the fold
        // continuation marker. Avoid a `\` line-continuation here — that
        // would eat the leading space and produce a non-folded fixture
        // that doesn't exercise the unfold path.
        let ical = concat!(
            "BEGIN:VEVENT\r\n",
            "DESCRIPTION:line one\r\n",
            " continues here\r\n",
            " and here\r\n",
            "UID:x\r\n",
            "END:VEVENT",
        );
        assert_eq!(
            vevent_field(ical, "DESCRIPTION").as_deref(),
            Some("line onecontinues hereand here"),
        );
    }

    /// A property that just isn't there returns None — used by the
    /// providers to leave the corresponding `CalendarEvent` field as
    /// `None` rather than fabricating a value.
    #[test]
    fn missing_property_returns_none() {
        let ical = "BEGIN:VEVENT\nUID:x\nEND:VEVENT";
        assert!(vevent_field(ical, "SUMMARY").is_none());
        assert!(vevent_field(ical, "DTSTART").is_none());
    }

    /// `vevent_raw_lines_named` preserves the parameter portion of
    /// each line so the RRULE expander can hand them to a real iCal
    /// parser. It must skip VTIMEZONE-block matches and ignore the
    /// VTIMEZONE's own RRULE (DST transition rules).
    #[test]
    fn raw_lines_named_preserves_params_and_skips_vtimezone() {
        let ical = concat!(
            "BEGIN:VCALENDAR\r\n",
            "BEGIN:VTIMEZONE\r\n",
            "BEGIN:STANDARD\r\n",
            "DTSTART:18950101T000000\r\n",
            "RRULE:FREQ=YEARLY;UNTIL=19230513T070000Z\r\n",
            "END:STANDARD\r\n",
            "END:VTIMEZONE\r\n",
            "BEGIN:VEVENT\r\n",
            "DTSTART;TZID=America/Toronto:20260305T180000\r\n",
            "DTEND;TZID=America/Toronto:20260305T190000\r\n",
            "RRULE:FREQ=WEEKLY\r\n",
            "EXDATE;TZID=America/Toronto:20260326T180000\r\n",
            "EXDATE;TZID=America/Toronto:20260402T180000\r\n",
            "SUMMARY:Erica Tutoring\r\n",
            "END:VEVENT\r\n",
            "END:VCALENDAR\r\n",
        );
        // DTSTART line keeps `TZID=America/Toronto`. VTIMEZONE's
        // `DTSTART:18950101T000000` is excluded.
        assert_eq!(
            vevent_raw_lines_named(ical, "DTSTART"),
            vec!["DTSTART;TZID=America/Toronto:20260305T180000".to_string()],
        );
        // VEVENT-level RRULE only — no VTIMEZONE STANDARD rule.
        assert_eq!(
            vevent_raw_lines_named(ical, "RRULE"),
            vec!["RRULE:FREQ=WEEKLY".to_string()],
        );
        // Multiple EXDATEs in document order.
        assert_eq!(
            vevent_raw_lines_named(ical, "EXDATE"),
            vec![
                "EXDATE;TZID=America/Toronto:20260326T180000".to_string(),
                "EXDATE;TZID=America/Toronto:20260402T180000".to_string(),
            ],
        );
    }

    /// RECURRENCE-ID overrides appear as additional VEVENT blocks with
    /// the same UID. We extract from the FIRST VEVENT only — the
    /// override-handling layer (RRULE expansion in mcp::server) walks
    /// the full payload separately. Without this stop, the override's
    /// fields would silently shadow the master.
    #[test]
    fn first_vevent_wins_over_recurrence_id_overrides() {
        let ical = "BEGIN:VCALENDAR\n\
                    BEGIN:VEVENT\n\
                    DTSTART:20260305T180000\n\
                    SUMMARY:Master\n\
                    UID:x\n\
                    END:VEVENT\n\
                    BEGIN:VEVENT\n\
                    DTSTART:20260326T200000\n\
                    SUMMARY:Override-only summary\n\
                    UID:x\n\
                    RECURRENCE-ID:20260326T180000\n\
                    END:VEVENT\n\
                    END:VCALENDAR";
        assert_eq!(vevent_field(ical, "SUMMARY").as_deref(), Some("Master"));
        assert_eq!(vevent_field(ical, "DTSTART").as_deref(), Some("20260305T180000"));
    }
}
