//! Source abstraction for pulling mail state from a backend.
//!
//! pimsteward can read mail from more than one backend:
//!
//! - **[`RestMailSource`]** — forwardemail's REST API, which is the v1
//!   default. Low overhead when polling is cheap and the API's `raw`
//!   field gives us byte-identical RFC822.
//! - **[`ImapMailSource`]** — native IMAP with CONDSTORE modseq-based
//!   delta sync. Trades a little setup complexity for efficient polling
//!   at high message volumes.
//!
//! Both implement the [`MailSource`] trait, which the pull loop calls to
//! list folders, list messages, and fetch raw bytes. Writes stay on REST
//! regardless of read source — forwardemail's write semantics are cleaner
//! via REST and mixing write backends would complicate audit attribution.
//!
//! Calendar has both [`CalendarSource`] and [`CalendarWriter`] traits with
//! REST and iCloud-CalDAV implementations. Contacts has [`ContactsSource`]
//! (REST + CardDAV). Sieve is forwardemail-specific and has no trait.

pub mod caldav;
pub mod carddav;
pub mod dav;
pub mod imap;
pub mod rest;
pub mod traits;

pub use caldav::DavCalendarSource;
pub use carddav::DavContactsSource;
pub use imap::ImapMailSource;
pub use rest::{RestCalendarSource, RestCalendarWriter, RestContactsSource, RestMailSource};
pub use traits::{
    CalendarSource, CalendarWriter, ContactsSource, FetchedMessage, ListResult, MailSource,
    MailWriter,
};
