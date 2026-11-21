//! The environment store: sets kept by name with optimistic versions, the environment side of
//! the control store.
//!
//! The tenant control store ([`spot_tenancy::ControlStore`]) owns tenants; this trait owns the
//! *sets* that link them. It mirrors the control store's contract: names are unique, a replace
//! carries the version it read, and a store failure is never a silent default.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::{EnvironmentError, Result, codes};
use crate::model::EnvironmentSet;

/// One stored set and the version a replace must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSet {
    /// The set.
    pub set: EnvironmentSet,
    /// The optimistic version; `create` stores version 1 and every replace adds one.
    pub version: u64,
}

/// The durable environment sets. Implementations must make `create` and `replace` atomic.
pub trait EnvironmentStore {
    /// Stores a new set under its name.
    ///
    /// # Errors
    /// [`codes::STORE_NAME_TAKEN`] when the name is in use, or a store failure.
    fn create(&self, set: EnvironmentSet) -> Result<StoredSet>;

    /// Reads one set by name.
    ///
    /// # Errors
    /// [`codes::STORE_NOT_FOUND`] or a store failure.
    fn get(&self, name: &str) -> Result<StoredSet>;

    /// Replaces a set whose stored version equals `expected_version`; the new version is
    /// `expected_version + 1`.
    ///
    /// # Errors
    /// [`codes::STORE_NOT_FOUND`], [`codes::STORE_VERSION_CONFLICT`] or a store failure.
    fn replace(&self, set: EnvironmentSet, expected_version: u64) -> Result<StoredSet>;

    /// Every set, ordered by name.
    ///
    /// # Errors
    /// A store failure.
    fn list(&self) -> Result<Vec<StoredSet>>;
}

/// The in-memory [`EnvironmentStore`], for tests and single-process use.
#[derive(Default)]
pub struct MemoryEnvironmentStore {
    sets: Mutex<BTreeMap<String, StoredSet>>,
}

impl MemoryEnvironmentStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, StoredSet>>> {
        self.sets.lock().map_err(|_| {
            EnvironmentError::new(codes::STORE_FAILED, "the environment store is unavailable")
        })
    }
}

impl EnvironmentStore for MemoryEnvironmentStore {
    fn create(&self, set: EnvironmentSet) -> Result<StoredSet> {
        set.validate()?;
        let mut sets = self.lock()?;
        if sets.contains_key(set.name()) {
            return Err(EnvironmentError::about(
                codes::STORE_NAME_TAKEN,
                set.name(),
                "an environment set with that name already exists",
            ));
        }
        let stored = StoredSet { set, version: 1 };
        sets.insert(stored.set.name().to_owned(), stored.clone());
        Ok(stored)
    }

    fn get(&self, name: &str) -> Result<StoredSet> {
        self.lock()?.get(name).cloned().ok_or_else(|| {
            EnvironmentError::about(
                codes::STORE_NOT_FOUND,
                name,
                "no environment set with that name exists",
            )
        })
    }

    fn replace(&self, set: EnvironmentSet, expected_version: u64) -> Result<StoredSet> {
        set.validate()?;
        let mut sets = self.lock()?;
        let name = set.name().to_owned();
        let Some(current) = sets.get(&name) else {
            return Err(EnvironmentError::about(
                codes::STORE_NOT_FOUND,
                name,
                "no environment set with that name exists",
            ));
        };
        if current.version != expected_version {
            return Err(EnvironmentError::about(
                codes::STORE_VERSION_CONFLICT,
                name,
                "the set changed since it was read",
            ));
        }
        let stored = StoredSet {
            set,
            version: expected_version + 1,
        };
        sets.insert(name, stored.clone());
        Ok(stored)
    }

    fn list(&self) -> Result<Vec<StoredSet>> {
        Ok(self.lock()?.values().cloned().collect())
    }
}
