//! The core data model: scopes, typed stored and effective values, resolution results and
//! the diff and hash carriers.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_mgmt_ops::ParamValue;

/// A scope level at which a setting may be set, ordered from broad to narrow.
///
/// The order is the contract: a chain lists scopes in this order and the narrowest set value
/// wins during resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The entire fleet.
    Fleet,
    /// A tagged group of servers.
    Group,
    /// A single server.
    Server,
    /// A tenant and its application data.
    Tenant,
    /// A company within a tenant.
    Company,
    /// A user within a company.
    User,
}

impl Scope {
    /// All scopes, broad to narrow.
    pub const ALL: [Scope; 6] = [
        Scope::Fleet,
        Scope::Group,
        Scope::Server,
        Scope::Tenant,
        Scope::Company,
        Scope::User,
    ];

    /// The lower-case wire name (`fleet`, `group`, ...).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Scope::Fleet => "fleet",
            Scope::Group => "group",
            Scope::Server => "server",
            Scope::Tenant => "tenant",
            Scope::Company => "company",
            Scope::User => "user",
        }
    }

    /// The scope named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|scope| scope.name() == name)
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A scope with its identifier: `Fleet`, `Group("emea")`, `Server("srv-17")`,
/// `Tenant("acme")`, `Company("acme/C1")`, `User("acme/u42")`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeRef {
    /// The entire fleet (no identifier).
    Fleet,
    /// A tagged group of servers.
    Group(String),
    /// A single server.
    Server(String),
    /// A tenant and its application data.
    Tenant(String),
    /// A company within a tenant.
    Company(String),
    /// A user within a company.
    User(String),
}

impl ScopeRef {
    /// The scope level of this reference.
    #[must_use]
    pub fn scope(&self) -> Scope {
        match self {
            ScopeRef::Fleet => Scope::Fleet,
            ScopeRef::Group(_) => Scope::Group,
            ScopeRef::Server(_) => Scope::Server,
            ScopeRef::Tenant(_) => Scope::Tenant,
            ScopeRef::Company(_) => Scope::Company,
            ScopeRef::User(_) => Scope::User,
        }
    }

    /// The identifier, if the level has one.
    #[must_use]
    pub fn identifier(&self) -> Option<&str> {
        match self {
            ScopeRef::Fleet => None,
            ScopeRef::Group(id)
            | ScopeRef::Server(id)
            | ScopeRef::Tenant(id)
            | ScopeRef::Company(id)
            | ScopeRef::User(id) => Some(id),
        }
    }

    /// The canonical `level:id` text used in hashes and origin labels (`fleet`, `group:emea`).
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            ScopeRef::Fleet => "fleet".to_owned(),
            _ => format!(
                "{}:{}",
                self.scope().name(),
                self.identifier().unwrap_or("")
            ),
        }
    }
}

impl fmt::Display for ScopeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Whether changing a setting requires a restart, and of what.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Restart {
    /// No restart is needed.
    #[default]
    None,
    /// One named service must restart.
    Service {
        /// The service that must restart.
        name: String,
    },
    /// The whole host must restart.
    Host,
}

impl Restart {
    /// Whether any restart is required.
    #[must_use]
    pub fn needed(&self) -> bool {
        !matches!(self, Restart::None)
    }

    /// A short human sentence, or `None` when no restart is needed.
    #[must_use]
    pub fn describe(&self) -> Option<String> {
        match self {
            Restart::None => None,
            Restart::Service { name } => Some(format!("restart the {name} service")),
            Restart::Host => Some("restart the host".to_owned()),
        }
    }
}

/// Deprecation metadata for a setting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deprecated {
    /// The version since which the setting is deprecated.
    pub since: String,
    /// The key to use instead.
    pub use_instead: String,
}

/// A conditional dependency: the setting is relevant only while another key holds a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependsOn {
    /// The key this setting depends on.
    pub key: String,
    /// The value that key must hold.
    pub equals: ParamValue,
}

/// A secret value.
///
/// The text is retained (a real store persists it), but it is never disclosed by `Debug` or
/// serde: both render `<redacted>`. Only [`SecretValue::expose`] returns the text, and the
/// resolution, diff, screen, summary and hash paths never call it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps `text` as a secret.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The secret text. A store persists it; nothing that displays or hashes a value calls it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for SecretValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(SecretValue)
    }
}

/// A value as held by a [`ValueStore`](crate::store::ValueStore): a plain value or a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredValue {
    /// A plain, displayable value.
    Value(ParamValue),
    /// A secret value, always rendered `<redacted>`.
    Secret(SecretValue),
}

/// A value as shown to the console: a plain value or the fact that a secret is set.
///
/// Unlike [`StoredValue`], this type cannot hold a secret's text, so no resolution, diff or
/// screen result can leak one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveValue {
    /// A plain value.
    Value(ParamValue),
    /// A secret is set; its text is not disclosed.
    Redacted,
}

impl EffectiveValue {
    /// The plain value, or `None` for a redaction.
    #[must_use]
    pub fn as_value(&self) -> Option<&ParamValue> {
        match self {
            EffectiveValue::Value(value) => Some(value),
            EffectiveValue::Redacted => None,
        }
    }
}

/// Where an effective value came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The setting's default.
    Default,
    /// Explicitly set at the named scope.
    Set(ScopeRef),
}

/// One scope considered during resolution and what it held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Considered {
    /// The scope that was checked.
    pub scope: ScopeRef,
    /// What that scope held: `None` when unset, `Some(EffectiveValue::Redacted)` for a secret.
    pub stored: Option<EffectiveValue>,
}

/// The resolved value of a setting: the effective value, where it came from, and every scope
/// considered (so the console can say "set at group emea, overridden at server srv-17").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    /// The effective value.
    pub value: EffectiveValue,
    /// Where the effective value came from.
    pub origin: Origin,
    /// Every scope checked, broad to narrow.
    pub chain: Vec<Considered>,
}

/// A setting whose effective value differs from its default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Difference {
    /// The setting key.
    pub key: String,
    /// The effective value (`Redacted` for a set secret).
    pub value: EffectiveValue,
    /// Where the effective value was set.
    pub origin: Origin,
    /// The default value, `None` for a secret.
    pub default: Option<ParamValue>,
}

/// A hash of every explicitly set effective value, sorted by key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectiveHash(pub String);

impl fmt::Display for EffectiveHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
