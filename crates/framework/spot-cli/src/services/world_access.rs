//! The world-access composer (WP-AI-07.T5; blueprint WS13, ADR-26): the one provider of
//! `dyn WorldAccess`, assembled part by part from the subsystems a remedy changes. Operator
//! guide: `docs/running-spot/world-access.md`.
//!
//! # What it does
//!
//! The remedy host (`services/remedies`) and the fix pipeline (`services/fix_pipeline`) run a
//! remedy on a [`WorldAccess`]: a context over the typed ports of `spot-remedies` (jobs, cache,
//! retrieval index, edge sync, quarantine, BPM cases, documents). Nothing implemented that trait,
//! so on a real site a dry-run, apply or undo answered `remedies_host.no_world` and the pipeline
//! stayed degraded. This module is that implementation, and nothing else: it **composes**.
//!
//! - **One part per port** ([`parts`]): `JobsPart`, `CachePart`, `IndexPart`, `EdgePart`,
//!   `QuarantinePart`, `CasesPart`, `DocumentsPart`. Each owns two faces: `live()` (the real
//!   subsystem) and `rehearsal()` (a snapshot read into the in-memory fixture ops of
//!   `spot_remedies::fixture`, so a rehearsal can be changed and thrown away and never writes to
//!   the subsystem). Each part lives in its own file of `world_access/` and exports
//!   `from_site(&SiteCtx) -> Option<Arc<dyn XPart>>`; WP-AI-07.T5 declares them as stubs that
//!   return `None`, WP-AI-07.T6 and T7 write the real ones.
//! - **Live** ([`WorldKind::Live`]): `RemedyCtx::new` over the seven `live_as(actor)` ops (the remedy's
//!   own `agent:<id>`; a part that records or checks who acts uses it, the default is `live()`).
//! - **Rehearsal** ([`WorldKind::Rehearsal`]): the same over the seven `rehearsal()` snapshots,
//!   taken together under one lock so the snapshot is consistent (no remedy runs on the live world
//!   while it is taken, and the other way round). Two rehearsals are independent of each other and
//!   of the live site. `[services.world_access] rehearsal = false` refuses them
//!   (`world_access.rehearsal_off`), which also keeps the fix pipeline from rehearsing, so a T2
//!   fix is not applied.
//! - **A missing part is a degraded part, not a missing world.** For a part whose `from_site`
//!   returned `None` the composer substitutes the `Unavailable*` ops ([`unavailable`]): reads
//!   answer empty, every change answers `OpError` "not available on this site". Remedies over the
//!   other subsystems still run; a remedy whose precondition reads the missing part is refused
//!   cleanly by the registry (the thing it asks for "does not exist"), never `no_world`. The
//!   module's readiness text names the parts that are missing: `degraded: world_access (no jobs,
//!   cache)`.
//! - **The composer adds no audit of its own**: the remedy host already records the intent and
//!   the events of every call, and the world never sees a payload or a cell value.
//!
//! # Start order
//!
//! `ORDER` is 1265: below the remedy host (1270), which reads `ctx.port::<dyn WorldAccess>()` when
//! it registers, so the port must be provided before it. The brief asks for an `ORDER` above every
//! module whose port a part reads, but those modules start later than the host (the quarantine
//! store at 1400, the sync hub at 1300, the retrieval service at 1270 itself) and ports flow
//! forward only. So the composer **provides first and resolves last**: it registers the (still
//! empty) composer at 1265 and queues a finaliser (`SiteCtx::finally`) that runs after every
//! module has registered; the finaliser reads each part's `from_site` with every port in place and
//! fills the composer once. Nothing calls `with_ctx` before the site has finished assembling.
//!
//! # Threading
//!
//! The host calls `with_ctx` on a blocking thread (`spawn_blocking`), and a part's ops are
//! synchronous. A part over asynchronous storage blocks on a `tokio::runtime::Handle` it captured
//! when it was built ([`parts::capture_runtime`], read inside the runtime): `Handle::block_on`,
//! never `block_in_place`, which is invalid on a `spawn_blocking` thread.
//!
//! # Level
//!
//! **Optional.** The service's own check always passes; a site without some parts is `degraded`
//! (never failed) and says which. With `enabled = false` the module provides nothing (logged once
//! at start) and the remedy host answers `remedies_host.no_world` again.
//!
//! # Ports
//!
//! Provides `dyn WorldAccess` ([`WorldComposer`]); takes whatever each part's `from_site` reads.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::Deserialize;
use spot_remedies::{
    CacheOps, CaseOps, DocumentOps, EdgeOps, IndexOps, JobOps, QuarantineOps, RemedyCtx,
};
use tokio::sync::watch;

use crate::services::remedies::world::{CtxRequest, WorldAccess, WorldError, WorldKind};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "world_access/cache.rs"]
pub mod cache;
#[path = "world_access/cases.rs"]
pub mod cases;
#[path = "world_access/documents.rs"]
pub mod documents;
#[path = "world_access/edge.rs"]
pub mod edge;
#[path = "world_access/fixture.rs"]
pub mod fixture;
#[path = "world_access/index.rs"]
pub mod index;
#[path = "world_access/jobs.rs"]
pub mod jobs;
#[path = "world_access/parts.rs"]
pub mod parts;
#[path = "world_access/quarantine.rs"]
pub mod quarantine;
#[path = "world_access/unavailable.rs"]
pub mod unavailable;

use parts::{
    CachePart, CasesPart, DocumentsPart, EdgePart, IndexPart, JobsPart, PartName, QuarantinePart,
};
use unavailable::{
    UnavailableCache, UnavailableCases, UnavailableDocuments, UnavailableEdge, UnavailableIndex,
    UnavailableJobs, UnavailableQuarantine,
};

/// Start order: below the remedy host (1270), which reads the port when it registers; the parts
/// are resolved in a finaliser (see the module doc).
pub const ORDER: u32 = 1265;

/// The catalogue code that heads a settings refusal.
const SETTINGS_INVALID: &str = "world_access.settings_invalid";

/// Why a rehearsal is refused when the settings switch them off.
const REHEARSAL_OFF: &str = concat!(
    "world_access.rehearsal_off",
    ": rehearsal is switched off in the site settings"
);

/// Why a world is refused before the site has finished assembling.
const NOT_ASSEMBLED: &str = concat!(
    "world_access.not_assembled",
    ": the world is not assembled yet"
);

fn default_true() -> bool {
    true
}

/// `[services.world_access]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Provide the world. Default true. Off: the module provides nothing.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Offer rehearsal snapshots. Default true. Off: a rehearsal is refused, which also stops the
    /// fix pipeline from rehearsing.
    #[serde(default = "default_true")]
    pub rehearsal: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            rehearsal: true,
        }
    }
}

/// The seven parts a site has. A part the site lacks is `None` and is replaced by the
/// unavailable ops when a world is opened.
#[derive(Clone, Default)]
pub struct Parts {
    /// The jobs part.
    pub jobs: Option<Arc<dyn JobsPart>>,
    /// The cache part.
    pub cache: Option<Arc<dyn CachePart>>,
    /// The retrieval-index part.
    pub index: Option<Arc<dyn IndexPart>>,
    /// The edge-sync part.
    pub edge: Option<Arc<dyn EdgePart>>,
    /// The quarantine part.
    pub quarantine: Option<Arc<dyn QuarantinePart>>,
    /// The cases part.
    pub cases: Option<Arc<dyn CasesPart>>,
    /// The documents part.
    pub documents: Option<Arc<dyn DocumentsPart>>,
}

impl Parts {
    /// No part at all (what the stubs give: every remedy refuses cleanly, none answers `no_world`).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads every part from the site: each part's own `from_site`, with every port in place.
    #[must_use]
    pub fn from_site(ctx: &SiteCtx<'_>) -> Self {
        Self {
            jobs: jobs::from_site(ctx),
            cache: cache::from_site(ctx),
            index: index::from_site(ctx),
            edge: edge::from_site(ctx),
            quarantine: quarantine::from_site(ctx),
            cases: cases::from_site(ctx),
            documents: documents::from_site(ctx),
        }
    }

    /// The parts this site does not have, in the readiness order.
    #[must_use]
    pub fn missing(&self) -> Vec<PartName> {
        let present = [
            self.jobs.is_some(),
            self.cache.is_some(),
            self.index.is_some(),
            self.edge.is_some(),
            self.quarantine.is_some(),
            self.cases.is_some(),
            self.documents.is_some(),
        ];
        PartName::ALL
            .iter()
            .zip(present)
            .filter(|(_, here)| !here)
            .map(|(name, _)| *name)
            .collect()
    }

    fn live(&self, actor: &str) -> Ports<'_> {
        Ports {
            jobs: match &self.jobs {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableJobs),
            },
            cache: match &self.cache {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableCache),
            },
            index: match &self.index {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableIndex),
            },
            edge: match &self.edge {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableEdge),
            },
            quarantine: match &self.quarantine {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableQuarantine),
            },
            cases: match &self.cases {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableCases),
            },
            documents: match &self.documents {
                Some(part) => part.live_as(actor),
                None => Box::new(UnavailableDocuments),
            },
        }
    }

    fn rehearsal(&self) -> std::result::Result<Ports<'static>, WorldError> {
        Ok(Ports {
            jobs: match &self.jobs {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableJobs),
            },
            cache: match &self.cache {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableCache),
            },
            index: match &self.index {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableIndex),
            },
            edge: match &self.edge {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableEdge),
            },
            quarantine: match &self.quarantine {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableQuarantine),
            },
            cases: match &self.cases {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableCases),
            },
            documents: match &self.documents {
                Some(part) => part.rehearsal()?,
                None => Box::new(UnavailableDocuments),
            },
        })
    }
}

/// The seven ports of one opened world.
struct Ports<'p> {
    jobs: Box<dyn JobOps + 'p>,
    cache: Box<dyn CacheOps + 'p>,
    index: Box<dyn IndexOps + 'p>,
    edge: Box<dyn EdgeOps + 'p>,
    quarantine: Box<dyn QuarantineOps + 'p>,
    cases: Box<dyn CaseOps + 'p>,
    documents: Box<dyn DocumentOps + 'p>,
}

impl Ports<'_> {
    /// Builds the context over these ports and the request, and runs `work` once.
    fn run(&mut self, request: CtxRequest<'_>, work: &mut dyn FnMut(&mut RemedyCtx<'_>)) {
        let mut ctx = RemedyCtx::new(
            &mut *self.jobs,
            &mut *self.cache,
            &mut *self.index,
            &mut *self.edge,
            &mut *self.quarantine,
            &mut *self.cases,
            &mut *self.documents,
            request.audit,
            request.actor,
            request.now,
            request.settings,
        );
        work(&mut ctx);
    }
}

/// The composer: the provider of `dyn WorldAccess`.
pub struct WorldComposer {
    settings: Settings,
    parts: OnceLock<Parts>,
    /// Held while a world is open: a remedy on the live world and a snapshot never interleave.
    gate: Mutex<()>,
}

impl WorldComposer {
    /// A composer whose parts are filled later (by the finaliser).
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            parts: OnceLock::new(),
            gate: Mutex::new(()),
        }
    }

    /// A composer over `parts` already: the seam the tests build a world from (a `Parts` from
    /// [`fixture::FixtureWorld::parts`], or [`Parts::none`] for a site with the stubs).
    #[must_use]
    pub fn with_parts(settings: Settings, parts: Parts) -> Self {
        let composer = Self::new(settings);
        composer.fill(parts);
        composer
    }

    /// Fills the parts, once. A second fill is ignored and answers `false`.
    pub fn fill(&self, parts: Parts) -> bool {
        self.parts.set(parts).is_ok()
    }

    /// The parts the site lacks, or `None` while they have not been resolved yet.
    #[must_use]
    pub fn missing(&self) -> Option<Vec<PartName>> {
        self.parts.get().map(Parts::missing)
    }

    /// The readiness reason: the missing parts (`no jobs, cache`), or `None` when every part is
    /// live.
    #[must_use]
    pub fn degraded(&self) -> Option<String> {
        let Some(parts) = self.parts.get() else {
            return Some("parts not resolved yet".to_owned());
        };
        let missing = parts.missing();
        if missing.is_empty() {
            return None;
        }
        let names: Vec<&str> = missing.iter().map(|name| name.as_str()).collect();
        Some(format!("no {}", names.join(", ")))
    }
}

impl WorldAccess for WorldComposer {
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> std::result::Result<(), WorldError> {
        let Some(parts) = self.parts.get() else {
            return Err(WorldError(NOT_ASSEMBLED));
        };
        let _open = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        match kind {
            WorldKind::Live => {
                let actor = request.actor;
                parts.live(actor).run(request, work);
                Ok(())
            }
            WorldKind::Rehearsal => {
                if !self.settings.rehearsal {
                    return Err(WorldError(REHEARSAL_OFF));
                }
                parts.rehearsal()?.run(request, work);
                Ok(())
            }
        }
    }
}

/// The service: the readiness entry. Its check always passes; its degradation names the missing
/// parts.
struct WorldService {
    composer: Arc<WorldComposer>,
}

impl Service for WorldService {
    fn name(&self) -> &'static str {
        "world_access"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }

    fn degraded(&self) -> Option<String> {
        self.composer.degraded()
    }
}

/// Registers the composer: its readiness entry and the `dyn WorldAccess` port, and queues the
/// finaliser that resolves the parts once every module has registered. Inert when
/// `services.world_access.enabled = false`.
///
/// # Errors
///
/// A refusal headed `world_access.settings_invalid` for a bad settings table.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx
        .settings_or_default("world_access")
        .map_err(|e| Error(format!("{SETTINGS_INVALID}: {e}")))?;
    if !ctx.assembling() {
        return Ok(());
    }
    if !settings.enabled {
        tracing::info!("world_access is switched off: remedies have no world to run on");
        return Ok(());
    }
    let composer = Arc::new(WorldComposer::new(settings));
    ctx.add(Arc::new(WorldService {
        composer: Arc::clone(&composer),
    }));
    let world: Arc<dyn WorldAccess> = Arc::clone(&composer) as Arc<dyn WorldAccess>;
    ctx.provide(world)?;
    ctx.finally(move |ctx| {
        let parts = Parts::from_site(ctx);
        let missing = parts.missing();
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().map(|name| name.as_str()).collect();
            tracing::info!(missing = %names.join(", "), "world_access: some parts are not available on this site");
        }
        composer.fill(parts);
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on() {
        let s = Settings::default();
        assert!(s.enabled && s.rehearsal);
        let parsed: Settings = toml::from_str("").unwrap();
        assert!(parsed.enabled && parsed.rehearsal);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
        let off: Settings = toml::from_str("rehearsal = false").unwrap();
        assert!(off.enabled && !off.rehearsal);
    }

    // The start order is below the remedy host's, proved when the crate compiles.
    const _: () = assert!(ORDER < crate::services::remedies::ORDER);

    #[test]
    fn no_parts_is_degraded_naming_every_part_in_order() {
        let composer = WorldComposer::with_parts(Settings::default(), Parts::none());
        assert_eq!(
            composer.degraded().as_deref(),
            Some("no jobs, cache, index, edge, quarantine, cases, documents")
        );
        assert_eq!(composer.missing().unwrap().len(), 7);
    }

    #[test]
    fn a_composer_not_yet_filled_says_so_and_opens_no_world() {
        let composer = WorldComposer::new(Settings::default());
        assert!(composer.missing().is_none());
        assert_eq!(
            composer.degraded().as_deref(),
            Some("parts not resolved yet")
        );
    }

    #[test]
    fn a_composer_fills_once() {
        let composer = WorldComposer::new(Settings::default());
        assert!(composer.fill(Parts::none()));
        assert!(!composer.fill(Parts::none()));
    }

    #[test]
    fn a_full_fixture_world_is_not_degraded() {
        let world = fixture::FixtureWorld::new(spot_remedies::FakeWorld::new());
        let composer = WorldComposer::with_parts(Settings::default(), world.parts());
        assert_eq!(composer.degraded(), None);
        let mut parts = world.parts();
        parts.cache = None;
        parts.documents = None;
        let composer = WorldComposer::with_parts(Settings::default(), parts);
        assert_eq!(composer.degraded().as_deref(), Some("no cache, documents"));
    }

    #[test]
    fn the_part_names_are_stable() {
        let names: Vec<&str> = PartName::ALL.iter().map(|n| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "jobs",
                "cache",
                "index",
                "edge",
                "quarantine",
                "cases",
                "documents"
            ]
        );
        assert!(
            PartName::Jobs
                .rehearsal_unavailable()
                .to_string()
                .ends_with("rehearsal unavailable for jobs")
        );
    }
}
