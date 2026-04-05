//! Permission model.
//!
//! v1 is deliberately coarse: one setting per resource type, applied
//! globally. See PLAN.md § "Permission model" for rationale.

use serde::{Deserialize, Serialize};
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
    /// True if this access level permits reading (read or readwrite).
    pub fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    /// True if this access level permits writing (readwrite only).
    pub fn can_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// Full permission matrix — one [`Access`] per [`Resource`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub email: Access,
    #[serde(default)]
    pub calendar: Access,
    #[serde(default)]
    pub contacts: Access,
    #[serde(default)]
    pub sieve: Access,
}

impl Permissions {
    /// Look up the access level for a resource.
    pub fn get(&self, resource: Resource) -> Access {
        match resource {
            Resource::Email => self.email,
            Resource::Calendar => self.calendar,
            Resource::Contacts => self.contacts,
            Resource::Sieve => self.sieve,
        }
    }

    /// Gate a read operation. Returns an error if the resource isn't readable.
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

    /// Gate a write operation. Returns an error if the resource isn't writable.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn check_read_allows_read_and_readwrite() {
        let p = Permissions {
            email: Access::Read,
            calendar: Access::ReadWrite,
            contacts: Access::None,
            sieve: Access::None,
        };
        assert!(p.check_read(Resource::Email).is_ok());
        assert!(p.check_read(Resource::Calendar).is_ok());
        assert!(p.check_read(Resource::Contacts).is_err());
        assert!(p.check_read(Resource::Sieve).is_err());
    }

    #[test]
    fn check_write_requires_readwrite() {
        let p = Permissions {
            email: Access::Read,
            calendar: Access::ReadWrite,
            contacts: Access::None,
            sieve: Access::None,
        };
        // read-only on email: write blocked
        assert!(p.check_write(Resource::Email).is_err());
        // readwrite on calendar: write allowed
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
    }

    #[test]
    fn roundtrip_toml() {
        let p = Permissions {
            email: Access::Read,
            calendar: Access::ReadWrite,
            contacts: Access::ReadWrite,
            sieve: Access::None,
        };
        let s = toml::to_string(&p).unwrap();
        let back: Permissions = toml::from_str(&s).unwrap();
        assert_eq!(back.email, Access::Read);
        assert_eq!(back.calendar, Access::ReadWrite);
        assert_eq!(back.contacts, Access::ReadWrite);
        assert_eq!(back.sieve, Access::None);
    }
}
