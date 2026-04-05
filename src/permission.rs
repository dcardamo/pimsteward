//! Permission model.
//!
//! Each resource (email, calendar, contacts, sieve) has a default access
//! level. Email and calendar additionally support scoped overrides:
//!
//! - **Email** can have per-folder rules keyed by folder path ("INBOX",
//!   "Sent Mail", etc.). If a rule matches, it wins over the default.
//! - **Calendar** can have per-calendar rules keyed by the forwardemail
//!   calendar id.
//!
//! Contacts and sieve are globally scoped in v2 — forwardemail has one
//! default address book per alias and a flat namespace of sieve scripts,
//! so per-item rules add friction without meaningful security value.
//!
//! # TOML forms
//!
//! Simple (back-compat with v1):
//!
//! ```toml
//! [permissions]
//! email    = "read"
//! calendar = "read_write"
//! contacts = "read_write"
//! sieve    = "read_write"
//! ```
//!
//! Scoped (v2):
//!
//! ```toml
//! [permissions]
//! contacts = "read_write"
//! sieve    = "read_write"
//!
//! [permissions.email]
//! default = "read"
//! [permissions.email.folders]
//! "INBOX"     = "read_write"
//! "Archive"   = "read_write"
//! "Trash"     = "none"
//!
//! [permissions.calendar]
//! default = "none"
//! [permissions.calendar.by_id]
//! "cal-personal-abc" = "read_write"
//! "cal-work-xyz"     = "read"
//! ```
//!
//! Both forms deserialize into the same [`Permissions`] struct via an
//! `untagged` enum on each per-resource field.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Which forwardemail resource kind a tool or operation touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Email,
    Calendar,
    Contacts,
    Sieve,
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Email => "email",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::Sieve => "sieve",
        };
        f.write_str(s)
    }
}

/// Per-resource access level.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    None,
    Read,
    ReadWrite,
}

impl Access {
    pub fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }
    pub fn can_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Email permission: either a flat access level or a scoped form with
/// per-folder overrides. Serialized untagged so `email = "read"` (flat)
/// and `email.default = "read"` (scoped) both parse as valid TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmailPermission {
    Flat(Access),
    Scoped(ScopedEmail),
}

impl Default for EmailPermission {
    fn default() -> Self {
        // Default is a flat "none" — denies everything until the user
        // grants something explicitly. Using Flat here (rather than a
        // dedicated Unset variant) keeps the enum cleanly serializable
        // in both directions, which figment needs for its layered merge.
        Self::Flat(Access::None)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopedEmail {
    #[serde(default)]
    pub default: Access,
    #[serde(default)]
    pub folders: HashMap<String, Access>,
}

impl EmailPermission {
    pub fn default_access(&self) -> Access {
        match self {
            Self::Flat(a) => *a,
            Self::Scoped(s) => s.default,
        }
    }

    /// Access for a specific folder. Per-folder rule wins over the default
    /// if present.
    pub fn for_folder(&self, folder: Option<&str>) -> Access {
        match self {
            Self::Flat(a) => *a,
            Self::Scoped(s) => match folder {
                Some(f) => s.folders.get(f).copied().unwrap_or(s.default),
                None => s.default,
            },
        }
    }
}

/// Calendar permission: flat or scoped by calendar id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CalendarPermission {
    Flat(Access),
    Scoped(ScopedCalendar),
}

impl Default for CalendarPermission {
    fn default() -> Self {
        Self::Flat(Access::None)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScopedCalendar {
    #[serde(default)]
    pub default: Access,
    #[serde(default)]
    pub by_id: HashMap<String, Access>,
}

impl CalendarPermission {
    pub fn default_access(&self) -> Access {
        match self {
            Self::Flat(a) => *a,
            Self::Scoped(s) => s.default,
        }
    }

    pub fn for_calendar(&self, calendar_id: Option<&str>) -> Access {
        match self {
            Self::Flat(a) => *a,
            Self::Scoped(s) => match calendar_id {
                Some(c) => s.by_id.get(c).copied().unwrap_or(s.default),
                None => s.default,
            },
        }
    }
}

/// Send-email permission. Orthogonal to the read/write `email` field
/// because sending over SMTP is a different, strictly more consequential
/// capability than mutating mailbox state:
///
/// * Mailbox mutations stay inside the alias and are reversible via git
///   restore — the worst a bad move/delete can do is shuffle bytes around
///   a tree pimsteward owns.
/// * `POST /v1/emails` bridges to forwardemail's outgoing SMTP relay. Once
///   it returns success the message has been accepted for delivery to a
///   third party and there is no restore. The worst a bad send can do is
///   put words in your mouth on an audit-visible wire.
///
/// Because of that asymmetry, `email = "read_write"` deliberately does
/// NOT imply send. Send is always opt-in, and the default is `Denied`.
/// The MCP `send_email` tool calls [`Permissions::check_email_send`]
/// independently of any email read/write check.
///
/// Note that forwardemail uses one alias credential for both IMAP/REST
/// mailbox access and SMTP sending — pimsteward enforces the split at
/// the policy layer, not at the transport layer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendPermission {
    /// Default. `send_email` is not exposed and any direct call is refused.
    #[default]
    Denied,
    /// Send is allowed. `send_email` is exposed as an MCP tool and every
    /// invocation is recorded in git with a `tool: send_email` audit
    /// trailer carrying recipients, subject, and body sha256.
    Allowed,
}

impl SendPermission {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Full permission matrix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub email: EmailPermission,
    /// Send-email permission — see [`SendPermission`] for why this is a
    /// separate field rather than a level inside `email`.
    #[serde(default)]
    pub email_send: SendPermission,
    #[serde(default)]
    pub calendar: CalendarPermission,
    #[serde(default)]
    pub contacts: Access,
    #[serde(default)]
    pub sieve: Access,
}

/// Scope of a permission check — identifies a specific resource instance
/// when one is known. `Resource` (no scope) falls back to the default for
/// that resource type.
#[derive(Debug, Clone)]
pub enum Scope<'a> {
    Email { folder: Option<&'a str> },
    Calendar { calendar_id: Option<&'a str> },
    Contacts,
    Sieve,
}

impl Permissions {
    /// Flat resource-level access (default for the resource).
    pub fn get(&self, resource: Resource) -> Access {
        match resource {
            Resource::Email => self.email.default_access(),
            Resource::Calendar => self.calendar.default_access(),
            Resource::Contacts => self.contacts,
            Resource::Sieve => self.sieve,
        }
    }

    /// Scoped access lookup. If a scope override applies, returns that;
    /// otherwise returns the resource default.
    pub fn get_scoped(&self, scope: &Scope<'_>) -> Access {
        match scope {
            Scope::Email { folder } => self.email.for_folder(*folder),
            Scope::Calendar { calendar_id } => self.calendar.for_calendar(*calendar_id),
            Scope::Contacts => self.contacts,
            Scope::Sieve => self.sieve,
        }
    }

    /// Gate a resource-level read.
    pub fn check_read(&self, resource: Resource) -> Result<(), crate::Error> {
        let granted = self.get(resource);
        if granted.can_read() {
            Ok(())
        } else {
            Err(crate::Error::PermissionDenied {
                resource,
                required: Access::Read,
                granted,
            })
        }
    }

    /// Gate a resource-level write.
    pub fn check_write(&self, resource: Resource) -> Result<(), crate::Error> {
        let granted = self.get(resource);
        if granted.can_write() {
            Ok(())
        } else {
            Err(crate::Error::PermissionDenied {
                resource,
                required: Access::ReadWrite,
                granted,
            })
        }
    }

    /// Gate a read with an optional scope override (per-folder, per-calendar).
    /// If the scope is `None` for its resource, behaves identically to
    /// [`check_read`].
    pub fn check_read_scoped(&self, scope: &Scope<'_>) -> Result<(), crate::Error> {
        let granted = self.get_scoped(scope);
        if granted.can_read() {
            Ok(())
        } else {
            Err(crate::Error::PermissionDenied {
                resource: scope_resource(scope),
                required: Access::Read,
                granted,
            })
        }
    }

    /// Gate an outgoing SMTP send. Independent of `email` read/write
    /// because the blast radius of a send is strictly larger than any
    /// mailbox mutation — see [`SendPermission`].
    pub fn check_email_send(&self) -> Result<(), crate::Error> {
        if self.email_send.is_allowed() {
            Ok(())
        } else {
            Err(crate::Error::SendDenied)
        }
    }

    /// Gate a write with an optional scope override.
    pub fn check_write_scoped(&self, scope: &Scope<'_>) -> Result<(), crate::Error> {
        let granted = self.get_scoped(scope);
        if granted.can_write() {
            Ok(())
        } else {
            Err(crate::Error::PermissionDenied {
                resource: scope_resource(scope),
                required: Access::ReadWrite,
                granted,
            })
        }
    }
}

fn scope_resource(scope: &Scope<'_>) -> Resource {
    match scope {
        Scope::Email { .. } => Resource::Email,
        Scope::Calendar { .. } => Resource::Calendar,
        Scope::Contacts => Resource::Contacts,
        Scope::Sieve => Resource::Sieve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Access basics ───────────────────────────────────────────────

    #[test]
    fn access_ordering() {
        assert!(Access::None < Access::Read);
        assert!(Access::Read < Access::ReadWrite);
    }

    #[test]
    fn can_read_matrix() {
        assert!(!Access::None.can_read());
        assert!(Access::Read.can_read());
        assert!(Access::ReadWrite.can_read());
    }

    #[test]
    fn can_write_matrix() {
        assert!(!Access::None.can_write());
        assert!(!Access::Read.can_write());
        assert!(Access::ReadWrite.can_write());
    }

    // ── Resource-level checks (back-compat with v1 API) ────────────

    fn flat_permissions(
        email: Access,
        calendar: Access,
        contacts: Access,
        sieve: Access,
    ) -> Permissions {
        Permissions {
            email: EmailPermission::Flat(email),
            email_send: SendPermission::Denied,
            calendar: CalendarPermission::Flat(calendar),
            contacts,
            sieve,
        }
    }

    #[test]
    fn check_read_allows_read_and_readwrite() {
        let p = flat_permissions(Access::Read, Access::ReadWrite, Access::None, Access::None);
        assert!(p.check_read(Resource::Email).is_ok());
        assert!(p.check_read(Resource::Calendar).is_ok());
        assert!(p.check_read(Resource::Contacts).is_err());
        assert!(p.check_read(Resource::Sieve).is_err());
    }

    #[test]
    fn check_write_requires_readwrite() {
        let p = flat_permissions(Access::Read, Access::ReadWrite, Access::None, Access::None);
        assert!(p.check_write(Resource::Email).is_err());
        assert!(p.check_write(Resource::Calendar).is_ok());
    }

    #[test]
    fn default_permissions_deny_everything() {
        let p = Permissions::default();
        for r in [
            Resource::Email,
            Resource::Calendar,
            Resource::Contacts,
            Resource::Sieve,
        ] {
            assert!(p.check_read(r).is_err(), "{r} should default to deny");
            assert!(p.check_write(r).is_err(), "{r} should default to deny");
        }
        // Send is a separate capability; default must also be deny.
        assert!(p.check_email_send().is_err(), "send defaults to denied");
    }

    // ── Send permission ────────────────────────────────────────────
    //
    // send is orthogonal to read/write on the email resource. The
    // invariant the tests below protect: read_write on email NEVER
    // implies send — you must explicitly set email_send = "allowed".

    #[test]
    fn send_denied_by_default_even_with_email_read_write() {
        let p = Permissions {
            email: EmailPermission::Flat(Access::ReadWrite),
            ..Permissions::default()
        };
        assert!(
            p.check_email_send().is_err(),
            "read_write on email must NOT grant send — send is its own opt-in"
        );
    }

    #[test]
    fn send_allowed_when_explicitly_set() {
        let p = Permissions {
            email_send: SendPermission::Allowed,
            ..Permissions::default()
        };
        assert!(p.check_email_send().is_ok());
    }

    #[test]
    fn send_allowed_works_even_when_email_is_none() {
        // Weird but legal: you can grant send without read_write. The
        // AI could compose a message via its own tool context and ask
        // pimsteward to transmit it without ever reading the mailbox.
        // pimsteward should not second-guess this config — the user
        // said what they meant.
        let p = Permissions {
            email: EmailPermission::Flat(Access::None),
            email_send: SendPermission::Allowed,
            ..Permissions::default()
        };
        assert!(p.check_email_send().is_ok());
    }

    #[test]
    fn send_denied_error_is_its_own_variant() {
        let p = Permissions::default();
        match p.check_email_send() {
            Err(crate::Error::SendDenied) => {}
            other => panic!("expected SendDenied, got {other:?}"),
        }
    }

    // ── Scoped checks ──────────────────────────────────────────────

    #[test]
    fn email_scoped_default_applies_when_no_folder() {
        let p = Permissions {
            email: EmailPermission::Scoped(ScopedEmail {
                default: Access::Read,
                folders: HashMap::new(),
            }),
            ..Permissions::default()
        };
        assert!(p.check_read_scoped(&Scope::Email { folder: None }).is_ok());
        assert!(p
            .check_write_scoped(&Scope::Email { folder: None })
            .is_err());
    }

    #[test]
    fn email_per_folder_override_wins() {
        let mut folders = HashMap::new();
        folders.insert("INBOX".to_string(), Access::ReadWrite);
        folders.insert("Trash".to_string(), Access::None);
        let p = Permissions {
            email: EmailPermission::Scoped(ScopedEmail {
                default: Access::Read,
                folders,
            }),
            ..Permissions::default()
        };

        // INBOX: overridden to readwrite
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("INBOX")
            })
            .is_ok());
        // Trash: overridden to none — not even readable
        assert!(p
            .check_read_scoped(&Scope::Email {
                folder: Some("Trash")
            })
            .is_err());
        // Unknown folder: falls through to default=read
        assert!(p
            .check_read_scoped(&Scope::Email {
                folder: Some("SomeOtherFolder")
            })
            .is_ok());
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("SomeOtherFolder")
            })
            .is_err());
    }

    #[test]
    fn calendar_scoped_per_id_override() {
        let mut by_id = HashMap::new();
        by_id.insert("cal-1".to_string(), Access::ReadWrite);
        by_id.insert("cal-2".to_string(), Access::None);
        let p = Permissions {
            calendar: CalendarPermission::Scoped(ScopedCalendar {
                default: Access::Read,
                by_id,
            }),
            ..Permissions::default()
        };

        assert!(p
            .check_write_scoped(&Scope::Calendar {
                calendar_id: Some("cal-1")
            })
            .is_ok());
        assert!(p
            .check_read_scoped(&Scope::Calendar {
                calendar_id: Some("cal-2")
            })
            .is_err());
        // Unknown id falls back to default
        assert!(p
            .check_read_scoped(&Scope::Calendar {
                calendar_id: Some("unknown")
            })
            .is_ok());
    }

    #[test]
    fn email_drafts_only_write_with_default_read() {
        // The motivating scoped use case: agent can read everything, write
        // only to Drafts. No resource-level baseline gate should block
        // this — per-folder override must be authoritative.
        let mut folders = HashMap::new();
        folders.insert("Drafts".to_string(), Access::ReadWrite);
        let p = Permissions {
            email: EmailPermission::Scoped(ScopedEmail {
                default: Access::Read,
                folders,
            }),
            ..Permissions::default()
        };
        // Read works everywhere
        assert!(p
            .check_read_scoped(&Scope::Email {
                folder: Some("INBOX")
            })
            .is_ok());
        // Write blocked on non-Drafts folders
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("INBOX")
            })
            .is_err());
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("Trash")
            })
            .is_err());
        // Write allowed on Drafts
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("Drafts")
            })
            .is_ok());
    }

    #[test]
    fn flat_email_permission_ignores_folder_scope() {
        let p = Permissions {
            email: EmailPermission::Flat(Access::ReadWrite),
            ..Permissions::default()
        };
        // Flat permissions return the same access regardless of folder
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("INBOX")
            })
            .is_ok());
        assert!(p
            .check_write_scoped(&Scope::Email {
                folder: Some("Trash")
            })
            .is_ok());
    }

    // ── TOML roundtrip — back-compat and new scoped form ──────────

    #[test]
    fn toml_flat_form_parses_as_v1_did() {
        let toml_str = r#"
email = "read"
calendar = "read_write"
contacts = "read_write"
sieve = "none"
"#;
        let p: Permissions = toml::from_str(toml_str).unwrap();
        assert_eq!(p.email.default_access(), Access::Read);
        assert_eq!(p.calendar.default_access(), Access::ReadWrite);
        assert_eq!(p.contacts, Access::ReadWrite);
        assert_eq!(p.sieve, Access::None);
        // email_send is absent from this config — must default to denied.
        assert_eq!(p.email_send, SendPermission::Denied);
    }

    #[test]
    fn toml_email_send_roundtrip() {
        // Default (absent): denied.
        let p: Permissions = toml::from_str("").unwrap();
        assert_eq!(p.email_send, SendPermission::Denied);

        // Explicit denied.
        let p: Permissions = toml::from_str(r#"email_send = "denied""#).unwrap();
        assert_eq!(p.email_send, SendPermission::Denied);
        assert!(p.check_email_send().is_err());

        // Explicit allowed.
        let p: Permissions = toml::from_str(r#"email_send = "allowed""#).unwrap();
        assert_eq!(p.email_send, SendPermission::Allowed);
        assert!(p.check_email_send().is_ok());

        // Combined with a read-only email config — the common
        // "agent can draft but also send" shape.
        let p: Permissions = toml::from_str(
            r#"
email = "read"
email_send = "allowed"
"#,
        )
        .unwrap();
        assert_eq!(p.email.default_access(), Access::Read);
        assert_eq!(p.email_send, SendPermission::Allowed);
    }

    #[test]
    fn toml_scoped_email_form_parses() {
        let toml_str = r#"
contacts = "read_write"
sieve = "read_write"

[email]
default = "read"
folders = { INBOX = "read_write", Trash = "none" }

[calendar]
default = "read"
by_id = { "cal-abc" = "read_write" }
"#;
        let p: Permissions = toml::from_str(toml_str).unwrap();
        assert_eq!(p.email.default_access(), Access::Read);
        assert_eq!(
            p.email.for_folder(Some("INBOX")),
            Access::ReadWrite,
            "INBOX override should win"
        );
        assert_eq!(p.email.for_folder(Some("Trash")), Access::None);
        assert_eq!(
            p.email.for_folder(Some("Unknown")),
            Access::Read,
            "unknown folder should fall back to default"
        );
        assert_eq!(p.calendar.for_calendar(Some("cal-abc")), Access::ReadWrite);
        assert_eq!(
            p.calendar.for_calendar(Some("cal-xyz")),
            Access::Read,
            "unknown cal id falls back to default"
        );
    }

    #[test]
    fn toml_mixed_flat_and_scoped() {
        // Email flat, calendar scoped — both should work in one doc.
        let toml_str = r#"
email = "read"
contacts = "read_write"
sieve = "none"

[calendar]
default = "none"
by_id = { "work" = "read" }
"#;
        let p: Permissions = toml::from_str(toml_str).unwrap();
        assert_eq!(p.email.default_access(), Access::Read);
        assert_eq!(p.calendar.for_calendar(Some("work")), Access::Read);
        assert_eq!(
            p.calendar.for_calendar(Some("personal")),
            Access::None,
            "unknown cal should fall through to calendar.default=none"
        );
    }
}
