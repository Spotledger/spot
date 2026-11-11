//! The node's durable store and the hub's record images.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value;
use spot_sync_closure::{Bundle, ClosureError, ImageSource, RecordKey, SourceImage};
use spot_sync_node::{AppliedRecord, AppliedSet, ApplyRefusal, BundleApply};

/// The durable records of every node in the world: one version per `(tenant, key)`.
///
/// It implements the node's apply seam ([`BundleApply`]) the way the real apply path will: all of
/// a bundle or none of it, keyed by `(tenant, key)` and versioned by the record's event position.
/// Replays are recognised (`duplicates`) and older versions ignored (`older_ignored`), which is
/// what makes the "nothing applied twice" assertion mean something.
#[derive(Clone, Debug, Default)]
pub struct MemStore {
    records: BTreeMap<(String, RecordKey), u64>,
    history: Vec<(String, RecordKey, u64)>,
    applies: u64,
    duplicates: u64,
    older_ignored: u64,
    refuse_next: Option<RecordKey>,
    refusals: u64,
}

impl MemStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The version of `key` for `tenant`, if the node holds it.
    #[must_use]
    pub fn version_of(&self, tenant: &str, key: &RecordKey) -> Option<u64> {
        self.records.get(&(tenant.to_owned(), key.clone())).copied()
    }

    /// Every record the tenant's node holds, by key.
    #[must_use]
    pub fn records_of(&self, tenant: &str) -> BTreeMap<RecordKey, u64> {
        self.records
            .iter()
            .filter(|((t, _), _)| t == tenant)
            .map(|((_, key), version)| (key.clone(), *version))
            .collect()
    }

    /// Every record the node holds, whichever tenant it belongs to.
    #[must_use]
    pub fn all_records(&self) -> &BTreeMap<(String, RecordKey), u64> {
        &self.records
    }

    /// Every store in apply order: `(tenant, key, version)`.
    #[must_use]
    pub fn history(&self) -> &[(String, RecordKey, u64)] {
        &self.history
    }

    /// How many bundles the sink was asked to apply.
    #[must_use]
    pub fn applies(&self) -> u64 {
        self.applies
    }

    /// Records skipped because that exact version was already stored.
    #[must_use]
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Records skipped because a higher version was already stored.
    #[must_use]
    pub fn older_ignored(&self) -> u64 {
        self.older_ignored
    }

    /// How many bundles this sink refused.
    #[must_use]
    pub fn refusals(&self) -> u64 {
        self.refusals
    }

    /// Refuse the next bundle that carries `key` once, then work normally: a transient sink
    /// failure, which the node reports as a refusal and retries later.
    pub fn refuse_next(&mut self, key: RecordKey) {
        self.refuse_next = Some(key);
    }

    /// Drop every record at a version above `position`: the node was restored from a backup taken
    /// at that position.
    pub fn truncate_after(&mut self, position: u64) {
        self.records.retain(|_, version| *version <= position);
        self.history.retain(|(_, _, version)| *version <= position);
    }

    /// Install one snapshot record by the same version rule as a bundle apply: a repeat or an
    /// older version changes nothing. Returns whether the record was stored.
    pub fn install(&mut self, tenant: &str, key: &RecordKey, version: u64) -> bool {
        let slot = (tenant.to_owned(), key.clone());
        match self.records.get(&slot).copied() {
            Some(held) if held >= version => false,
            _ => {
                self.records.insert(slot, version);
                self.history.push((tenant.to_owned(), key.clone(), version));
                true
            }
        }
    }

    /// Forget everything: a wiped or replaced node.
    pub fn wipe(&mut self) {
        self.records.clear();
        self.history.clear();
    }

    /// Every `(key, version)` the store holds more than once. Empty for a correct store.
    #[must_use]
    pub fn repeated(&self) -> Vec<(RecordKey, u64)> {
        let mut seen: BTreeMap<(RecordKey, u64), usize> = BTreeMap::new();
        for (_, key, version) in &self.history {
            *seen.entry((key.clone(), *version)).or_insert(0) += 1;
        }
        seen.into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|((key, version), _)| (key, version))
            .collect()
    }
}

impl BundleApply for MemStore {
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.records
            .get(&(tenant.to_owned(), key.clone()))
            .is_some_and(|version| *version >= min_version)
    }

    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal> {
        if let Some(poison) = &self.refuse_next
            && bundle.records.iter().any(|record| &record.key == poison)
        {
            self.refuse_next = None;
            self.refusals += 1;
            return Err(ApplyRefusal {
                reason: "injected sink refusal".to_owned(),
            });
        }
        self.applies += 1;
        let mut set = AppliedSet::default();
        for record in &bundle.records {
            let slot = (tenant.to_owned(), record.key.clone());
            match self.records.get(&slot).copied() {
                Some(held) if held == record.position => self.duplicates += 1,
                Some(held) if held > record.position => self.older_ignored += 1,
                _ => {
                    self.records.insert(slot, record.position);
                    self.history
                        .push((tenant.to_owned(), record.key.clone(), record.position));
                    set.records.push(AppliedRecord {
                        key: record.key.clone(),
                        version: record.position,
                    });
                }
            }
        }
        Ok(set)
    }
}

/// The hub's current image of every record, with the position of its latest event.
///
/// The relay reads this through [`ImageSource`] and puts the image in the bundle, exactly as the
/// real image source would (full-state events, ADR-061). It is a shared handle: the World keeps a
/// clone to write new versions into while the relay holds another to read them from, and a
/// bootstrap snapshot is built from it. Single-threaded by design (a deterministic simulation).
#[derive(Clone, Debug, Default)]
pub struct MemImages {
    images: Rc<RefCell<BTreeMap<RecordKey, SourceImage>>>,
}

impl MemImages {
    /// No images.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or replace) one record's image at `position`.
    pub fn set(&self, key: RecordKey, image: Value, position: u64) {
        self.images
            .borrow_mut()
            .insert(key.clone(), SourceImage::new(key, image, position));
    }

    /// Every record whose latest event is at or below `position`, by key.
    #[must_use]
    pub fn at_or_below(&self, position: u64) -> BTreeMap<RecordKey, SourceImage> {
        self.images
            .borrow()
            .iter()
            .filter(|(_, image)| image.position <= position)
            .map(|(key, image)| (key.clone(), image.clone()))
            .collect()
    }

    /// How many records the source holds an image for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.borrow().len()
    }

    /// Whether the source holds no image at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.borrow().is_empty()
    }
}

impl ImageSource for MemImages {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.images.borrow().get(key).cloned())
    }
}
