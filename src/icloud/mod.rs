//! iCloud CalDAV-specific code. The generic CalDAV transport lives in
//! `src/source/dav.rs` and `src/source/caldav.rs`. This module layers
//! iCloud-specific quirks on top: RFC 6764 discovery, the User-Agent
//! requirement, etag-strict write semantics. Task 4 adds discovery only;
//! Task 5 will add the source/writer.

pub mod caldav;
pub mod discovery;

pub use discovery::{discover, DiscoveredCalendar};
