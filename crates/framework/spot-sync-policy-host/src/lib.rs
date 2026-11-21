//! `spot-sync-policy-host`: what syncs, decided from the site's metadata (WP-RTB-02.T8, ADR-063).
//!
//! [`PolicyHost::load`] resolves a metadata generation's declared policies and runs every lint
//! (`spot_sync_policy::verify`) **at load**. A generation with any problem is **refused whole**:
//! the host keeps serving the previous generation and says why (`sync_policy: refused generation
//! 14 (sync_policy.closed_link on Invoice)`); nothing of a refused generation is applied.
//!
//! # Fail closed
//!
//! Until a generation has loaded, and for a DocType that has no policy, [`PolicyHost::may_leave`]
//! answers `false`: syncing is opt-in, the default class is `local`, and an unreadable policy
//! means nothing of that DocType leaves the node.
//!
//! # Direction and ownership
//!
//! A node pushes only what the policy sends **up** (`Up` or `Both`) and the hub sends only what it
//! sends **down**; a `master` record is hub-authored, so an edge-authored change to one is refused
//! at both ends with `sync_policy.master_edge_write` ([`PolicyHost::edge_may_write`]). Excluded
//! fields are stripped by `spot_sync_policy::outgoing_view`; the queue never holds an unredacted
//! image (the hub applies it before queueing).
//!
//! # Presets and the default
//!
//! The four presets of ADR-063 (`local`, `master`, `record`, `handover`) are read from the
//! library's `Class` as `local`, `master`, `transactional` and `shared` ([`preset_of`]); where ADR
//! and library differ (the library has no `handover` class, and `shared` also covers two-way
//! shared data) the library wins and the difference is reported. A DocType nobody declared a policy
//! for follows [`Defaults`] (`[sync] default_preset`): `local` unless the site chose otherwise, and
//! only for **runtime-created** DocTypes (those with no owning app). The default is declared into
//! the generation **before** the lints run, so a default that cannot hold (a link to a `local`
//! DocType, a missing owner scope) refuses the generation like any other policy problem.
//!
//! # Providers and stripping
//!
//! [`scope`] gives the hub relay its `ScopeKeysOf`, `DestinationScope` and redactor from the
//! current generation; [`strip`] removes excluded fields from an event payload on the way up
//! (the way down is the redactor); [`handover`] decides what an arriving handover record gets.
//!
//! A policy change that narrows scope takes effect for new entries at once and does not retract
//! what a node already holds (retraction is a separate, audited operation).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod handover;
pub mod scope;
pub mod strip;

use std::sync::Arc;

use spot_meta::EffectiveMeta;
use spot_sync_hub_adapters::PolicyBook;
use spot_sync_policy::{
    Class, Direction, PolicySet, PolicyTable, Preset, ScopeKeyType, SyncPolicy, Trigger, verify,
};

/// An edge tried to write a hub-authored (`master`) record.
pub const MASTER_EDGE_WRITE: &str = "sync_policy.master_edge_write";

/// The ADR-063 preset a library class reads as. `Shared` reads as `handover`: the library has no
/// separate class for it.
#[must_use]
pub fn preset_of(class: Class) -> Preset {
    match class {
        Class::Local => Preset::Local,
        Class::Master => Preset::Master,
        Class::Transactional => Preset::Record,
        Class::Shared => Preset::Handover,
    }
}

/// The class and direction a preset stands for.
#[must_use]
pub fn class_of(preset: Preset) -> (Class, Direction) {
    match preset {
        Preset::Local => (Class::Local, Direction::None),
        Preset::Master => (Class::Master, Direction::Down),
        Preset::Record => (Class::Transactional, Direction::Up),
        Preset::Handover => (Class::Shared, Direction::Both),
    }
}

/// What a DocType nobody declared a policy for follows (`[sync] default_preset`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// The preset. `local` (the default) declares nothing.
    pub preset: Preset,
    /// The scope key type a `record` or `handover` default is owned by. Without it such a default
    /// fails the owner-scope lint and the generation is refused (visible, never silent).
    pub owner_scope: Option<ScopeKeyType>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            preset: Preset::Local,
            owner_scope: None,
        }
    }
}

impl Defaults {
    /// The policy the default declares, or `None` for `local`.
    #[must_use]
    pub fn policy(&self) -> Option<SyncPolicy> {
        if self.preset == Preset::Local {
            return None;
        }
        let (class, direction) = class_of(self.preset);
        let owner = match class {
            Class::Master => None,
            _ => self.owner_scope.clone(),
        };
        Some(SyncPolicy::new(
            class,
            owner,
            direction,
            Trigger::OnSubmit,
            Default::default(),
        ))
    }
}

/// One verified generation: the table every decision reads, and the book the redactor reads.
#[derive(Debug)]
pub struct Snapshot {
    generation: u64,
    table: PolicyTable,
    book: PolicyBook,
}

impl Snapshot {
    fn new(generation: u64, table: PolicyTable) -> Self {
        let mut book = PolicyBook::new();
        for (doctype, policy) in table.iter() {
            book.insert(doctype, policy.clone());
        }
        Self {
            generation,
            table,
            book,
        }
    }

    /// The generation number.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The policy of `doctype`, or `None` for a DocType this generation does not hold.
    #[must_use]
    pub fn policy(&self, doctype: &str) -> Option<&SyncPolicy> {
        self.table.get(doctype)
    }

    /// The book the hub's redactor reads.
    #[must_use]
    pub fn book(&self) -> &PolicyBook {
        &self.book
    }

    /// Whether a record of `doctype` may leave this node (`up`) or the hub (`!up`). Fail closed:
    /// `false` for a DocType this generation does not hold and for `local` DocTypes.
    #[must_use]
    pub fn may_leave(&self, doctype: &str, up: bool) -> bool {
        match self.policy(doctype) {
            Some(policy) if policy.class.is_synced() => {
                if up {
                    policy.direction.has_up()
                } else {
                    policy.direction.has_down()
                }
            }
            _ => false,
        }
    }

    /// The preset `doctype` follows, `local` for one this generation does not hold.
    #[must_use]
    pub fn preset(&self, doctype: &str) -> Preset {
        self.policy(doctype)
            .map_or(Preset::Local, |policy| preset_of(policy.class))
    }

    /// Every DocType that syncs, with its preset and direction, for `spot sync policy show`.
    pub fn synced(&self) -> impl Iterator<Item = (&str, Preset, Direction)> {
        self.table
            .iter()
            .filter(|(_, policy)| policy.class.is_synced())
            .map(|(doctype, policy)| (doctype, preset_of(policy.class), policy.direction))
    }
}

/// Why a generation was refused, for the readiness line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The refused generation number.
    pub generation: u64,
    /// The first problem's code.
    pub code: &'static str,
    /// The first problem's DocType.
    pub doctype: String,
    /// How many problems there were.
    pub problems: usize,
}

/// The hosted policy view.
#[derive(Debug, Default)]
pub struct PolicyHost {
    current: Option<Arc<Snapshot>>,
    refused: Option<Refused>,
    defaults: Defaults,
}

impl PolicyHost {
    /// A host with no generation loaded: nothing leaves the node.
    pub fn new() -> Self {
        Self::default()
    }

    /// A host whose undeclared runtime-created DocTypes follow `defaults`.
    pub fn with_defaults(defaults: Defaults) -> Self {
        Self {
            defaults,
            ..Self::default()
        }
    }

    /// The defaults in force.
    pub fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    /// Loads generation `generation`. The default policy is declared for every runtime-created
    /// DocType nobody declared one for, then every lint runs. On any problem the generation is
    /// refused and the previous one keeps serving.
    ///
    /// # Errors
    ///
    /// The [`Refused`] record (also kept for [`readiness`](Self::readiness)).
    pub fn load(
        &mut self,
        generation: u64,
        meta: &EffectiveMeta,
        declared: &PolicySet,
    ) -> Result<(), Refused> {
        let mut declared = declared.clone();
        if let Some(default) = self.defaults.policy() {
            for definition in meta.definitions() {
                // A runtime-created DocType has no owning app; an installed app's own DocTypes
                // sync only when their metadata says so.
                if definition.app.is_none()
                    && !definition.is_child
                    && declared.get(&definition.name).is_none()
                {
                    declared.declare(&definition.name, default.clone());
                }
            }
        }
        match verify(meta, &declared) {
            Ok(table) => {
                self.current = Some(Arc::new(Snapshot::new(generation, table)));
                self.refused = None;
                Ok(())
            }
            Err(problems) => {
                let first = problems.first();
                let refused = Refused {
                    generation,
                    code: first.map_or("sync_policy.unknown", |p| p.code()),
                    doctype: first.map(|p| p.doctype().to_owned()).unwrap_or_default(),
                    problems: problems.len(),
                };
                self.refused = Some(refused.clone());
                Err(refused)
            }
        }
    }

    /// The generation currently serving.
    pub fn generation(&self) -> Option<u64> {
        self.current.as_ref().map(|snapshot| snapshot.generation())
    }

    /// The serving generation, for the providers (cheap to clone, immutable).
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.current.clone()
    }

    /// The last refusal, until a generation loads.
    pub fn refused(&self) -> Option<&Refused> {
        self.refused.as_ref()
    }

    /// Whether a record of `doctype` may leave **this node** (`up = true`: pushed to the hub) or
    /// the hub (`up = false`: sent down). Fail closed: false with no generation loaded and for
    /// `local` DocTypes.
    pub fn may_leave(&self, doctype: &str, up: bool) -> bool {
        self.current
            .as_ref()
            .is_some_and(|snapshot| snapshot.may_leave(doctype, up))
    }

    /// Whether an edge may author a change to `doctype`. A `master` record is hub-authored.
    ///
    /// # Errors
    ///
    /// [`MASTER_EDGE_WRITE`].
    pub fn edge_may_write(&self, doctype: &str) -> Result<(), &'static str> {
        match self
            .current
            .as_ref()
            .and_then(|snapshot| snapshot.policy(doctype))
        {
            Some(policy) if policy.class == Class::Master => Err(MASTER_EDGE_WRITE),
            _ => Ok(()),
        }
    }

    /// The readiness line: `sync_policy: generation 14` or
    /// `sync_policy: refused generation 15 (sync_policy.closed_link on Invoice), serving 14`.
    pub fn readiness(&self) -> String {
        match (&self.refused, self.generation()) {
            (Some(r), Some(g)) => format!(
                "sync_policy: refused generation {} ({} on {}), serving {g}",
                r.generation, r.code, r.doctype
            ),
            (Some(r), None) => format!(
                "sync_policy: refused generation {} ({} on {}), nothing syncs",
                r.generation, r.code, r.doctype
            ),
            (None, Some(g)) => format!("sync_policy: generation {g}"),
            (None, None) => "sync_policy: no generation loaded, nothing syncs".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn nothing_leaves_before_a_generation_loads_and_the_line_says_so() {
        let host = PolicyHost::new();
        assert!(!host.may_leave("Invoice", true));
        assert!(!host.may_leave("Invoice", false));
        assert!(host.edge_may_write("Invoice").is_ok());
        assert_eq!(
            host.readiness(),
            "sync_policy: no generation loaded, nothing syncs"
        );
        assert_eq!(host.generation(), None);
    }

    #[test]
    fn presets_and_classes_map_both_ways() {
        for preset in Preset::ALL {
            assert_eq!(preset_of(class_of(preset).0), preset);
        }
        assert_eq!(class_of(Preset::Local).1, Direction::None);
        assert!(Defaults::default().policy().is_none());
    }
}
