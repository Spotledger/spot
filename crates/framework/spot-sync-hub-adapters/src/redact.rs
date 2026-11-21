//! The closure `Redactor` over `spot-sync-policy` (WP-SYN-02.T9, ADR-063 section 3).
//!
//! The closure builder redacts every image before it is queued. The rule of what is excluded is
//! [`SyncPolicy::excludes`]; this module applies it to the JSON journal image (parent fields plus
//! child tables as arrays of row objects), giving the same result as `outgoing_view` does on a
//! `Document`: every excluded top-level field and child table is absent, and a field named like
//! an excluded one inside a child row is absent too. Everything else is untouched, key order
//! included.
//!
//! # Fail closed
//!
//! An entity with no policy in the [`PolicyBook`] has its image replaced by `Value::Null`, and the
//! entity is recorded ([`PolicyRedactor::unredacted_entities`]). A policy that cannot be found
//! never means "send everything".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::Value;
use spot_sync_closure::Redactor;
use spot_sync_policy::SyncPolicy;

/// Entity name to its [`SyncPolicy`], built from the catalog by the caller.
#[derive(Debug, Clone, Default)]
pub struct PolicyBook {
    policies: BTreeMap<String, SyncPolicy>,
}

impl PolicyBook {
    /// An empty book: every entity is uncovered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the policy of an entity, replacing an earlier one.
    pub fn insert(&mut self, entity: impl Into<String>, policy: SyncPolicy) {
        self.policies.insert(entity.into(), policy);
    }

    /// The policy of an entity, if the book has one.
    #[must_use]
    pub fn policy(&self, entity: &str) -> Option<&SyncPolicy> {
        self.policies.get(entity)
    }

    /// The entities of a destination's scope that have no policy, sorted and without duplicates,
    /// so the hub can refuse to start a destination whose scope is not fully covered.
    #[must_use]
    pub fn missing_for(&self, entities: &[String]) -> Vec<String> {
        let missing: BTreeSet<&String> = entities
            .iter()
            .filter(|e| !self.policies.contains_key(e.as_str()))
            .collect();
        missing.into_iter().cloned().collect()
    }
}

impl FromIterator<(String, SyncPolicy)> for PolicyBook {
    fn from_iter<I: IntoIterator<Item = (String, SyncPolicy)>>(iter: I) -> Self {
        Self {
            policies: iter.into_iter().collect(),
        }
    }
}

/// A [`Redactor`] that removes what a [`PolicyBook`] excludes and fails closed for an entity
/// without a policy.
#[derive(Debug)]
pub struct PolicyRedactor<'a> {
    book: &'a PolicyBook,
    unredacted: Mutex<BTreeSet<String>>,
}

impl<'a> PolicyRedactor<'a> {
    /// A redactor over the given book.
    #[must_use]
    pub fn new(book: &'a PolicyBook) -> Self {
        Self {
            book,
            unredacted: Mutex::new(BTreeSet::new()),
        }
    }

    /// The sorted set of entities that were redacted to `Null` because they had no policy, so the
    /// hub can raise an operator warning.
    #[must_use]
    pub fn unredacted_entities(&self) -> Vec<String> {
        let set = self
            .unredacted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.iter().cloned().collect()
    }
}

impl Redactor for PolicyRedactor<'_> {
    fn redact_down(&self, entity: &str, image: &mut Value) {
        match self.book.policy(entity) {
            Some(policy) => strip(image, policy),
            None => {
                *image = Value::Null;
                self.unredacted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(entity.to_owned());
            }
        }
    }
}

/// Remove excluded top-level fields and child tables, and excluded names inside child rows. An
/// image that is not an object becomes `Null`.
fn strip(image: &mut Value, policy: &SyncPolicy) {
    let Value::Object(map) = image else {
        // A journal image is always an object. Anything else cannot be checked against the
        // policy, so it is not sent (fail closed), like an entity without a policy.
        *image = Value::Null;
        return;
    };
    map.retain(|name, _| !policy.excludes(name));
    for value in map.values_mut() {
        if let Value::Array(rows) = value {
            for row in rows {
                if let Value::Object(fields) = row {
                    fields.retain(|name, _| !policy.excludes(name));
                }
            }
        }
    }
}
