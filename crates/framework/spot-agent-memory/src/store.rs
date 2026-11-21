//! The store seam: one `MemoryStore` trait for the three kinds, and a deterministic in-memory
//! implementation.
//!
//! This crate is the **model and the logic**. The PostgreSQL/pgvector implementation is the next
//! task (WP-AI-09.T2): it plugs in behind this trait, and nothing here knows about SQL, a
//! connection pool or a vector extension.
//!
//! The trait is deliberately small: put one entry, read one, list a tenant's entries of a kind,
//! remove one, list the tenants. Recall, retention and the learning loop are built on those five
//! calls, so a backend has no second behaviour to keep in step with the in-memory store.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value as Json;

use crate::episode::Episode;
use crate::error::MemoryError;
use crate::fact::Fact;
use crate::id::TenantId;
use crate::meta::{EntryMeta, MemoryKind};
use crate::procedure::Procedure;

/// One entry of any kind. The three variants are the whole vocabulary: there is no fourth shape
/// a document payload could be smuggled through.
///
/// Serialised untagged, so an export carries the entry's own fields (the export groups by kind).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Entry {
    /// What happened.
    Episode(Episode),
    /// How to fix it.
    Procedure(Procedure),
    /// What the installation is.
    Fact(Fact),
}

impl Entry {
    /// The entry's kind.
    #[must_use]
    pub fn kind(&self) -> MemoryKind {
        match self {
            Self::Episode(episode) => episode.kind(),
            Self::Procedure(procedure) => procedure.kind(),
            Self::Fact(fact) => fact.kind(),
        }
    }

    /// The entry's id text.
    #[must_use]
    pub fn id_str(&self) -> &str {
        match self {
            Self::Episode(episode) => episode.id_str(),
            Self::Procedure(procedure) => procedure.id_str(),
            Self::Fact(fact) => fact.id_str(),
        }
    }

    /// The metadata every entry carries.
    #[must_use]
    pub fn meta(&self) -> &EntryMeta {
        match self {
            Self::Episode(episode) => &episode.meta,
            Self::Procedure(procedure) => &procedure.meta,
            Self::Fact(fact) => &fact.meta,
        }
    }

    /// The stored embedding.
    #[must_use]
    pub fn vector(&self) -> &[f32] {
        match self {
            Self::Episode(episode) => &episode.vector,
            Self::Procedure(procedure) => &procedure.vector,
            Self::Fact(fact) => &fact.vector,
        }
    }

    /// The text a recall hit shows: the episode's summary, the procedure's title, the fact's key
    /// and value.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Episode(episode) => episode.text().to_owned(),
            Self::Procedure(procedure) => procedure.text().to_owned(),
            Self::Fact(fact) => fact.text(),
        }
    }

    /// The structured symptom signature, where the kind has one.
    #[must_use]
    pub fn symptoms(&self) -> &[crate::id::Symptom] {
        match self {
            Self::Episode(episode) => &episode.symptoms,
            Self::Procedure(procedure) => &procedure.symptoms,
            Self::Fact(_) => &[],
        }
    }

    /// The remedies the entry names: a procedure's steps, an episode's actions.
    #[must_use]
    pub fn remedies(&self) -> Vec<&spot_remedies::RemedyId> {
        match self {
            Self::Episode(episode) => episode
                .actions
                .iter()
                .filter_map(|action| action.remedy.as_ref())
                .collect(),
            Self::Procedure(procedure) => procedure.steps.iter().map(|step| &step.remedy).collect(),
            Self::Fact(_) => Vec::new(),
        }
    }

    /// Checks the entry's own structure and its kind's rules.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] naming the rule that failed.
    pub fn validate(&self) -> Result<(), MemoryError> {
        match self {
            Self::Episode(episode) => episode.validate(),
            Self::Procedure(procedure) => procedure.validate(),
            Self::Fact(fact) => fact.validate(),
        }
    }

    /// The entry as JSON, for an export or a support bundle.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Serialise`] when a value cannot be rendered.
    pub fn to_json(&self) -> Result<Json, MemoryError> {
        serde_json::to_value(self).map_err(|error| MemoryError::Serialise {
            what: format!("{} entry `{}`", self.kind(), self.id_str()),
            detail: error.to_string(),
        })
    }
}

impl From<Episode> for Entry {
    fn from(episode: Episode) -> Self {
        Self::Episode(episode)
    }
}

impl From<Procedure> for Entry {
    fn from(procedure: Procedure) -> Self {
        Self::Procedure(procedure)
    }
}

impl From<Fact> for Entry {
    fn from(fact: Fact) -> Self {
        Self::Fact(fact)
    }
}

/// Where memory lives. One trait for the three kinds; T2 implements it over PostgreSQL.
///
/// Every method is tenant-scoped: `list` never returns another tenant's entries, and `get` is
/// keyed by tenant, so episodic memory cannot cross tenants even by a caller's mistake.
pub trait MemoryStore: Send + Sync {
    /// Stores `entry`, replacing an entry of the same tenant, kind and id.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] when the backend refuses, and the entry's own validation error
    /// when it is not storable.
    fn put(&self, entry: Entry) -> Result<(), MemoryError>;

    /// The entry of `kind` with `id` in `tenant`, if it is there.
    fn get(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Option<Entry>;

    /// Every entry of `kind` in `tenant`, ordered by id.
    fn list(&self, tenant: &TenantId, kind: MemoryKind) -> Vec<Entry>;

    /// Removes one entry; `Ok(false)` when it was not there.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] when the backend refuses.
    fn remove(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Result<bool, MemoryError>;

    /// Every tenant the store holds at least one entry for, ordered.
    fn tenants(&self) -> Vec<TenantId>;
}

/// The store key: tenant, kind and id.
type StoreKey = (String, MemoryKind, String);

/// A deterministic in-memory [`MemoryStore`]: no backend, no clock, no randomness.
///
/// Entries come back ordered by id because the map is ordered; the same writes always give the
/// same listing, which is what the recall, retention and learning tests rely on.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    entries: Mutex<BTreeMap<StoreKey, Entry>>,
}

impl InMemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries the store holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the store holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// How many entries of one kind a tenant has.
    #[must_use]
    pub fn count(&self, tenant: &TenantId, kind: MemoryKind) -> usize {
        self.lock()
            .keys()
            .filter(|(owner, entry_kind, _)| owner == tenant.as_str() && *entry_kind == kind)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<StoreKey, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl MemoryStore for InMemoryStore {
    fn put(&self, entry: Entry) -> Result<(), MemoryError> {
        entry.validate()?;
        let key = (
            entry.meta().tenant.as_str().to_owned(),
            entry.kind(),
            entry.id_str().to_owned(),
        );
        self.lock().insert(key, entry);
        Ok(())
    }

    fn get(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Option<Entry> {
        self.lock()
            .get(&(tenant.as_str().to_owned(), kind, id.to_owned()))
            .cloned()
    }

    fn list(&self, tenant: &TenantId, kind: MemoryKind) -> Vec<Entry> {
        self.lock()
            .iter()
            .filter(|((owner, entry_kind, _), _)| owner == tenant.as_str() && *entry_kind == kind)
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    fn remove(&self, tenant: &TenantId, kind: MemoryKind, id: &str) -> Result<bool, MemoryError> {
        let removed = self
            .lock()
            .remove(&(tenant.as_str().to_owned(), kind, id.to_owned()))
            .is_some();
        Ok(removed)
    }

    fn tenants(&self) -> Vec<TenantId> {
        let mut names: Vec<String> = self
            .lock()
            .keys()
            .map(|(owner, _, _)| owner.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .filter_map(|name| TenantId::parse(&name).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, InMemoryStore, MemoryStore};
    use crate::id::TenantId;
    use crate::meta::MemoryKind;
    use crate::testing::{episode, procedure};

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    #[test]
    fn a_store_keeps_kinds_and_tenants_apart() {
        let store = InMemoryStore::new();
        store.put(Entry::Episode(episode("tenant_a", 1))).unwrap();
        store
            .put(Entry::Procedure(procedure("tenant_a", 1)))
            .unwrap();
        store.put(Entry::Episode(episode("tenant_b", 1))).unwrap();

        assert_eq!(store.len(), 3);
        assert_eq!(store.count(&tenant("tenant_a"), MemoryKind::Episode), 1);
        assert_eq!(store.count(&tenant("tenant_b"), MemoryKind::Episode), 1);
        assert_eq!(store.tenants(), [tenant("tenant_a"), tenant("tenant_b")]);

        let listed = store.list(&tenant("tenant_a"), MemoryKind::Episode);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].meta().tenant, tenant("tenant_a"));
        assert!(
            store
                .get(&tenant("tenant_a"), MemoryKind::Procedure, "procedure.p1")
                .is_some()
        );
        assert!(
            store
                .get(&tenant("tenant_b"), MemoryKind::Procedure, "procedure.p1")
                .is_none()
        );
    }

    #[test]
    fn a_listing_is_ordered_by_id_and_a_removal_is_reported() {
        let store = InMemoryStore::new();
        store.put(Entry::Episode(episode("tenant_a", 2))).unwrap();
        store.put(Entry::Episode(episode("tenant_a", 1))).unwrap();
        let ids: Vec<String> = store
            .list(&tenant("tenant_a"), MemoryKind::Episode)
            .iter()
            .map(|entry| entry.id_str().to_owned())
            .collect();
        assert_eq!(ids, ["episode.e1", "episode.e2"]);

        assert!(
            store
                .remove(&tenant("tenant_a"), MemoryKind::Episode, "episode.e1")
                .unwrap()
        );
        assert!(
            !store
                .remove(&tenant("tenant_a"), MemoryKind::Episode, "episode.e1")
                .unwrap()
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn an_invalid_entry_never_reaches_the_store() {
        let store = InMemoryStore::new();
        let mut entry = episode("tenant_a", 1);
        entry.symptoms.clear();
        let error = store.put(Entry::Episode(entry)).unwrap_err();
        assert_eq!(error.code(), "agent_memory.entry_invalid");
        assert!(store.is_empty());
    }

    #[test]
    fn an_entry_serialises_only_its_own_kinds_fields() {
        let entry = Entry::Episode(episode("tenant_a", 1));
        let json = entry.to_json().unwrap();
        let object = json.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "actions", "audits", "id", "incident", "meta", "outcome", "summary", "symptoms",
                "vector"
            ],
            "no field on an episode can hold a document payload"
        );
    }
}
