//! The edge side: install beside, hold, release, retire when unpinned, pick the start version.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, FOREIGN_RECORD, HASH_MISMATCH, Result};
use crate::record::{CompatClass, ProcessRecord};
use crate::store::{DefinitionStore, PinCounter};

/// What this node's engine can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// The engine version of this node.
    pub engine_version: u64,
    /// Whether new Cases may start on a `Breaking` version. Default false: a node starts new
    /// Cases only on versions that are not breaking.
    pub accepts_breaking: bool,
}

/// What receiving a record did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// Installed beside the others.
    Installed,
    /// The same version with the same hash was already installed.
    NoOp,
    /// The engine is too old: held until an upgrade, never installed and never dropped.
    Held,
}

/// One installed version, as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The process.
    pub process: String,
    /// The version.
    pub version: u64,
}

/// The edge intake.
pub struct ProcessIntake<S: DefinitionStore, P: PinCounter> {
    store: S,
    pins: P,
    tenant: String,
    capability: Capability,
    held: BTreeMap<(String, u64), ProcessRecord>,
    retired: BTreeSet<(String, u64)>,
    quarantined: u64,
}

impl<S: DefinitionStore, P: PinCounter> ProcessIntake<S, P> {
    /// An intake for `tenant`.
    pub fn new(store: S, pins: P, tenant: impl Into<String>, capability: Capability) -> Self {
        Self {
            store,
            pins,
            tenant: tenant.into(),
            capability,
            held: BTreeMap::new(),
            retired: BTreeSet::new(),
            quarantined: 0,
        }
    }

    /// The store (read access for the runtime and tests).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Records quarantined for a hash mismatch since start.
    pub fn quarantined(&self) -> u64 {
        self.quarantined
    }

    /// The versions waiting for an engine upgrade, as `(process, version)`: the hold list.
    pub fn held(&self) -> Vec<Installed> {
        self.held
            .keys()
            .map(|(process, version)| Installed {
                process: process.clone(),
                version: *version,
            })
            .collect()
    }

    /// Receives a hub record.
    ///
    /// # Errors
    ///
    /// `sync_process.foreign_record` (another tenant, or the payload names another process),
    /// `sync_process.hash_mismatch` (quarantined and counted: the record is not installed, and a
    /// different hash for an installed version is a defect), `sync_process.invalid_record`.
    pub fn receive(&mut self, record: &ProcessRecord) -> Result<Received> {
        record.check_shape()?;
        if record.tenant != self.tenant {
            return Err(Error::new(
                FOREIGN_RECORD,
                "the process belongs to another tenant",
            ));
        }
        if !record.hash_ok() {
            self.quarantined += 1;
            return Err(Error::new(
                HASH_MISMATCH,
                format!(
                    "the definition of {} does not match its hash; quarantined",
                    record.name()
                ),
            ));
        }
        if let Some(hash) = self.store.hash_of(&record.process, record.version) {
            if hash == record.definition_hash {
                return Ok(Received::NoOp);
            }
            self.quarantined += 1;
            return Err(Error::new(
                HASH_MISMATCH,
                format!(
                    "{} is installed with another definition; quarantined",
                    record.name()
                ),
            ));
        }
        // The engine decides: `min_engine_version` can only make this node hold.
        if record.min_engine_version > self.capability.engine_version {
            self.held
                .insert((record.process.clone(), record.version), record.clone());
            return Ok(Received::Held);
        }
        self.store.install(record)?;
        self.retired
            .remove(&(record.process.clone(), record.version));
        Ok(Received::Installed)
    }

    /// The engine was upgraded: installs every held record it can now run and returns them.
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    pub fn on_upgrade(&mut self, engine_version: u64) -> Result<Vec<Installed>> {
        self.capability.engine_version = self.capability.engine_version.max(engine_version);
        let ready: Vec<(String, u64)> = self
            .held
            .iter()
            .filter(|(_, r)| r.min_engine_version <= self.capability.engine_version)
            .map(|(k, _)| k.clone())
            .collect();
        let mut released = Vec::new();
        for key in ready {
            if let Some(record) = self.held.remove(&key) {
                self.store.install(&record)?;
                released.push(Installed {
                    process: key.0,
                    version: key.1,
                });
            }
        }
        Ok(released)
    }

    /// The hub retired a version: it is removed only once no Case is pinned to it. Returns whether
    /// it was removed now; otherwise it waits for [`sweep`](Self::sweep).
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    pub fn retire(&mut self, process: &str, version: u64) -> Result<bool> {
        self.held.remove(&(process.to_owned(), version));
        if self.store.hash_of(process, version).is_none() {
            return Ok(true);
        }
        if self.pins.pins(process, version) == 0 {
            self.store.remove(process, version)?;
            self.retired.remove(&(process.to_owned(), version));
            return Ok(true);
        }
        self.retired.insert((process.to_owned(), version));
        Ok(false)
    }

    /// Removes retired versions whose pin count has reached zero.
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    pub fn sweep(&mut self) -> Result<Vec<Installed>> {
        let due: Vec<(String, u64)> = self
            .retired
            .iter()
            .filter(|(p, v)| self.pins.pins(p, *v) == 0)
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for key in due {
            self.store.remove(&key.0, key.1)?;
            self.retired.remove(&key);
            removed.push(Installed {
                process: key.0,
                version: key.1,
            });
        }
        Ok(removed)
    }

    /// The version a **new** Case of `process` starts on: the highest installed version this
    /// engine can run that is not retired and, unless the node accepts breaking versions, not
    /// breaking. `None` when nothing qualifies.
    pub fn start_version(&self, process: &str) -> Option<u64> {
        self.store
            .versions(process)
            .into_iter()
            .rev()
            .filter(|v| !self.retired.contains(&(process.to_owned(), *v)))
            .find(|v| {
                self.store.get(process, *v).is_some_and(|r| {
                    r.min_engine_version <= self.capability.engine_version
                        && (self.capability.accepts_breaking
                            || r.compat_class != CompatClass::Breaking)
                })
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::record::CompatClass;
    use crate::store::{MemoryPins, MemoryStore};

    use super::*;

    fn rec(version: u64, min_engine: u64, compat: CompatClass) -> ProcessRecord {
        ProcessRecord::new(
            "acme",
            "order",
            version,
            min_engine,
            compat,
            r#"{"process":"order","v":1}"#,
        )
        .map(|mut r| {
            r.payload = format!(r#"{{"process":"order","v":{version}}}"#);
            r.definition_hash = crate::record::hash_definition(&r.payload);
            r
        })
        .unwrap()
    }

    fn intake(engine: u64) -> (ProcessIntake<MemoryStore, MemoryPins>, MemoryPins) {
        let pins = MemoryPins::new();
        (
            ProcessIntake::new(
                MemoryStore::new(),
                pins.clone(),
                "acme",
                Capability {
                    engine_version: engine,
                    accepts_breaking: false,
                },
            ),
            pins,
        )
    }

    #[test]
    fn install_beside_and_a_republish_is_a_no_op() {
        let (mut intake, _) = intake(5);
        assert_eq!(
            intake.receive(&rec(1, 1, CompatClass::Compatible)).unwrap(),
            Received::Installed
        );
        assert_eq!(
            intake.receive(&rec(2, 1, CompatClass::Compatible)).unwrap(),
            Received::Installed
        );
        assert_eq!(
            intake.receive(&rec(2, 1, CompatClass::Compatible)).unwrap(),
            Received::NoOp
        );
        assert_eq!(intake.store().versions("order"), vec![1, 2]);
    }

    #[test]
    fn a_bad_hash_is_quarantined_and_a_changed_installed_version_is_refused() {
        let (mut intake, _) = intake(5);
        let mut bad = rec(1, 1, CompatClass::Compatible);
        bad.definition_hash = "00".into();
        assert_eq!(intake.receive(&bad).unwrap_err().code, HASH_MISMATCH);
        assert_eq!(intake.quarantined(), 1);
        assert!(intake.store().versions("order").is_empty());
        intake.receive(&rec(1, 1, CompatClass::Compatible)).unwrap();
        let mut changed = rec(1, 1, CompatClass::Compatible);
        changed.payload = r#"{"process":"order","v":99}"#.into();
        changed.definition_hash = crate::record::hash_definition(&changed.payload);
        assert_eq!(intake.receive(&changed).unwrap_err().code, HASH_MISMATCH);
    }

    #[test]
    fn another_tenant_is_refused() {
        let (mut intake, _) = intake(5);
        let mut other = rec(1, 1, CompatClass::Compatible);
        other.tenant = "globex".into();
        assert_eq!(intake.receive(&other).unwrap_err().code, FOREIGN_RECORD);
    }

    #[test]
    fn a_definition_the_engine_cannot_run_is_held_and_released_on_upgrade() {
        let (mut intake, _) = intake(1);
        assert_eq!(
            intake.receive(&rec(3, 2, CompatClass::Compatible)).unwrap(),
            Received::Held
        );
        assert!(intake.store().versions("order").is_empty());
        assert_eq!(
            intake.held(),
            vec![Installed {
                process: "order".into(),
                version: 3
            }]
        );
        assert!(intake.on_upgrade(1).unwrap().is_empty());
        assert_eq!(intake.on_upgrade(2).unwrap().len(), 1);
        assert_eq!(intake.store().versions("order"), vec![3]);
        assert!(intake.held().is_empty());
    }

    #[test]
    fn a_pinned_version_survives_retirement_until_the_pin_count_is_zero() {
        let (mut intake, pins) = intake(5);
        intake.receive(&rec(1, 1, CompatClass::Compatible)).unwrap();
        intake.receive(&rec(2, 1, CompatClass::Compatible)).unwrap();
        pins.set("order", 1, 2);
        assert!(!intake.retire("order", 1).unwrap());
        assert_eq!(intake.store().versions("order"), vec![1, 2]);
        assert!(intake.sweep().unwrap().is_empty());
        // Retired: new Cases no longer start on it even while it stays for the pinned ones.
        assert_eq!(intake.start_version("order"), Some(2));
        pins.set("order", 1, 0);
        assert_eq!(intake.sweep().unwrap().len(), 1);
        assert_eq!(intake.store().versions("order"), vec![2]);
        // An unpinned version is removed at once.
        assert!(intake.retire("order", 2).unwrap());
        assert!(intake.store().versions("order").is_empty());
    }

    #[test]
    fn start_version_picks_the_newest_runnable_non_breaking_version() {
        let (mut intake, _) = intake(2);
        intake.receive(&rec(1, 1, CompatClass::Compatible)).unwrap();
        intake.receive(&rec(2, 2, CompatClass::Compatible)).unwrap();
        intake.receive(&rec(3, 2, CompatClass::Breaking)).unwrap();
        intake.receive(&rec(4, 3, CompatClass::Compatible)).unwrap(); // held: engine 2 < 3
        assert_eq!(intake.start_version("order"), Some(2));
        assert_eq!(intake.held().len(), 1);
        assert_eq!(intake.start_version("unknown"), None);
        let mut breaking_ok = ProcessIntake::new(
            MemoryStore::new(),
            MemoryPins::new(),
            "acme",
            Capability {
                engine_version: 2,
                accepts_breaking: true,
            },
        );
        breaking_ok
            .receive(&rec(3, 2, CompatClass::Breaking))
            .unwrap();
        assert_eq!(breaking_ok.start_version("order"), Some(3));
    }
}
