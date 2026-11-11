//! The values a condition carries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The parameters a raised condition carries: the values its explanation template fills in
/// (ADR-067 section 1, `explanation`).
///
/// A name-to-text map, sorted by name, so the audit payload is byte-stable for the same
/// parameters and nothing here can carry a floating-point value (ADR-012) or a binary blob. A
/// number is inserted as its text; the template, not this type, knows what it means.
///
/// The engine does not police the names: [`ConditionDef::params`](crate::ConditionDef::params)
/// declares the names the explanation braces and
/// [`ConditionDef::missing_params`](crate::ConditionDef::missing_params) reports the ones a raise
/// left out, but an app may attach context of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConditionParams(BTreeMap<String, String>);

impl ConditionParams {
    /// No parameters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name` to `value`, replacing any earlier value.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.0.insert(name.to_owned(), value.into());
    }

    /// `self` with `name` set to `value`: the builder form, for definitions and tests.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.set(name, value);
        self
    }

    /// The value of `name`, if it is set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// The parameters in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// How many parameters there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no parameters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, String)> for ConditionParams {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
