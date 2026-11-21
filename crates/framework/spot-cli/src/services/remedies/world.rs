//! The seams between the remedy host and the rest of the site: the world a remedy changes, the
//! catalogue it is loaded from and the audit events it leaves.
//!
//! A remedy changes the world only through the typed ports of `RemedyCtx` (jobs, cache, retrieval
//! index, edge sync, quarantine, BPM cases, documents). Which implementation of those ports a call
//! runs on is the host's decision, so it is a seam: [`WorldAccess`] hands the host a context over
//! the live site, or over a rehearsal copy of it ([`WorldKind::Rehearsal`], a snapshot: the fix
//! pipeline rehearses on it and never on the live site). The modules that own the subsystems
//! provide the implementation; with none provided the host can list remedies and prove their
//! self-tests but cannot run one.

use spot_remedies::{AuditEvent, AuditSink, Remedy, RemedyCtx, RemedySettings, seeds};

/// Which copy of the world a context is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorldKind {
    /// The running site.
    Live,
    /// A snapshot of it, safe to change and throw away.
    Rehearsal,
}

/// What a context is built with besides the ports.
pub struct CtxRequest<'a> {
    /// The acting agent's id.
    pub actor: &'a str,
    /// The instant, in unix seconds (the injected clock's).
    pub now: i64,
    /// The remedy settings.
    pub settings: &'a RemedySettings,
    /// Where the framework's audit events go.
    pub audit: &'a mut dyn AuditSink,
}

/// Why a world could not be opened. The text is fixed and names no data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldError(pub &'static str);

impl std::fmt::Display for WorldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for WorldError {}

/// The typed ports of one copy of the world.
///
/// The methods are synchronous because the remedy trait is; the host calls them from a blocking
/// thread, so an implementation over asynchronous storage may block on its handle there.
pub trait WorldAccess: Send + Sync {
    /// Opens the world of `kind` and runs `work` with a context over its ports.
    ///
    /// The implementation builds the context with `RemedyCtx::new` from its own ports and the
    /// request's actor, instant, settings and audit sink, runs `work` once and returns.
    ///
    /// # Errors
    ///
    /// [`WorldError`] when the world cannot be opened (and `work` did not run).
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> Result<(), WorldError>;
}

/// Where the remedies come from. The shipped catalogue is the default; another module may provide
/// a larger one as a port. The host builds two registries from it (live and rehearsal), so it
/// must hand out a fresh set each time.
pub trait Catalogue: Send + Sync {
    /// A fresh set of every remedy.
    fn remedies(&self) -> Vec<Box<dyn Remedy>>;
}

/// The remedies that ship with `spot-remedies`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShippedCatalogue;

impl Catalogue for ShippedCatalogue {
    fn remedies(&self) -> Vec<Box<dyn Remedy>> {
        seeds::all()
    }
}

/// Collects the framework's audit events during one blocking call, so the host can append them to
/// the site's audit chain afterwards.
#[derive(Debug, Default)]
pub struct Collect {
    events: Vec<AuditEvent>,
}

impl Collect {
    /// The events collected, oldest first.
    #[must_use]
    pub fn into_events(self) -> Vec<AuditEvent> {
        self.events
    }
}

impl AuditSink for Collect {
    fn record(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}
