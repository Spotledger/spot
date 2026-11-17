//! The seams the edge runtime fills, with in-memory implementations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::record::ProcessRecord;

/// Where installed definitions live.
pub trait DefinitionStore {
    /// The stored hash of `(process, version)`, if installed.
    fn hash_of(&self, process: &str, version: u64) -> Option<String>;
    /// The installed record.
    fn get(&self, process: &str, version: u64) -> Option<ProcessRecord>;
    /// Installs a record beside the others.
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    fn install(&mut self, record: &ProcessRecord) -> Result<()>;
    /// Removes a version.
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    fn remove(&mut self, process: &str, version: u64) -> Result<()>;
    /// Installed versions of a process, ascending.
    fn versions(&self, process: &str) -> Vec<u64>;
}

/// How many running Cases are pinned to a version (the BPM runtime implements it).
pub trait PinCounter {
    /// The number of Cases pinned to `(process, version)`.
    fn pins(&self, process: &str, version: u64) -> u64;
}

/// An in-memory store.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    records: BTreeMap<(String, u64), ProcessRecord>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl DefinitionStore for MemoryStore {
    fn hash_of(&self, process: &str, version: u64) -> Option<String> {
        self.records
            .get(&(process.to_owned(), version))
            .map(|r| r.definition_hash.clone())
    }

    fn get(&self, process: &str, version: u64) -> Option<ProcessRecord> {
        self.records.get(&(process.to_owned(), version)).cloned()
    }

    fn install(&mut self, record: &ProcessRecord) -> Result<()> {
        self.records
            .insert((record.process.clone(), record.version), record.clone());
        Ok(())
    }

    fn remove(&mut self, process: &str, version: u64) -> Result<()> {
        self.records.remove(&(process.to_owned(), version));
        Ok(())
    }

    fn versions(&self, process: &str) -> Vec<u64> {
        self.records
            .keys()
            .filter(|(p, _)| p == process)
            .map(|(_, v)| *v)
            .collect()
    }
}

/// An in-memory pin counter a test can move.
#[derive(Clone, Debug, Default)]
pub struct MemoryPins(Arc<Mutex<BTreeMap<(String, u64), u64>>>);

impl MemoryPins {
    /// No pins.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the pin count of a version.
    pub fn set(&self, process: &str, version: u64, count: u64) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((process.to_owned(), version), count);
    }
}

impl PinCounter for MemoryPins {
    fn pins(&self, process: &str, version: u64) -> u64 {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(process.to_owned(), version))
            .copied()
            .unwrap_or(0)
    }
}

/// Helper for tests and callers: the set of installed `(process, version)` pairs.
#[allow(dead_code)]
pub fn installed_set(store: &dyn DefinitionStore, processes: &[&str]) -> BTreeSet<(String, u64)> {
    processes
        .iter()
        .flat_map(|p| {
            store
                .versions(p)
                .into_iter()
                .map(move |v| ((*p).to_owned(), v))
        })
        .collect()
}
