//! MCP server exposing pimsteward's capabilities to AI assistants.
//!
//! Transport: stdio. Any MCP-compatible client (Claude Desktop, Cursor,
//! etc.) can spawn `pimsteward mcp` and talk JSON-RPC over stdin/stdout.
//!
//! Tool surface (v1, read-only):
//!
//! | Tool           | Resource  | Description                                       |
//! | -------------- | --------- | ------------------------------------------------- |
//! | search_email   | email     | Pass-through to forwardemail's message search     |
//! | list_folders   | email     | List mailbox folders with message counts          |
//! | list_calendars | calendar  | List calendars                                    |
//! | list_events    | calendar  | List events from git cache (fast, offline-capable)|
//! | list_contacts  | contacts  | List contacts from git cache                      |
//! | list_sieve     | sieve     | List sieve scripts                                |
//! | history        | meta      | Git log for a path in the backup tree             |
//!
//! Permission gating: every tool checks the config's access level at call
//! time and returns an MCP error if denied. In principle we could also hide
//! denied tools from `list_tools`, but keeping them visible-but-rejecting
//! makes the error message more useful to an AI that's trying to learn the
//! permissions surface.

pub mod server;

pub use server::{ManageSieveConfig, PimstewardServer};
