//! The metadata text form of a sync policy: a DocType property, key/value, exactly like
//! [`spot_meta::custom::Property`].
//!
//! `spot-meta`'s property text form is `Property::parse(key, value)`: one key and one text value,
//! where a structured property (`lookup`, `masking`) carries TOML as its value. This crate mirrors
//! that shape so the sync policy **composes with customisation**: a site's `Property Setter`
//! record holds `(doctype, field = None, key, value)`, and [`PolicyProperty::parse`] reads the
//! same key and value. `spot-meta` does not yet carry the property (adding it is WP-SYN-01, ADR-063
//! is still *Proposed*), so this crate owns the parse and the caller passes the declarations to
//! [`crate::resolve`]; when the descriptor row lands, `Property::parse` calls straight into this.
//!
//! The keys are:
//!
//! | Key | Value |
//! | --- | --- |
//! | `sync_policy` | the whole policy as TOML, inline (`class = "master", direction = "down"`) or multi-line |
//! | `sync_class` | one class: `local`, `master`, `transactional`, `shared` |
//! | `sync_direction` | one direction: `up`, `down`, `both`, `none` |
//! | `sync_trigger` | one trigger: `on_submit`, `on_save`, `manual` |
//! | `sync_owner_scope` | a scope key type; an empty value clears it |
//! | `sync_exclude_fields` | field names, separated by commas or new lines; an empty value clears them |

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::error::PolicyError;
use crate::policy::{Class, Direction, FieldName, ScopeKeyType, SyncPolicy, Trigger};

/// The property key that carries a whole policy as TOML.
pub const SYNC_POLICY_KEY: &str = "sync_policy";
/// The property key that carries the class.
pub const SYNC_CLASS_KEY: &str = "sync_class";
/// The property key that carries the direction.
pub const SYNC_DIRECTION_KEY: &str = "sync_direction";
/// The property key that carries the trigger.
pub const SYNC_TRIGGER_KEY: &str = "sync_trigger";
/// The property key that carries the owner scope key type.
pub const SYNC_OWNER_SCOPE_KEY: &str = "sync_owner_scope";
/// The property key that carries the excluded fields.
pub const SYNC_EXCLUDE_FIELDS_KEY: &str = "sync_exclude_fields";

/// Every sync policy property key.
pub const KEYS: [&str; 6] = [
    SYNC_POLICY_KEY,
    SYNC_CLASS_KEY,
    SYNC_DIRECTION_KEY,
    SYNC_TRIGGER_KEY,
    SYNC_OWNER_SCOPE_KEY,
    SYNC_EXCLUDE_FIELDS_KEY,
];

/// One parsed sync policy property: the whole policy, or one field of it.
///
/// Applying it to a [`SyncPolicy`] with [`PolicyProperty::apply_to`] is how a site composes a
/// policy from overrides: each property changes only its own field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyProperty {
    /// The whole policy, from the `sync_policy` key's TOML body.
    Policy(SyncPolicy),
    /// The class.
    Class(Class),
    /// The direction.
    Direction(Direction),
    /// The trigger.
    Trigger(Trigger),
    /// The owner scope key type; `None` clears it.
    OwnerScope(Option<ScopeKeyType>),
    /// The excluded fields; an empty set clears them.
    ExcludeFields(BTreeSet<FieldName>),
}

impl PolicyProperty {
    /// The key this property is written under.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            PolicyProperty::Policy(_) => SYNC_POLICY_KEY,
            PolicyProperty::Class(_) => SYNC_CLASS_KEY,
            PolicyProperty::Direction(_) => SYNC_DIRECTION_KEY,
            PolicyProperty::Trigger(_) => SYNC_TRIGGER_KEY,
            PolicyProperty::OwnerScope(_) => SYNC_OWNER_SCOPE_KEY,
            PolicyProperty::ExcludeFields(_) => SYNC_EXCLUDE_FIELDS_KEY,
        }
    }

    /// Parses one property from its key and text value, mirroring
    /// [`spot_meta::Property::parse`].
    ///
    /// # Errors
    ///
    /// [`PolicyError`] with [`crate::codes::UNKNOWN_PROPERTY`] for a key that is not a sync policy
    /// key, or [`crate::codes::BAD_VALUE`] for a value that does not fit it.
    pub fn parse(key: &str, value: &str) -> Result<Self, PolicyError> {
        match key {
            SYNC_POLICY_KEY => parse_policy(value).map(PolicyProperty::Policy),
            SYNC_CLASS_KEY => Class::parse(value)
                .map(PolicyProperty::Class)
                .ok_or_else(|| {
                    unknown_value(key, value, &["local", "master", "transactional", "shared"])
                }),
            SYNC_DIRECTION_KEY => Direction::parse(value)
                .map(PolicyProperty::Direction)
                .ok_or_else(|| unknown_value(key, value, &["up", "down", "both", "none"])),
            SYNC_TRIGGER_KEY => Trigger::parse(value)
                .map(PolicyProperty::Trigger)
                .ok_or_else(|| unknown_value(key, value, &["on_submit", "on_save", "manual"])),
            SYNC_OWNER_SCOPE_KEY => {
                if value.is_empty() {
                    Ok(PolicyProperty::OwnerScope(None))
                } else {
                    ScopeKeyType::new(value).map(|s| PolicyProperty::OwnerScope(Some(s)))
                }
            }
            SYNC_EXCLUDE_FIELDS_KEY => parse_fields(value).map(PolicyProperty::ExcludeFields),
            other => Err(PolicyError::unknown_property(other)),
        }
    }

    /// Applies this property to a policy, changing only its own field.
    pub fn apply_to(&self, policy: &mut SyncPolicy) {
        match self {
            PolicyProperty::Policy(p) => *policy = p.clone(),
            PolicyProperty::Class(c) => policy.class = *c,
            PolicyProperty::Direction(d) => policy.direction = *d,
            PolicyProperty::Trigger(t) => policy.trigger = *t,
            PolicyProperty::OwnerScope(s) => policy.owner_scope = s.clone(),
            PolicyProperty::ExcludeFields(fields) => policy.exclude_fields = fields.clone(),
        }
    }
}

/// The TOML body of a `sync_policy` property, before conversion.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyText {
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    owner_scope: Option<String>,
    #[serde(default)]
    exclude_fields: Option<Vec<String>>,
}

/// Reads the whole-policy TOML body: multi-line as written, or one inline `key = value, ...`
/// line (an inline table's body), exactly as `Property::parse` reads a lookup.
fn parse_policy(value: &str) -> Result<SyncPolicy, PolicyError> {
    let text: PolicyText = if value.contains('\n') {
        toml::from_str(value)
    } else {
        #[derive(Deserialize)]
        struct Inline {
            sync_policy: PolicyText,
        }
        toml::from_str::<Inline>(&format!("sync_policy = {{ {value} }}")).map(|i| i.sync_policy)
    }
    .map_err(|e| PolicyError::bad_value(SYNC_POLICY_KEY, first_line(&e.to_string())))?;

    let mut policy = SyncPolicy::local();
    if let Some(class) = text.class {
        policy.class = Class::parse(&class).ok_or_else(|| {
            unknown_value(
                SYNC_CLASS_KEY,
                &class,
                &["local", "master", "transactional", "shared"],
            )
        })?;
    }
    if let Some(direction) = text.direction {
        policy.direction = Direction::parse(&direction).ok_or_else(|| {
            unknown_value(
                SYNC_DIRECTION_KEY,
                &direction,
                &["up", "down", "both", "none"],
            )
        })?;
    }
    if let Some(trigger) = text.trigger {
        policy.trigger = Trigger::parse(&trigger).ok_or_else(|| {
            unknown_value(
                SYNC_TRIGGER_KEY,
                &trigger,
                &["on_submit", "on_save", "manual"],
            )
        })?;
    }
    if let Some(owner_scope) = text.owner_scope
        && !owner_scope.is_empty()
    {
        policy.owner_scope = Some(ScopeKeyType::new(&owner_scope)?);
    }
    if let Some(fields) = text.exclude_fields {
        let mut set = BTreeSet::new();
        for field in fields {
            set.insert(FieldName::new(&field)?);
        }
        policy.exclude_fields = set;
    }
    Ok(policy)
}

/// Splits excluded fields at commas and new lines, trims each, drops the empty, validates the
/// rest.
fn parse_fields(value: &str) -> Result<BTreeSet<FieldName>, PolicyError> {
    let mut set = BTreeSet::new();
    for part in value.split([',', '\n']) {
        let name = part.trim();
        if name.is_empty() {
            continue;
        }
        set.insert(FieldName::new(name)?);
    }
    Ok(set)
}

/// A bad-value error naming the allowed texts.
fn unknown_value(key: &str, value: &str, allowed: &[&str]) -> PolicyError {
    PolicyError::bad_value(
        key,
        format!("`{value}` is not one of {}", allowed.join(", ")),
    )
}

/// Drops the noisy tail of a TOML error (the "expected one of ..." part and internal locations).
fn first_line(text: &str) -> String {
    match text.split_once('\n') {
        Some((first, _)) => first.trim_end().to_owned(),
        None => text.trim_end().to_owned(),
    }
}
