//! The registry of compiled rule sets, swapped by content hash.
//!
//! A [`Registry`] maps a name to an [`Arc<Compiled>`]. Readers take the whole map with one
//! wait-free load and keep the `Arc`s they took, so an evaluation that started on version N
//! finishes on N; [`Registry::swap`] publishes a new map atomically and new evaluations use it.
//! [`Registry::reload`] recompiles every set against a new effective metadata and swaps the ones
//! whose content changed — the hook the metadata live-reload signal (`spot-meta-live`) drives.
//!
//! Reload decides by **content**, never by the metadata version: two different metadata sets can
//! carry the same version (every fresh `spot_meta::Registry` reports 1), so a version-only check
//! would keep enforcing a rule set compiled against metadata that has since changed. The version
//! a [`Compiled`] set and a [`Reload`] carry are informational — for logs and diagnostics.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use spot_meta::EffectiveMeta;
use spot_meta_live::Snapshot;

use crate::compile::{Compiled, RuleSet};
use crate::error::RuleGraphError;

/// One rule set that failed to recompile during a [`Reload`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadFailure {
    /// The rule set's name.
    pub name: String,
    /// Why it did not compile against the new metadata.
    pub error: RuleGraphError,
}

/// What a [`Registry::reload`] did.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Reload {
    /// The metadata version the sets were recompiled against.
    ///
    /// Informational only: [`Reload::reloaded`], [`Reload::unchanged`] and
    /// [`Reload::failed`] say what happened, because the same version can
    /// accompany a recompile or none at all.
    pub version: u64,
    /// The names whose content changed and were swapped.
    pub reloaded: Vec<String>,
    /// The names whose content did not change.
    pub unchanged: Vec<String>,
    /// The names that failed to recompile; each keeps its previous compiled set.
    pub failed: Vec<ReloadFailure>,
}

/// A named collection of compiled rule sets.
#[derive(Debug)]
pub struct Registry {
    sets: ArcSwap<BTreeMap<String, Arc<Compiled>>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sets: ArcSwap::from_pointee(BTreeMap::new()),
        }
    }

    /// The compiled set named `name`, or `None`. Wait-free for the reader.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<Compiled>> {
        self.sets.load().get(name).cloned()
    }

    /// The names of every rule set, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.sets.load().keys().cloned().collect()
    }

    /// Inserts or replaces the set named `name`, returning the previous set.
    pub fn insert(
        &self,
        name: impl Into<String>,
        compiled: Arc<Compiled>,
    ) -> Option<Arc<Compiled>> {
        let name = name.into();
        let previous = self.sets.rcu(|current| {
            let mut next = (**current).clone();
            next.insert(name.clone(), compiled.clone());
            Arc::new(next)
        });
        previous.get(&name).cloned()
    }

    /// Replaces the set named `name` with `compiled`, returning the previous set. The alias of
    /// [`Registry::insert`] that names the hot-swap intent.
    pub fn swap(&self, name: impl Into<String>, compiled: Arc<Compiled>) -> Option<Arc<Compiled>> {
        self.insert(name, compiled)
    }

    /// Removes the set named `name`, returning it.
    pub fn remove(&self, name: &str) -> Option<Arc<Compiled>> {
        let previous = self.sets.rcu(|current| {
            let mut next = (**current).clone();
            next.remove(name);
            Arc::new(next)
        });
        previous.get(name).cloned()
    }

    /// Compiles `source` against `meta` and inserts it as `name`.
    ///
    /// # Errors
    ///
    /// Whatever [`RuleSet::compile`] returns.
    pub fn register(
        &self,
        name: impl Into<String>,
        source: &str,
        meta: &EffectiveMeta,
    ) -> Result<Arc<Compiled>, RuleGraphError> {
        let compiled = Arc::new(RuleSet::compile(source, meta)?);
        self.insert(name, compiled.clone());
        Ok(compiled)
    }

    /// Recompiles every registered set against `meta`, swapping the ones whose content changed.
    ///
    /// The comparison is the compiled content hash, not the metadata version: two same-version
    /// metadata sets still recompile, because a customisation can change what a rule reads under
    /// one version (`Reload::version` is informational).
    ///
    /// A set that fails to compile keeps its previous compiled form and is reported in
    /// [`Reload::failed`], so a bad rule file never takes the whole registry down.
    #[must_use]
    pub fn reload(&self, meta: &EffectiveMeta) -> Reload {
        let current = self.sets.load();
        let mut next: BTreeMap<String, Arc<Compiled>> = (**current).clone();
        let mut report = Reload {
            version: meta.version(),
            ..Reload::default()
        };
        let mut changes: Vec<(String, Arc<Compiled>)> = Vec::new();
        for (name, compiled) in current.iter() {
            match RuleSet::compile(compiled.source(), meta) {
                Ok(recompiled) => {
                    let recompiled = Arc::new(recompiled);
                    if recompiled.hash() == compiled.hash() {
                        report.unchanged.push(name.clone());
                    } else {
                        changes.push((name.clone(), recompiled));
                        report.reloaded.push(name.clone());
                    }
                }
                Err(error) => report.failed.push(ReloadFailure {
                    name: name.clone(),
                    error,
                }),
            }
        }
        if !changes.is_empty() {
            for (name, compiled) in changes {
                next.insert(name, compiled);
            }
            self.sets.store(Arc::new(next));
        }
        report.reloaded.sort();
        report.unchanged.sort();
        report
    }

    /// Recompiles every registered set against a live metadata [`Snapshot`], the hook the
    /// metadata live-reload signal drives.
    #[must_use]
    pub fn apply_snapshot(&self, snapshot: &Snapshot) -> Reload {
        self.reload(snapshot.meta())
    }
}
