//! iCloud CalDAV-specific code. The generic CalDAV transport lives in
//! `src/source/dav.rs` and `src/source/caldav.rs`. This module layers
//! iCloud-specific quirks on top: RFC 6764 discovery (`discovery`), the
//! User-Agent requirement, and etag-strict read/write semantics
//! (`caldav`). Task 6 will wire these behind the `Provider` trait.

pub mod caldav;
pub mod discovery;

pub use caldav::{IcloudCalendarSource, IcloudCalendarWriter};
pub use discovery::{discover, DiscoveredCalendar};
