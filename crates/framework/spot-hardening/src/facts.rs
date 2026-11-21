//! Where facts come from: the [`FactSource`] trait the agent modules implement, and an
//! in-memory [`Facts`] set for callers that already hold the values.
//!
//! Evaluation reads facts through this one trait, so a live agent, a cached snapshot and a
//! test's fake are the same code path. The source is synchronous and returns one fact at a
//! time; a missing fact is `Ok(None)` (the control is then not applicable), while a source
//! that cannot answer at all returns an error (the control's outcome is then an error).

use std::collections::BTreeMap;

use crate::error::HardeningError;
use crate::model::{FactKey, FactValue};

/// Something that can report the facts a baseline's checks read.
pub trait FactSource {
    /// The value of `key`, or `None` when this host does not report it.
    ///
    /// # Errors
    ///
    /// [`HardeningError::FactUnavailable`] when the source cannot answer (the agent is not
    /// enrolled, a command failed, a file could not be read). The error never carries a
    /// secret or a raw command output.
    fn fact(&self, key: &FactKey) -> Result<Option<FactValue>, HardeningError>;
}

/// A fixed, in-memory set of facts.
///
/// Useful for a caller that already collected the values (a cached report, a batch of facts
/// from one agent call) and for tests. Iteration is in key order, so a report built from it is
/// deterministic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    values: BTreeMap<FactKey, FactValue>,
}

impl Facts {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key` to `value`.
    pub fn set(&mut self, key: FactKey, value: FactValue) {
        self.values.insert(key, value);
    }

    /// Sets `key` to `value`, for chaining.
    #[must_use]
    pub fn with(mut self, key: FactKey, value: FactValue) -> Self {
        self.set(key, value);
        self
    }

    /// The value of `key`, when set.
    #[must_use]
    pub fn get(&self, key: &FactKey) -> Option<&FactValue> {
        self.values.get(key)
    }

    /// Every fact, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&FactKey, &FactValue)> {
        self.values.iter()
    }
}

impl FactSource for Facts {
    fn fact(&self, key: &FactKey) -> Result<Option<FactValue>, HardeningError> {
        Ok(self.values.get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_facts_set_answers_through_the_trait() {
        let key = FactKey::try_new("ssh.enabled").unwrap();
        let facts = Facts::new().with(key.clone(), FactValue::Bool(false));
        assert_eq!(facts.get(&key), Some(&FactValue::Bool(false)));
        assert_eq!(
            FactSource::fact(&facts, &key).unwrap(),
            Some(FactValue::Bool(false))
        );
        let absent = FactKey::try_new("mac.mode").unwrap();
        assert_eq!(FactSource::fact(&facts, &absent).unwrap(), None);
    }

    #[test]
    fn facts_iterate_in_key_order() {
        let facts = Facts::new()
            .with(
                FactKey::try_new("time.synchronised").unwrap(),
                FactValue::Bool(true),
            )
            .with(
                FactKey::try_new("mac.mode").unwrap(),
                FactValue::Text("enforcing".to_owned()),
            );
        let keys: Vec<&str> = facts.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["mac.mode", "time.synchronised"]);
    }
}
