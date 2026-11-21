//! A fake server: an in-memory fact set with modelled remediation effects.
//!
//! The agent applies an [`OpRef`] to a real host; this fake applies it to a map, so a test (or
//! a console preview) can run the whole loop — evaluate, plan, apply, re-evaluate — without a
//! host. An effect is the fact change an operation is modelled to cause; [`FakeServer::apply`]
//! looks the operation up by id and writes its effect. An operation with no modelled effect is
//! left unapplied and reported as `false`, so a test never silently passes on a missing effect.
//!
//! The fake is deterministic: facts iterate in key order and an effect replaces the facts it
//! names.

use std::collections::BTreeMap;

use crate::error::HardeningError;
use crate::facts::FactSource;
use crate::model::{FactKey, FactValue, OpRef, OsFamily};

/// An in-memory server that answers facts and can be remediated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FakeServer {
    os_family: Option<OsFamily>,
    facts: BTreeMap<FactKey, FactValue>,
    effects: BTreeMap<String, Vec<(FactKey, FactValue)>>,
}

impl FakeServer {
    /// An empty fake for `os_family`.
    #[must_use]
    pub fn new(os_family: OsFamily) -> Self {
        Self {
            os_family: Some(os_family),
            facts: BTreeMap::new(),
            effects: BTreeMap::new(),
        }
    }

    /// The OS family this fake models.
    #[must_use]
    pub fn os_family(&self) -> Option<OsFamily> {
        self.os_family
    }

    /// Sets `key` to `value`.
    pub fn set(&mut self, key: FactKey, value: FactValue) {
        self.facts.insert(key, value);
    }

    /// Sets `key` to `value`, for chaining.
    #[must_use]
    pub fn with(mut self, key: FactKey, value: FactValue) -> Self {
        self.set(key, value);
        self
    }

    /// Models the effect of `op`: applying it writes each `(key, value)`.
    #[must_use]
    pub fn with_effect(mut self, op: &str, effect: Vec<(FactKey, FactValue)>) -> Self {
        self.effects.insert(op.to_owned(), effect);
        self
    }

    /// The value of `key`, when set.
    #[must_use]
    pub fn get(&self, key: &FactKey) -> Option<&FactValue> {
        self.facts.get(key)
    }

    /// Applies `op` to the fake, writing its modelled effect.
    ///
    /// Returns `true` when the operation has a modelled effect and was applied, `false` when
    /// no effect is modelled (the fake is unchanged, so a test can assert the effect exists).
    pub fn apply(&mut self, op: &OpRef) -> bool {
        let Some(effect) = self.effects.get(op.op.as_str()) else {
            return false;
        };
        for (key, value) in effect {
            self.facts.insert(key.clone(), value.clone());
        }
        true
    }
}

impl FactSource for FakeServer {
    fn fact(&self, key: &FactKey) -> Result<Option<FactValue>, HardeningError> {
        Ok(self.facts.get(key).cloned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::{FactKey, FactValue};
    use spot_mgmt_ops::OpId;

    #[test]
    fn apply_writes_the_modelled_effect() {
        let enabled = FactKey::try_new("ssh.enabled").unwrap();
        let mut server = FakeServer::new(OsFamily::Linux)
            .with(enabled.clone(), FactValue::Bool(true))
            .with_effect(
                "os.service.disable",
                vec![(enabled.clone(), FactValue::Bool(false))],
            );
        assert_eq!(server.get(&enabled), Some(&FactValue::Bool(true)));
        let op = OpRef::new(OpId::new("os.service.disable").unwrap());
        assert!(server.apply(&op));
        assert_eq!(server.get(&enabled), Some(&FactValue::Bool(false)));
    }

    #[test]
    fn an_unmodelled_operation_is_not_applied() {
        let mut server = FakeServer::new(OsFamily::Linux);
        let op = OpRef::new(OpId::new("os.firewall.close").unwrap());
        assert!(!server.apply(&op));
    }

    #[test]
    fn the_fake_answers_through_the_fact_source_trait() {
        let key = FactKey::try_new("time.synchronised").unwrap();
        let server = FakeServer::new(OsFamily::Linux).with(key.clone(), FactValue::Bool(false));
        assert_eq!(
            FactSource::fact(&server, &key).unwrap(),
            Some(FactValue::Bool(false))
        );
        let absent = FactKey::try_new("mac.mode").unwrap();
        assert_eq!(FactSource::fact(&server, &absent).unwrap(), None);
    }
}
