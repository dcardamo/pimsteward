//! Dependency-light iCalendar helpers and the `CalendarEvent` DTO.
//!
//! This crate is deliberately free of network, storage, and credential
//! code so it can be shared with a host-side ICS feed builder without
//! dragging in pimsteward's full surface. It depends only on
//! serde/chrono/time.

mod event;
pub mod feed;
pub mod ical;
pub mod imip;

pub use event::CalendarEvent;
pub use feed::{build_feed, keep_for_feed, merge_calendar};
pub use imip::{attendees, build_imip, organizer, IcalAddress};
