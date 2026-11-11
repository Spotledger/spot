//! The policy value: [`SyncPolicy`] and the small types it is made of.
//!
//! The four fields are the ones the blueprint names (WS10, WP-EDG-08): `class`, `owner_scope`,
//! `direction`, `trigger`, plus the `exclude_fields` set. The default is [`Class::Local`] with
//! [`Direction::None`] — *nothing syncs unless declared* (ADR-062 rule 1).

use std::collections::BTreeSet;
use std::fmt;

use crate::error::PolicyError;

/// The class of data a DocType carries (blueprint WS10). The class fixes who owns a record and
/// which way it may flow; the [`Direction`] must agree with it, which [`crate::lint`] enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    /// Stays on the node where it lives; nothing syncs. **The default.**
    Local,
    /// Hub-owned reference data the network shares, sent down to nodes in scope.
    Master,
    /// A transaction that happens at a node, sent up to the hub.
    Transactional,
    /// Data whose ownership is held per scope key and may flow both ways.
    Shared,
}

impl Class {
    /// Every class, in declaration order.
    pub const ALL: [Class; 4] = [
        Class::Local,
        Class::Master,
        Class::Transactional,
        Class::Shared,
    ];

    /// The text form used in metadata, for example `local`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Local => "local",
            Class::Master => "master",
            Class::Transactional => "transactional",
            Class::Shared => "shared",
        }
    }

    /// Parses the text form. Case-sensitive: metadata is lower-case by convention.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "local" => Some(Class::Local),
            "master" => Some(Class::Master),
            "transactional" => Some(Class::Transactional),
            "shared" => Some(Class::Shared),
            _ => None,
        }
    }

    /// Whether this class syncs at all (every class but [`Class::Local`]).
    #[must_use]
    pub fn is_synced(self) -> bool {
        !matches!(self, Class::Local)
    }
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which way a policy's records may flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// From the creating node up to the hub.
    Up,
    /// From the hub down to nodes in scope.
    Down,
    /// Both ways.
    Both,
    /// Nowhere. The default, and the only direction a [`Class::Local`] policy may carry.
    None,
}

impl Direction {
    /// Every direction, in declaration order.
    pub const ALL: [Direction; 4] = [
        Direction::Up,
        Direction::Down,
        Direction::Both,
        Direction::None,
    ];

    /// The text form used in metadata, for example `up`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Both => "both",
            Direction::None => "none",
        }
    }

    /// Parses the text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "up" => Some(Direction::Up),
            "down" => Some(Direction::Down),
            "both" => Some(Direction::Both),
            "none" => Some(Direction::None),
            _ => None,
        }
    }

    /// Whether records may flow up (to the hub).
    #[must_use]
    pub fn has_up(self) -> bool {
        matches!(self, Direction::Up | Direction::Both)
    }

    /// Whether records may flow down (from the hub).
    #[must_use]
    pub fn has_down(self) -> bool {
        matches!(self, Direction::Down | Direction::Both)
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// When a record becomes eligible to sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// On submission (the default; a draft stays local). The sync unit is a submitted version.
    OnSubmit,
    /// On every save, draft or not.
    OnSave,
    /// Only when an operator asks, explicitly.
    Manual,
}

impl Trigger {
    /// Every trigger, in declaration order.
    pub const ALL: [Trigger; 3] = [Trigger::OnSubmit, Trigger::OnSave, Trigger::Manual];

    /// The text form used in metadata, for example `on_submit`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::OnSubmit => "on_submit",
            Trigger::OnSave => "on_save",
            Trigger::Manual => "manual",
        }
    }

    /// Parses the text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "on_submit" => Some(Trigger::OnSubmit),
            "on_save" => Some(Trigger::OnSave),
            "manual" => Some(Trigger::Manual),
            _ => None,
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered scope key type, for example `warehouse` or `company`.
///
/// The platform never interprets it (ADR-062 section 1): this type only fixes its syntax, so two
/// policies written in two places agree on what "the same key type" means. The meaning of a key
/// type is ADR-063, still *Proposed*.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScopeKeyType(String);

impl ScopeKeyType {
    /// Longest allowed key type, in bytes (the same limit `spot-partition`'s scope key parts
    /// use, so a policy's key type always fits one).
    pub const MAX_LEN: usize = 128;

    /// Validates and wraps a key type: 1 to [`ScopeKeyType::MAX_LEN`] printable bytes with no
    /// control characters.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] with [`crate::codes::BAD_VALUE`] when the text is empty, too long or
    /// holds a control character.
    pub fn new(text: &str) -> Result<Self, PolicyError> {
        let reason = if text.is_empty() {
            Some("it is empty")
        } else if text.len() > Self::MAX_LEN {
            Some("it is longer than the 128-byte limit")
        } else if text.chars().any(char::is_control) {
            Some("it holds a control character")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(PolicyError::bad_value(
                "sync_owner_scope",
                format!("the scope key type is invalid: {reason}"),
            )),
            None => Ok(Self(text.to_owned())),
        }
    }

    /// The key type as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ScopeKeyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A field name in an `exclude_fields` set.
///
/// Validated like a storage name: lower-case letters, digits and underscores, starting with a
/// letter, at most 63 bytes — a field that syncs is a column at the destination, so its name must
/// be a legal one there too.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldName(String);

impl FieldName {
    /// Longest allowed field name, in bytes.
    pub const MAX_LEN: usize = 63;

    /// Validates and wraps a field name.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] with [`crate::codes::BAD_VALUE`] when the text is not a storage name.
    pub fn new(text: &str) -> Result<Self, PolicyError> {
        let mut chars = text.chars();
        let valid = text.len() <= Self::MAX_LEN
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(PolicyError::bad_value(
                "sync_exclude_fields",
                format!(
                    "`{text}` is not a field name: use lower-case letters, digits and \
                     underscores, starting with a letter, at most 63 bytes"
                ),
            ))
        }
    }

    /// The field name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One DocType's sync policy: the four fields the blueprint names plus the excluded-field set.
///
/// The default ([`SyncPolicy::local`]) syncs nothing. A DocType opts in by carrying a policy in
/// its metadata (the text form of [`crate::text`]), never by code naming it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPolicy {
    /// The class of data, which fixes ownership and the allowed direction.
    pub class: Class,
    /// The scope key type the owning node must hold, when the policy flows upward.
    pub owner_scope: Option<ScopeKeyType>,
    /// Which way records may flow.
    pub direction: Direction,
    /// When a record becomes eligible to sync.
    pub trigger: Trigger,
    /// Fields that never leave the origin (or never go down / never go up; the direction is the
    /// policy's own).
    pub exclude_fields: BTreeSet<FieldName>,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self::local()
    }
}

impl SyncPolicy {
    /// The default policy: `local`, no owner scope, no direction, trigger on submit, nothing
    /// excluded. *Nothing syncs unless declared* (ADR-062 rule 1).
    #[must_use]
    pub fn local() -> Self {
        Self {
            class: Class::Local,
            owner_scope: None,
            direction: Direction::None,
            trigger: Trigger::OnSubmit,
            exclude_fields: BTreeSet::new(),
        }
    }

    /// A policy from its parts.
    #[must_use]
    pub fn new(
        class: Class,
        owner_scope: Option<ScopeKeyType>,
        direction: Direction,
        trigger: Trigger,
        exclude_fields: BTreeSet<FieldName>,
    ) -> Self {
        Self {
            class,
            owner_scope,
            direction,
            trigger,
            exclude_fields,
        }
    }

    /// Whether this policy syncs at all.
    #[must_use]
    pub fn is_synced(&self) -> bool {
        self.class.is_synced()
    }

    /// Whether the named field is excluded from sync.
    #[must_use]
    pub fn excludes(&self, field: &str) -> bool {
        self.exclude_fields.iter().any(|f| f.as_str() == field)
    }

    /// The excluded field names, in order.
    pub fn excluded(&self) -> impl Iterator<Item = &FieldName> {
        self.exclude_fields.iter()
    }
}
