//! The providers the hub relay takes per destination (WP-RTB-02.T10): the scope keys a record
//! requires ([`PolicyScopeKeys`], the `ScopeKeysOf` seam) and the destination itself
//! ([`DestinationFactory`]: a `DestinationScope` over the destination's subscription, and a
//! redactor over the policy generation that was current when it was built).
//!
//! # Where each fact comes from
//!
//! * which DocTypes sync, which way, and which scope key type owns a record: the **policy**
//!   generation that passed its lints at load (a refused generation never reaches here);
//! * which field of a record carries the value of a scope key: the site's configuration
//!   (`(DocType, key type) -> field`, the same table the partition owner check uses);
//! * the value itself: the hub's own stored copy of the record ([`FieldReader`]);
//! * what a destination may receive: its **hub-side** subscription, read from the registry. Nothing
//!   a node sent is read, so a node cannot widen its own scope.
//!
//! # Fail closed
//!
//! A record whose DocType has no policy or is `local`, whose owner scope field is not configured or
//! has no value, or whose value is not a valid scope key, answers "not known" (`None`): the
//! relay treats that as **not covered**. A record that is synced with no owner scope is for every
//! subscriber of its DocType (`Some(vec![])`), exactly what the adapters document.
//!
//! # A new generation
//!
//! A destination built here holds the generation current at its construction. When a new
//! generation loads, the service rebuilds every destination (re-reading the registry's
//! subscriptions), so a policy or subscription change reaches the relay at the next rebuild and a
//! refused generation changes nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use spot_sync_closure::RecordKey;
use spot_sync_closure::Redactor;
use spot_sync_hub::{Destination, DestinationScope, StreamEntry};
use spot_sync_hub_adapters::{MasterScope, PolicyRedactor, ScopeKeysOf, encode_scope_key};
use spot_sync_master::{ScopeKey, Subscription};
use spot_sync_skew::NodeReport;

use crate::Snapshot;

/// Reads the value a record carries for a scope key, from the hub's own copy of the record.
pub trait FieldReader: Send + Sync {
    /// The text value of `field` of record `entity`/`id`, or `None` when the record or the field
    /// is unknown or empty.
    fn scope_value(&self, entity: &str, id: &str, field: &str) -> Option<String>;
}

/// The scope keys each record requires, from the policy and the configured scope fields.
pub struct PolicyScopeKeys {
    snapshot: Arc<Snapshot>,
    fields: BTreeMap<(String, String), String>,
    reader: Arc<dyn FieldReader>,
}

impl PolicyScopeKeys {
    /// Keys for `snapshot`. `fields` maps `(DocType, key type)` to the field that supplies the
    /// key's value.
    #[must_use]
    pub fn new(
        snapshot: Arc<Snapshot>,
        fields: BTreeMap<(String, String), String>,
        reader: Arc<dyn FieldReader>,
    ) -> Self {
        Self {
            snapshot,
            fields,
            reader,
        }
    }
}

impl std::fmt::Debug for PolicyScopeKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyScopeKeys")
            .field("fields", &self.fields.len())
            .finish_non_exhaustive()
    }
}

impl ScopeKeysOf for PolicyScopeKeys {
    fn scope_keys(&self, entry_key: &RecordKey) -> Option<Vec<String>> {
        let policy = self.snapshot.policy(&entry_key.entity)?;
        if !policy.class.is_synced() {
            return None;
        }
        let Some(key_type) = &policy.owner_scope else {
            return Some(Vec::new());
        };
        let field = self
            .fields
            .get(&(entry_key.entity.clone(), key_type.as_str().to_owned()))?;
        let value = self
            .reader
            .scope_value(&entry_key.entity, &entry_key.id, field)?;
        let key = ScopeKey::new(key_type.as_str(), &value).ok()?;
        encode_scope_key(&key).map(|text| vec![text])
    }
}

/// A [`DestinationScope`] that owns what it reads: the destination's subscription and the policy
/// keys. (`MasterScope` borrows its subscription; the relay holds its destinations for ever.)
pub struct OwnedScope {
    subscription: Arc<Subscription>,
    keys: Arc<PolicyScopeKeys>,
}

impl OwnedScope {
    /// A scope for the destination whose registry subscription is `subscription`.
    #[must_use]
    pub fn new(subscription: Arc<Subscription>, keys: Arc<PolicyScopeKeys>) -> Self {
        Self { subscription, keys }
    }

    fn view(&self) -> MasterScope<'_, &PolicyScopeKeys> {
        MasterScope::new(&self.subscription, &*self.keys)
    }
}

impl DestinationScope for OwnedScope {
    fn covers(&self, entry: &StreamEntry) -> bool {
        // A record may only go DOWN what the policy sends down: a DocType that flows up only never
        // reaches a destination through this scope.
        self.keys.snapshot.may_leave(&entry.key.entity, false) && self.view().covers(entry)
    }

    fn covers_key(&self, key: &RecordKey) -> bool {
        self.keys.snapshot.may_leave(&key.entity, false) && self.view().covers_key(key)
    }

    fn owns(&self, key: &RecordKey) -> bool {
        self.view().owns(key)
    }
}

/// A redactor that owns the policy book it reads (`PolicyRedactor` borrows it).
pub struct OwnedRedactor {
    snapshot: Arc<Snapshot>,
}

impl OwnedRedactor {
    /// A redactor over `snapshot`.
    #[must_use]
    pub fn new(snapshot: Arc<Snapshot>) -> Self {
        Self { snapshot }
    }
}

impl Redactor for OwnedRedactor {
    fn redact_down(&self, entity: &str, image: &mut Value) {
        PolicyRedactor::new(self.snapshot.book()).redact_down(entity, image);
    }
}

/// Builds relay destinations from the current policy generation and a subscription.
pub struct DestinationFactory {
    keys: Arc<PolicyScopeKeys>,
    snapshot: Arc<Snapshot>,
}

impl DestinationFactory {
    /// A factory over `keys` (which holds the generation).
    #[must_use]
    pub fn new(keys: PolicyScopeKeys) -> Self {
        let snapshot = Arc::clone(&keys.snapshot);
        Self {
            keys: Arc::new(keys),
            snapshot,
        }
    }

    /// The destination `id` for `subscription`, reporting `report` at its handshake.
    #[must_use]
    pub fn destination(
        &self,
        id: &str,
        subscription: Subscription,
        report: NodeReport,
    ) -> Destination {
        Destination::new(
            id,
            Box::new(OwnedScope::new(
                Arc::new(subscription),
                Arc::clone(&self.keys),
            )),
            Box::new(OwnedRedactor::new(Arc::clone(&self.snapshot))),
            report,
        )
    }
}
