//! The tenant record, its validated identifiers and the checked state machine.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// A tenant id: a UUIDv7 whose time and entropy the caller supplies (this crate never reads a
/// clock or a random source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TenantId(Uuid);

impl TenantId {
    /// Builds a UUIDv7 from a Unix-millisecond timestamp and ten bytes of caller entropy.
    #[must_use]
    pub fn new_v7(unix_ms: u64, entropy: [u8; 10]) -> Self {
        Self(uuid::Builder::from_unix_timestamp_millis(unix_ms, &entropy).into_uuid())
    }

    /// Wraps an existing UUID; only version 7 is accepted.
    ///
    /// # Errors
    /// [`Error::InvalidRequest`] when the UUID is not version 7.
    pub fn from_uuid(id: Uuid) -> Result<Self> {
        if id.get_version_num() == 7 {
            Ok(Self(id))
        } else {
            Err(Error::InvalidRequest {
                reason: "a tenant id must be a UUIDv7",
            })
        }
    }

    /// The underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// The schema name derived from this id. It is derived from the id and never from the
    /// tenant-chosen name, so a name can never reach an SQL identifier.
    #[must_use]
    pub fn schema_name(&self) -> String {
        format!("t_{}", self.0.simple())
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

fn valid_ident(s: &str, allow_hyphen: bool) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    s.len() <= 63
        && first.is_ascii_lowercase()
        && s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || (allow_hyphen && c == '-')
        })
}

macro_rules! ident_type {
    ($(#[$doc:meta])* $name:ident, $reason:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps the identifier (`[a-z][a-z0-9_-]*`, at most 63 characters).
            ///
            /// # Errors
            /// [`Error::InvalidRequest`] when the value is malformed.
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if valid_ident(&value, true) {
                    Ok(Self(value))
                } else {
                    Err(Error::InvalidRequest { reason: $reason })
                }
            }

            /// The identifier as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

ident_type!(
    /// A data-residency region.
    Region,
    "a region must be lower-case letters, digits, '-' or '_' and start with a letter"
);
ident_type!(
    /// A licence edition.
    Edition,
    "an edition must be lower-case letters, digits, '-' or '_' and start with a letter"
);
ident_type!(
    /// A cluster identifier.
    ClusterId,
    "a cluster id must be lower-case letters, digits, '-' or '_' and start with a letter"
);
ident_type!(
    /// A database identifier within a cluster.
    DatabaseId,
    "a database id must be lower-case letters, digits, '-' or '_' and start with a letter"
);

/// Names no tenant may take: they would collide with platform or SQL vocabulary.
const RESERVED_NAMES: &[&str] = &[
    "admin", "api", "control", "default", "internal", "platform", "public", "root", "spot",
    "system", "template",
];

/// A tenant name: 3 to 63 characters of `[a-z0-9-]`, starting with a letter, no trailing or
/// doubled hyphen, not a reserved word.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantName(String);

impl TenantName {
    /// Validates and wraps the name.
    ///
    /// # Errors
    /// [`Error::InvalidName`] naming the broken rule.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let reason = if value.len() < 3 || value.len() > 63 {
            Some("a name has 3 to 63 characters")
        } else if !value.starts_with(|c: char| c.is_ascii_lowercase()) {
            Some("a name starts with a lower-case letter")
        } else if !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            Some("a name uses only lower-case letters, digits and hyphens")
        } else if value.ends_with('-') || value.contains("--") {
            Some("a name has no trailing or doubled hyphen")
        } else if RESERVED_NAMES.contains(&value.as_str()) {
            Some("the name is reserved by the platform")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(Error::InvalidName { reason }),
            None => Ok(Self(value)),
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TenantName {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<TenantName> for String {
    fn from(value: TenantName) -> Self {
        value.0
    }
}

impl fmt::Display for TenantName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a tenant's data lives.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Placement {
    /// The cluster.
    pub cluster: ClusterId,
    /// The database in that cluster.
    pub database: DatabaseId,
    /// The schema in that database (always [`TenantId::schema_name`]).
    pub schema: String,
}

impl Placement {
    /// Checks the schema is the SQL-safe one derived from `id`.
    ///
    /// # Errors
    /// [`Error::InvalidPlacement`] when the schema is not the one derived from the tenant id.
    pub fn validate_for(&self, id: &TenantId) -> Result<()> {
        if self.schema == id.schema_name() && valid_ident(&self.schema, false) {
            Ok(())
        } else {
            Err(Error::InvalidPlacement {
                reason: "the schema must be the one derived from the tenant id",
            })
        }
    }
}

/// The lifecycle state of a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TenantState {
    /// Being provisioned; not usable.
    Provisioning,
    /// Serving.
    Active,
    /// Closed to its users; data kept.
    Suspended,
    /// Being moved to another placement.
    Moving,
    /// Archived or waiting out the retention delay.
    Deprovisioning,
    /// Gone; the record stays for audit.
    Deleted,
}

impl TenantState {
    /// Whether the transition table allows `self -> to`.
    #[must_use]
    pub fn can_become(self, to: Self) -> bool {
        use TenantState::{Active, Deleted, Deprovisioning, Moving, Provisioning, Suspended};
        matches!(
            (self, to),
            (Provisioning, Active | Deleted)
                | (Active, Suspended | Moving | Deprovisioning)
                | (Suspended, Active | Moving | Deprovisioning)
                | (Moving, Active | Suspended)
                | (Deprovisioning, Suspended | Deleted)
        )
    }
}

/// Tenant flags (a small bit set; unknown bits are rejected by [`TenantFlags::from_bits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct TenantFlags(u32);

impl TenantFlags {
    /// No flags.
    pub const NONE: Self = Self(0);
    /// Data may not be deleted or archived-and-dropped while set.
    pub const LEGAL_HOLD: Self = Self(1);
    /// The tenant is read-only.
    pub const READ_ONLY: Self = Self(2);
    const ALL: u32 = 3;

    /// Builds flags from raw bits.
    ///
    /// # Errors
    /// [`Error::InvalidRequest`] on an unknown bit.
    pub fn from_bits(bits: u32) -> Result<Self> {
        if bits & !Self::ALL == 0 {
            Ok(Self(bits))
        } else {
            Err(Error::InvalidRequest {
                reason: "unknown tenant flag",
            })
        }
    }

    /// Whether every flag of `other` is set.
    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union.
    #[must_use]
    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Flags without `other`.
    #[must_use]
    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// Where the final backup of a deprovisioning tenant stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// The backup reference returned by the archive port.
    pub backup_ref: String,
    /// Whether the backup was verified.
    pub verified: bool,
    /// Unix milliseconds before which nothing may be deleted.
    pub delete_after: u64,
}

/// One tenant in the control store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    /// The id.
    pub id: TenantId,
    /// The unique name.
    pub name: TenantName,
    /// The residency region; immutable once set.
    pub region: Region,
    /// The licence edition.
    pub edition: Edition,
    /// The lifecycle state.
    pub state: TenantState,
    /// Where the data lives.
    pub placement: Placement,
    /// Unix milliseconds of creation.
    pub created: u64,
    /// Flags.
    pub flags: TenantFlags,
    /// The optimistic version, bumped by every store update.
    pub version: u64,
    /// Final-backup record while deprovisioning.
    pub retention: Option<Retention>,
    /// After deletion, the name stays reserved until this instant (Unix milliseconds).
    pub name_reserved_until: Option<u64>,
}
