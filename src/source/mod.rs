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
//! Calendar, contacts, and sieve don't have a trait abstraction in v2.2
//! because only one backend exists for each (REST). Add per-resource
//! traits when a second backend arrives.

pub mod imap;
pub mod rest;
pub mod traits;

pub use imap::ImapMailSource;
pub use rest::RestMailSource;
pub use traits::{FetchedMessage, MailSource};
