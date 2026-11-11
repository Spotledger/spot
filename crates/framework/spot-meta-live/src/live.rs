//! [`LiveMeta`]: the snapshot holder the engine reads through, and the changes that move it.
//!
//! A change is expressed as a [`MetaChange`] (a custom field, a property override, new
//! definitions). [`LiveMeta::apply`] builds the whole new snapshot — metadata, plans, hash —
//! **before** swapping the pointer, so a failed build leaves the previous snapshot serving and a
//! reader never sees a half-built set.
//!
//! # Two numberings, one version
//!
//! A snapshot has the site's [`Version`] and the effective metadata's own
//! `meta().version()`, which `spot-meta` derives as `customisations.revision() + 1`. They are
//! kept **equal** for every snapshot at version 1 or above, because the metadata version is what
//! the HTTP layer serves as the `ETag` of `GET /api/v1/meta/{doctype}` and what caches key on:
//!
//! - [`Snapshot::build`] is the single place every path funnels through (`apply`, `adopt` and
//!   `spot-customise`'s `assemble`). It stamps the customisations' revision with `version - 1`
//!   (`Customisations::stamp_revision`), so `meta().version() == version` whatever revision the
//!   caller's customisations carried, and a new DocType, an adopted snapshot or a roll-back is
//!   reflected too;
//! - a local [`apply`](LiveMeta::apply) mints `current + 1`;
//! - the one exception is the starting snapshot [`Version::ZERO`]: `spot-meta` cannot build
//!   metadata older than version 1, so it reports 1. The first local change therefore mints
//!   version 2 (not 1), so the metadata version still rises on the first change; versions only
//!   have to rise, not be dense. A follower adopting version 1 reports 1, as the starting
//!   snapshot did, which is why a node whose first change was announced as version 2 and a
//!   node that adopts it agree. A site that starts from storage (a node booted with
//!   [`LiveMeta::at_version`] at 1 or more, or one that adopted) never sees this.

use std::sync::{Arc, Mutex, MutexGuard};

use arc_swap::ArcSwap;
use spot_core::SiteId;
use spot_meta::{BuildOptions, Customisations, FieldSpec, Property};

use crate::error::{LiveError, LiveResult, divergence};
use crate::snapshot::{Definitions, MetaHash, Snapshot, Version, without_engine_fields};

/// One change to a site's metadata.
///
/// `#[non_exhaustive]`: new change kinds may be added. Each maps to exactly what `spot-meta`'s
/// `Registry` mutators do, without this crate owning the registry.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum MetaChange {
    /// Add a custom field to a DocType.
    AddCustomField {
        /// The DocType to add it to.
        doctype: String,
        /// The field; its name must start with `custom_`. Boxed because a `FieldSpec` is large
        /// and every other variant is small.
        spec: Box<FieldSpec>,
        /// Place the field right after this one; at the end when `None`.
        insert_after: Option<String>,
    },
    /// Set a property of a field (`field: Some`) or of the DocType (`field: None`).
    SetProperty {
        /// The DocType the property belongs to.
        doctype: String,
        /// The field, or `None` for a DocType property.
        field: Option<String>,
        /// The property and its new value.
        property: Property,
    },
    /// Remove a custom field (and the overrides that name it).
    RemoveCustomField {
        /// The DocType the field is on.
        doctype: String,
        /// The custom field name.
        name: String,
    },
    /// Replace the site's definitions (a standard update). Customisations stay and apply on top.
    SetDefinitions {
        /// The new definitions.
        definitions: Definitions,
    },
}

/// What [`LiveMeta::adopt`] did with an offered snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Adopt {
    /// The offered version is older than the current one; it was ignored (never go backwards).
    Stale,
    /// The offered version equals the current one with the same content; nothing changed.
    Unchanged,
    /// A newer version was built, validated and swapped in.
    Applied(Version),
}

/// The site's live metadata: one immutable [`Snapshot`] behind a lock-free pointer.
///
/// Readers call [`current`](LiveMeta::current) and keep the `Arc`; a request that started on
/// version N finishes on N. Writers call [`apply`](LiveMeta::apply) (local change) or
/// [`adopt`](LiveMeta::adopt) (a snapshot built elsewhere); both are serialised against each
/// other, and both build before they swap.
pub struct LiveMeta {
    site: SiteId,
    options: BuildOptions,
    state: Mutex<MetaState>,
    current: ArcSwap<Snapshot>,
}

/// The inputs a build needs, kept so every change rebuilds from the same base.
#[derive(Debug)]
struct MetaState {
    definitions: Definitions,
    customisations: Customisations,
}

impl LiveMeta {
    /// A site at [`Version::ZERO`] holding `definitions` with no customisations.
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`] when the definitions do not build.
    pub fn new(site: SiteId, definitions: Definitions) -> LiveResult<Self> {
        Self::at_version(site, definitions, Version::ZERO)
    }

    /// As [`LiveMeta::new`] but already at `version` (a node that booted from storage).
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`] when the definitions do not build.
    pub fn at_version(
        site: SiteId,
        definitions: Definitions,
        version: Version,
    ) -> LiveResult<Self> {
        Self::with(site, definitions, BuildOptions::new(), version)
    }

    /// The general constructor: options (known seams, derivations) are kept for every later
    /// build, so a runtime change is checked against the same known seams.
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`] when the definitions do not build.
    pub fn with(
        site: SiteId,
        definitions: Definitions,
        options: BuildOptions,
        version: Version,
    ) -> LiveResult<Self> {
        let customisations = at_revision(version);
        let snapshot = Snapshot::build(definitions.clone(), &customisations, &options, version)?;
        Ok(Self {
            site,
            options,
            state: Mutex::new(MetaState {
                definitions,
                customisations,
            }),
            current: ArcSwap::from(Arc::new(snapshot)),
        })
    }

    /// The site these snapshots belong to.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The current snapshot, lock-free. The caller keeps the `Arc`: it always sees one
    /// consistent snapshot, whatever changes happen meanwhile.
    #[must_use]
    pub fn current(&self) -> Arc<Snapshot> {
        self.current.load_full()
    }

    /// Applies `change`: builds and validates the whole next snapshot, then swaps it in.
    ///
    /// On success the previous snapshot is replaced atomically and the new version returned.
    /// On failure **nothing changes**: the old snapshot keeps serving and every error is
    /// returned at once ([`LiveError::Build`]).
    ///
    /// # Errors
    ///
    /// [`LiveError::Build`], [`LiveError::UnknownCustomField`], [`LiveError::Encode`] or
    /// [`LiveError::Exhausted`].
    pub fn apply(&self, change: MetaChange) -> LiveResult<Version> {
        self.apply_inner(change).map(|snapshot| snapshot.version())
    }

    /// As [`apply`](LiveMeta::apply), returning the exact snapshot swapped in — so a publisher
    /// announces the `(version, hash)` it actually built, not a later one a racing writer made.
    pub(crate) fn apply_inner(&self, change: MetaChange) -> LiveResult<Arc<Snapshot>> {
        let mut state = self.lock();
        let next = self
            .current
            .load_full()
            .version()
            .checked_next()
            .ok_or(LiveError::Exhausted)?;
        let mut definitions = state.definitions.clone();
        let mut customisations = state.customisations.clone();
        apply_change(&mut definitions, &mut customisations, change)?;
        // `Snapshot::build` stamps the metadata with the minted version, so the two always
        // agree. The starting snapshot `Version::ZERO` reports metadata version 1, so the first
        // local change mints 2 (not 1): the served number must rise on the first change too.
        let version = next.max(Version::new(2));
        let snapshot = Arc::new(Snapshot::build(
            definitions.clone(),
            &customisations,
            &self.options,
            version,
        )?);
        state.definitions = definitions;
        state.customisations = customisations;
        state
            .customisations
            .stamp_revision(version.get().saturating_sub(1));
        self.current.store(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// Adopts a snapshot built on another node (or loaded from storage) at `version` with
    /// content `hash`.
    ///
    /// Never goes backwards: a version below the current one is [`Adopt::Stale`]. The same
    /// version with the same hash is [`Adopt::Unchanged`]; the same version with a **different**
    /// hash is [`LiveError::Divergence`], and so is a newer version whose `definitions` do not
    /// hash to `hash` — the receiving node refuses to guess and keeps its own snapshot.
    ///
    /// # Errors
    ///
    /// [`LiveError::Divergence`], or [`LiveError::Build`] when the offered definitions do not
    /// build.
    pub fn adopt(
        &self,
        version: Version,
        hash: &MetaHash,
        definitions: Definitions,
    ) -> LiveResult<Adopt> {
        let mut state = self.lock();
        let current = self.current.load_full();
        if version < current.version() {
            return Ok(Adopt::Stale);
        }
        if version == current.version() {
            if hash == current.hash() {
                return Ok(Adopt::Unchanged);
            }
            return Err(divergence(&self.site, version, current.hash(), hash));
        }
        // The offered definitions are effective: a posted DocType already carries its engine
        // fields. Drop them before rebuilding — the registry adds exactly one copy back.
        let definitions = without_engine_fields(definitions);
        // Built at the offered version, so `meta().version() == version` (see the module docs).
        let customisations = at_revision(version);
        let snapshot = Arc::new(Snapshot::build(
            definitions.clone(),
            &customisations,
            &self.options,
            version,
        )?);
        if snapshot.hash() != hash {
            return Err(divergence(&self.site, version, snapshot.hash(), hash));
        }
        state.definitions = definitions;
        state.customisations = customisations;
        self.current.store(snapshot);
        Ok(Adopt::Applied(version))
    }

    fn lock(&self) -> MutexGuard<'_, MetaState> {
        // The guarded data is the build inputs; a panic while building cannot leave them
        // inconsistent (they are replaced only after a successful build).
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for LiveMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveMeta")
            .field("site", &self.site)
            .field("version", &self.current.load().version())
            .finish_non_exhaustive()
    }
}

/// No customisations, at the revision that gives the metadata exactly `version`
/// (`version = revision + 1`). [`Version::ZERO`] gets revision 0, so its metadata reports 1.
fn at_revision(version: Version) -> Customisations {
    Customisations::with_revision(version.get().saturating_sub(1))
}

/// Applies one change to the build inputs, exactly as `spot-meta`'s `Registry` mutators do.
fn apply_change(
    definitions: &mut Definitions,
    customisations: &mut Customisations,
    change: MetaChange,
) -> LiveResult<()> {
    match change {
        MetaChange::AddCustomField {
            doctype,
            spec,
            insert_after,
        } => customisations.add_field(&doctype, *spec, insert_after.as_deref()),
        MetaChange::SetProperty {
            doctype,
            field,
            property,
        } => customisations.set(&doctype, field.as_deref(), property),
        MetaChange::RemoveCustomField { doctype, name } => {
            if !customisations.remove_field(&doctype, &name) {
                return Err(LiveError::UnknownCustomField {
                    doctype,
                    field: name,
                });
            }
        }
        MetaChange::SetDefinitions { definitions: new } => {
            *definitions = new;
            customisations.touch();
        }
    }
    Ok(())
}
