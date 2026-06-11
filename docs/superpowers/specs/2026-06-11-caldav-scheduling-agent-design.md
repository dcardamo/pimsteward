# CalDAV Scheduling Agent (organizer-side iMIP) — Design

**Date:** 2026-06-11
**Status:** Approved design, pending spec review
**Scope:** pimsteward only. Zero changes to rocky.

## Problem

ForwardEmail's CalDAV server does not implement server-side scheduling
(iMIP / iTIP, RFC 6638). When Dan creates a calendar event in Apple Calendar
(macOS) or DavX5 + Etar (Android) on the `dan@hld.ca` account and adds invitees,
the client writes `ORGANIZER` + `ATTENDEE` properties into the event via CalDAV
`PUT`, but **no invitation email is ever sent** — the server doesn't generate the
`METHOD:REQUEST` message, and a plain-CalDAV client won't either. Invitees are
recorded but never notified.

Confirmed from real data in `/var/lib/pimsteward-dan`:

- ~895 events have `ORGANIZER;...EMAIL=dan@hld.ca` (Dan is organizer).
- 1427 / 3170 events carry `ATTENDEE` lines with recoverable `EMAIL=` params.
- Some events show `SCHEDULE-AGENT=CLIENT` — RFC 6638's signal that scheduling
  fell to the client, which then did nothing.

## Goal

pimsteward becomes the organizer-side scheduling agent ForwardEmail isn't:
detect events Dan organizes, and send the iMIP messages (`REQUEST` for new and
updated events, `CANCEL` for deletions/removed attendees) on his behalf,
`From: dan@hld.ca`, with every send captured in the git audit log.

## Why pimsteward, not rocky

- The invite **must** be `From: dan@hld.ca` so recipient calendar apps accept it
  (iMIP requires `From` to match `ORGANIZER`) and RSVPs route back to Dan. Only
  pimsteward's `dan` daemon holds the `dan@hld.ca` alias credentials. Rocky sends
  as `rocky@hld.ca` — wrong identity, would break iMIP.
- pimsteward already owns the CalDAV pull, the git change-detection store, the
  third-party `send_email` capability, and the permission/audit model. The
  scheduling engine is composed entirely of pieces that already live there.
- pimsteward's design thesis is "permission-aware mediation + receipts for PIM."
  Organizer-side scheduling-as-a-service is squarely in scope.

Rocky is **not** in the path, including notifications. The debug notification
goes pimsteward → dan@ directly.

## Architecture

New module in the `pimsteward` crate (working name `scheduling`). It extends
existing subsystems rather than introducing new infrastructure:

- **Trigger:** hook the existing calendar pull loop. `pull::calendar::pull_calendar`
  already commits all changes per pull and returns a `PullSummary` with a
  `commit_sha` and added/updated/deleted counts. After each pull that produced
  changes, the scheduler **diffs `commit_sha` against its parent** to get the
  exact set of added / modified / deleted `.ics` files. That git diff *is* the
  change feed — no separate state machine.
- **iCalendar build/parse:** the `pimsteward-ical` crate already parses VEVENTs
  and serializes calendars (it emits `METHOD:PUBLISH` for feeds). Building a
  `METHOD:REQUEST` / `METHOD:CANCEL` VCALENDAR is an extension of existing code.
- **Send:** extend the forwardemail send path (`writes.rs`) to emit a proper
  iMIP MIME message (see below). Delivery, DKIM signing, and Sent-folder capture
  are already handled by `POST /v1/emails`.

### Components (each independently testable)

1. **Change feed** — given a pull's `commit_sha`, return `Vec<EventChange>` where
   each is `{ kind: Added|Modified|Deleted, uid, calendar_dir, new_ics, old_ics }`.
   Pure git-diff logic over the repo.
2. **Watermark gate** — filters the change feed to commits strictly after the
   activation watermark, and drops events whose `DTSTART` is in the past.
3. **Organizer/recipient resolver** — keeps only events where `ORGANIZER` email
   == `dan@hld.ca`; extracts attendee addresses from each `ATTENDEE`'s `EMAIL=`
   param (fallback: the `mailto:` cal-address value); excludes the organizer's
   own self-attendee.
4. **Scheduling-significance differ** — for `Modified` events, compares old vs new
   VEVENT and decides whether a re-send is warranted (see Lifecycle).
5. **iMIP builder** — produces the MIME message + the `text/calendar` payload for
   a given (event, method, recipients), normalizing cal-addresses to `mailto:`.
6. **Sent ledger** — restart-safe dedup + audit (see Idempotency).
7. **Notifier** — when `notify_on_send`, emails dan@ a summary of each send.
8. **Orchestrator** — wires 1→7 together, invoked post-pull.

## Lifecycle (organizer-side, scope B)

Per change in the (watermark-filtered, organizer-only) feed:

| Change                                   | Action                                              |
| ---------------------------------------- | --------------------------------------------------- |
| New event file with ≥1 attendee          | `METHOD:REQUEST` to all attendees                   |
| Modified — scheduling-significant change  | `METHOD:REQUEST` update (bumped `SEQUENCE`) to all  |
| Modified — attendee added                | `METHOD:REQUEST` to all attendees                   |
| Modified — attendee removed              | `METHOD:CANCEL` to the removed attendee only        |
| Event file deleted                       | `METHOD:CANCEL` to all attendees                    |

**Scheduling-significant fields:** `DTSTART`, `DTEND`, `RRULE`, `EXDATE`,
`RECURRENCE-ID`, `SUMMARY`, `LOCATION`, `DESCRIPTION`, attendee-set additions.
Changes to alarms (`VALARM`), `X-` properties, or attendee `PARTSTAT` alone are
**not** significant and trigger no send (avoids spamming attendees on noise).

**Recurring events:** a series is one invite — the VEVENT carries its `RRULE`,
so a single `REQUEST` covers it. Per-instance exception edits (`RECURRENCE-ID`)
are treated as an update to that instance. v1 scope.

## iMIP message construction

A `multipart/alternative` message:

- `text/plain` — human-readable summary (title, time, location, organizer).
- `text/calendar; method=REQUEST; charset=UTF-8` (or `CANCEL`) — a minimal valid
  VCALENDAR: `VERSION:2.0`, `PRODID`, `METHOD`, and the VEVENT with `UID`,
  `DTSTAMP`, `SEQUENCE`, `DTSTART`/`DTEND` (and `RRULE`/`EXDATE` when present),
  `SUMMARY`, `LOCATION`, `ORGANIZER`, and `ATTENDEE` lines with `RSVP=TRUE`.

Headers: `From: dan@hld.ca`, `To:` the recipient(s), `Subject:` derived from the
event summary (e.g. `Invitation: <summary>`), `Content-Class` per convention.

**Cal-address normalization (required):** ForwardEmail stores organizer/attendee
values as opaque tokens
(`ORGANIZER;CN=Dan Cardamore;EMAIL=dan@hld.ca:/aMTc...`). The outgoing iMIP must
use `mailto:` cal-addresses — rewrite the value after the colon to
`mailto:dan@hld.ca` for the organizer and `mailto:<EMAIL>` for each attendee.
Without this, recipient calendar apps reject the invite and RSVPs misroute.

Sent via `POST /v1/emails` using the dan alias → ForwardEmail DKIM-signs and
files a copy to Sent, which the next pull captures into git automatically.

## Idempotency — sent ledger

A `scheduling/sent.jsonl` file committed to the repo. One record per send:
`{ uid, sequence, recipient, method, message_id, sent_at }`. Before sending,
check the ledger; skip if `(uid, sequence, recipient, method)` already present.
After a successful send, append + commit. This guarantees:

- No double-send when a later pull re-touches an unchanged file.
- Restart safety — a daemon crash mid-batch resumes without re-sending.
- A complete audit trail of every scheduling message ever sent.

`SEQUENCE` handling: `REQUEST` uses the event's current `SEQUENCE`. A `CANCEL` on
deletion uses `last-seen SEQUENCE + 1` (tracked via the ledger).

## Configuration

New `[scheduling]` section in the dan provider config:

```toml
[scheduling]
enabled = true
notify_on_send = true   # debug tripwire — email dan@ on every send (ON for now)
```

No `dry_run` — live-sending from deploy per decision. `notify_on_send` is the
tripwire and can be flipped to `false` later.

## Permissions

Gated by pimsteward's existing per-resource permission policy. Sending iMIP
requires the `send` capability (already granted to the dan config) plus calendar
read (already `read_write`). The scheduler must refuse to send if `send` is not
permitted.

## Debug notification (ON now)

When `notify_on_send = true`, every iMIP send to third parties is accompanied by
a summary email to `dan@hld.ca`: event summary, method (REQUEST/CANCEL),
recipient list, and SEQUENCE. Sent at the same time as the invite (no hold/delay).

## Testing

**Unit (TDD):**
- Change-feed git-diff produces correct Added/Modified/Deleted sets.
- Watermark gate excludes pre-watermark commits and past `DTSTART`.
- Organizer filter keeps only dan@-organized events; recipient extraction from
  `EMAIL=` param with `mailto:` fallback; self-attendee excluded.
- Scheduling-significance differ: significant vs noise classification.
- iMIP builder golden tests: byte-stable `METHOD:REQUEST` and `METHOD:CANCEL`
  output, including cal-address normalization and `RSVP=TRUE`.
- Ledger dedup: same (uid, sequence, recipient, method) is sent once.

**End-to-end acceptance gate (mandatory before going live on real invitees):**
An automated test driving the **real** `dan@hld.ca` calendar but with
`rocky@hld.ca` as the **sole** invitee (a mailbox Dan owns, readable via the
`pimsteward-rocky` daemon — so no real contact is ever emailed):

1. Create an event with organizer dan@ and attendee rocky@ → assert a
   `METHOD:REQUEST` arrives in rocky@'s inbox with correct UID/SEQUENCE and a
   valid `text/calendar` part.
2. Update the event's start time → assert a `METHOD:REQUEST` update (incremented
   SEQUENCE) arrives.
3. Cancel / delete the event → assert a `METHOD:CANCEL` arrives.
4. Clean up: remove the test event(s) and ledger entries created by the test.

The e2e exercises every code path against live infrastructure (real CalDAV
write, real pull, real send, real receipt) while staying contained to
Dan-owned mailboxes. Only after it passes do we trust the agent with real
invitees.

## Out of scope (v1)

- Inbound RSVP processing (attendee accept/decline → writing `PARTSTAT` back).
  Organizer-side only.
- Acting on events organized by anyone other than dan@hld.ca.
- Backfilling invites for events created before activation (watermark forbids it).
