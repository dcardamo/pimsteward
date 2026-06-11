//! Policy layer: decide what iMIP messages a single calendar change implies.

use crate::scheduling::model::{ChangeKind, EventChange, Method, Outbound};
use crate::scheduling::watermark::is_past_start;
use chrono::{DateTime, Utc};
use pimsteward_ical::imip;

pub(crate) const SIGNIFICANT: &[&str] = &[
    "DTSTART", "DTEND", "DURATION", "RRULE", "RDATE", "EXDATE",
    "RECURRENCE-ID", "SUMMARY", "LOCATION", "DESCRIPTION",
];

/// A stable fingerprint of the scheduling-significant fields of an event,
/// used to dedup REQUESTs by content rather than by SEQUENCE (clients don't
/// always bump SEQUENCE on a significant edit).
pub(crate) fn significant_fingerprint(ics: &str) -> String {
    use pimsteward_ical::ical::vevent_field_all;
    let mut s = String::new();
    for name in SIGNIFICANT {
        s.push_str(name);
        s.push('=');
        s.push_str(&vevent_field_all(ics, name).join("\u{1f}"));
        s.push('\u{1e}');
    }
    s
}

fn non_self_attendees(ics: &str, organizer_self: &str) -> Vec<String> {
    imip::attendees(ics)
        .into_iter()
        .map(|a| a.email)
        .filter(|e| e != organizer_self)
        .collect()
}

fn is_significant_change(old: &str, new: &str) -> bool {
    use pimsteward_ical::ical::vevent_field_all;
    SIGNIFICANT.iter().any(|name| {
        vevent_field_all(old, name) != vevent_field_all(new, name)
    })
}

fn organized_by_self(ics: &str, organizer_self: &str) -> bool {
    imip::organizer(ics).map(|o| o.email == organizer_self).unwrap_or(false)
}

fn summary_of(ics: &str) -> String {
    pimsteward_ical::ical::vevent_field(ics, "SUMMARY").unwrap_or_else(|| "(no title)".into())
}

pub fn plan_change(
    change: &EventChange,
    organizer_self: &str,
    now: DateTime<Utc>,
) -> Vec<Outbound> {
    match change.kind {
        ChangeKind::Added => {
            let Some(ics) = change.new_ics.as_deref() else { return vec![] };
            if !organized_by_self(ics, organizer_self) || is_past_start(ics, now) {
                return vec![];
            }
            let to = non_self_attendees(ics, organizer_self);
            if to.is_empty() {
                return vec![];
            }
            vec![Outbound {
                method: Method::Request,
                uid: change.uid.clone(),
                sequence: 0,
                recipients: to,
                event_ics: ics.to_string(),
                summary: summary_of(ics),
            }]
        }
        ChangeKind::Modified => {
            let (Some(new), Some(old)) = (change.new_ics.as_deref(), change.old_ics.as_deref())
            else { return vec![] };
            if !organized_by_self(new, organizer_self) {
                return vec![];
            }
            let new_to = non_self_attendees(new, organizer_self);
            let old_to = non_self_attendees(old, organizer_self);
            let removed: Vec<String> =
                old_to.iter().filter(|e| !new_to.contains(e)).cloned().collect();

            let mut out = Vec::new();
            let added_attendee = new_to.iter().any(|e| !old_to.contains(e));
            if !is_past_start(new, now)
                && !new_to.is_empty()
                && (is_significant_change(old, new) || added_attendee)
            {
                out.push(Outbound {
                    method: Method::Request,
                    uid: change.uid.clone(),
                    sequence: 0,
                    recipients: new_to.clone(),
                    event_ics: new.to_string(),
                    summary: summary_of(new),
                });
            }
            if !removed.is_empty() {
                out.push(Outbound {
                    method: Method::Cancel,
                    uid: change.uid.clone(),
                    sequence: 0,
                    recipients: removed,
                    event_ics: new.to_string(),
                    summary: summary_of(new),
                });
            }
            out
        }
        ChangeKind::Deleted => {
            let Some(ics) = change.old_ics.as_deref() else { return vec![] };
            if !organized_by_self(ics, organizer_self) {
                return vec![];
            }
            let to = non_self_attendees(ics, organizer_self);
            if to.is_empty() {
                return vec![];
            }
            vec![Outbound {
                method: Method::Cancel,
                uid: change.uid.clone(),
                sequence: 0,
                recipients: to,
                event_ics: ics.to_string(),
                summary: summary_of(ics),
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(uid: &str, summary: &str, attendees: &[&str], extra: &str) -> String {
        let mut s = format!(
            "BEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:0\r\nDTSTART:20990101T120000Z\r\nSUMMARY:{summary}\r\nORGANIZER;EMAIL=dan@hld.ca:mailto:dan@hld.ca\r\n{extra}"
        );
        for a in attendees {
            s.push_str(&format!("ATTENDEE;EMAIL={a}:mailto:{a}\r\n"));
        }
        s.push_str("END:VEVENT\r\n");
        s
    }

    fn now() -> DateTime<Utc> { "2026-06-11T00:00:00Z".parse().unwrap() }

    #[test]
    fn added_with_attendees_requests_all_non_self() {
        let c = EventChange {
            kind: ChangeKind::Added, rel_path: "c/events/x.ics".into(), uid: "x".into(),
            new_ics: Some(ev("x", "Lunch", &["heather@hld.ca", "dan@hld.ca"], "")), old_ics: None,
        };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Request);
        assert_eq!(out[0].recipients, vec!["heather@hld.ca".to_string()]);
    }

    #[test]
    fn non_organizer_event_is_ignored() {
        let mut ics = ev("y", "Theirs", &["heather@hld.ca"], "");
        ics = ics.replace("EMAIL=dan@hld.ca:mailto:dan@hld.ca", "EMAIL=sean@x.com:mailto:sean@x.com");
        let c = EventChange { kind: ChangeKind::Added, rel_path: "c/events/y.ics".into(), uid: "y".into(),
            new_ics: Some(ics), old_ics: None };
        assert!(plan_change(&c, "dan@hld.ca", now()).is_empty());
    }

    #[test]
    fn time_change_is_significant() {
        let old = ev("z", "Mtg", &["heather@hld.ca"], "DTSTART:20990101T120000Z\r\n");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "DTSTART:20990101T130000Z\r\n");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Request);
    }

    #[test]
    fn alarm_only_change_is_not_significant() {
        let old = ev("z", "Mtg", &["heather@hld.ca"], "");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "BEGIN:VALARM\r\nACTION:DISPLAY\r\nEND:VALARM\r\n");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        assert!(plan_change(&c, "dan@hld.ca", now()).is_empty());
    }

    #[test]
    fn removed_attendee_gets_cancel_only() {
        let old = ev("z", "Mtg", &["heather@hld.ca", "kid@hld.ca"], "");
        let new = ev("z", "Mtg", &["heather@hld.ca"], "");
        let c = EventChange { kind: ChangeKind::Modified, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: Some(new), old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Cancel);
        assert_eq!(out[0].recipients, vec!["kid@hld.ca".to_string()]);
    }

    #[test]
    fn deleted_cancels_all() {
        let old = ev("z", "Mtg", &["heather@hld.ca", "kid@hld.ca"], "");
        let c = EventChange { kind: ChangeKind::Deleted, rel_path: "c/events/z.ics".into(), uid: "z".into(),
            new_ics: None, old_ics: Some(old) };
        let out = plan_change(&c, "dan@hld.ca", now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].method, Method::Cancel);
        let mut r = out[0].recipients.clone(); r.sort();
        assert_eq!(r, vec!["heather@hld.ca".to_string(), "kid@hld.ca".to_string()]);
    }
}
