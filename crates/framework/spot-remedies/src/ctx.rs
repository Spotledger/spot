//! The remedy context: the only handle a remedy has, and the gate it runs behind.
//!
//! A [`RemedyCtx`] wraps the typed ports, the caller's instant, the settings and the audit sink.
//! It offers no raw storage handle; the only way to change a document is
//! [`RemedyCtx::apply_document`], which enforces the hard floor — a submitted financial record
//! changes only through cancel, amend or a journal entry.
//!
//! Time is an argument: the context carries the caller's `now` in unix seconds and never reads a
//! clock. Fingerprints read back a typed target's state so the framework can detect an undo
//! conflict.

use spot_agent_tiers::{Approval, Context as TierContext, Domain, FinancialRoute};

use crate::audit::{AuditEvent, AuditKind, AuditSink};
use crate::error::RemedyError;
use crate::model::RemedyId;
use crate::ports::{
    CacheNs, CacheOps, CaseOps, DocOp, DocRef, DocState, DocumentChange, DocumentOps, EdgeOps,
    IndexOps, JobOps, QuarantineOps,
};

/// The deployment settings the framework reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemedySettings {
    /// How long an undo record stays usable, in seconds.
    pub undo_window_secs: i64,
    /// The domain remedies belong to (support).
    pub domain: Domain,
}

impl Default for RemedySettings {
    /// The default: a one-day undo window, in the support domain.
    fn default() -> Self {
        Self {
            undo_window_secs: 86_400,
            domain: Domain::Support,
        }
    }
}

/// The facts a run carries to the tier authoriser: the change journal, the snapshot and any
/// approval. The hard floor binds every remedy, so a remedy run uses [`Gate::change`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gate {
    /// A human approval, for a T3 remedy run by a non-deep agent.
    pub approval: Option<Approval>,
    /// Whether the change journal is on.
    pub journaled: bool,
    /// Whether a snapshot was already taken.
    pub snapshot_taken: bool,
    /// The route for a change to submitted financial records, if the remedy targets one.
    pub financial_route: Option<FinancialRoute>,
}

impl Gate {
    /// A change gate: journaled and snapshotted, as every remedy run must be.
    #[must_use]
    pub fn change() -> Self {
        Self {
            approval: None,
            journaled: true,
            snapshot_taken: true,
            financial_route: None,
        }
    }

    /// The gate with a carried human approval.
    #[must_use]
    pub fn with_approval(mut self, approval: Approval) -> Self {
        self.approval = Some(approval);
        self
    }

    /// The gate with the route a financial target changes through.
    #[must_use]
    pub fn with_financial_route(mut self, route: FinancialRoute) -> Self {
        self.financial_route = Some(route);
        self
    }

    /// The tier authoriser's view of this gate.
    #[must_use]
    pub fn tier_context(&self) -> TierContext {
        TierContext {
            human_approval: self.approval.clone(),
            journaled: self.journaled,
            snapshot_taken: self.snapshot_taken,
            targets_submitted_financial: self.financial_route,
        }
    }
}

/// The only handle a remedy has: typed ports, the instant, the settings and the audit sink.
pub struct RemedyCtx<'a> {
    jobs: &'a mut dyn JobOps,
    cache: &'a mut dyn CacheOps,
    index: &'a mut dyn IndexOps,
    edge: &'a mut dyn EdgeOps,
    quarantine: &'a mut dyn QuarantineOps,
    case: &'a mut dyn CaseOps,
    document: &'a mut dyn DocumentOps,
    audit: &'a mut dyn AuditSink,
    actor: &'a str,
    now: i64,
    settings: &'a RemedySettings,
}

impl<'a> RemedyCtx<'a> {
    /// Builds a context over the ports. The host (or the self-test fixture) supplies every
    /// reference; a remedy never builds one.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        jobs: &'a mut dyn JobOps,
        cache: &'a mut dyn CacheOps,
        index: &'a mut dyn IndexOps,
        edge: &'a mut dyn EdgeOps,
        quarantine: &'a mut dyn QuarantineOps,
        case: &'a mut dyn CaseOps,
        document: &'a mut dyn DocumentOps,
        audit: &'a mut dyn AuditSink,
        actor: &'a str,
        now: i64,
        settings: &'a RemedySettings,
    ) -> Self {
        Self {
            jobs,
            cache,
            index,
            edge,
            quarantine,
            case,
            document,
            audit,
            actor,
            now,
            settings,
        }
    }

    /// The job port.
    pub fn jobs(&mut self) -> &mut dyn JobOps {
        self.jobs
    }

    /// The cache port.
    pub fn cache(&mut self) -> &mut dyn CacheOps {
        self.cache
    }

    /// The retrieval-index port.
    pub fn index(&mut self) -> &mut dyn IndexOps {
        self.index
    }

    /// The edge-sync port.
    pub fn edge(&mut self) -> &mut dyn EdgeOps {
        self.edge
    }

    /// The quarantine port.
    pub fn quarantine(&mut self) -> &mut dyn QuarantineOps {
        self.quarantine
    }

    /// The BPM port.
    pub fn case(&mut self) -> &mut dyn CaseOps {
        self.case
    }

    /// The document port.
    pub fn document(&mut self) -> &mut dyn DocumentOps {
        self.document
    }

    /// The caller's instant (unix seconds).
    #[must_use]
    pub fn now(&self) -> i64 {
        self.now
    }

    /// The deployment settings.
    #[must_use]
    pub fn settings(&self) -> &RemedySettings {
        self.settings
    }

    /// The acting agent's id.
    #[must_use]
    pub fn actor(&self) -> &str {
        self.actor
    }

    /// Records an audit event for the acting agent at the context's instant.
    pub fn record(&mut self, kind: AuditKind, remedy: &RemedyId, detail: &str) {
        let event = AuditEvent::new(kind, remedy, self.actor, self.now, detail);
        self.audit.record(event);
    }

    /// Runs a typed document operation, enforcing the hard floor.
    ///
    /// # Errors
    ///
    /// [`RemedyError::HardFloorFinancial`] when the document is a submitted financial record and
    /// the operation is a direct edit (anything but cancel, amend or journal entry);
    /// [`RemedyError::OpFailed`] when the document does not exist or the operation is invalid in
    /// its state.
    pub fn apply_document(
        &mut self,
        doc: &DocRef,
        op: DocOp,
    ) -> Result<DocumentChange, RemedyError> {
        let view = self
            .document
            .view(doc)
            .ok_or_else(|| RemedyError::OpFailed {
                operation: op.name().to_owned(),
                detail: format!("no document `{}` exists", doc.as_str()),
            })?;
        if view.financial && view.state == DocState::Submitted && !op.is_allowed_financial_route() {
            return Err(RemedyError::HardFloorFinancial {
                document: doc.as_str().to_owned(),
                state: view.state.as_str().to_owned(),
            });
        }
        self.document
            .run(doc, &op)
            .map_err(|error| RemedyError::OpFailed {
                operation: error.operation,
                detail: error.detail,
            })
    }

    /// Reads back a typed target's state fingerprint, for the undo conflict check.
    ///
    /// A target is `<kind>:<key>`: `job:17`, `cache:sessions`, `index:retrieval`,
    /// `edge:customer-1`, `quarantine:q1`, `case:case1`, `doc:SINV-1`.
    #[must_use]
    pub fn fingerprint(&self, target: &str) -> Option<String> {
        let (kind, key) = target.split_once(':')?;
        match kind {
            "job" => {
                let view = self.jobs.job(&crate::ports::JobRef::new(key))?;
                Some(view.life.as_str().to_owned())
            }
            "cache" => {
                let count = self.cache.entries(&CacheNs::new(key));
                Some(format!("entries={count}"))
            }
            "index" => {
                let view = self.index.index(&crate::ports::IndexRef::new(key))?;
                Some(format!("{}:gen{}", view.health.as_str(), view.generation))
            }
            "edge" => {
                let view = self.edge.edge(&crate::ports::EdgeRef::new(key))?;
                Some(format!("edges={}", view.actual))
            }
            "quarantine" => {
                let view = self
                    .quarantine
                    .item(&crate::ports::QuarantineRef::new(key))?;
                Some(view.state.as_str().to_owned())
            }
            "case" => {
                let view = self.case.case(&crate::ports::CaseRef::new(key))?;
                Some(view.life.as_str().to_owned())
            }
            "doc" => {
                let view = self.document.view(&DocRef::new(key))?;
                Some(crate::ports::document_fingerprint(view))
            }
            _ => None,
        }
    }
}
