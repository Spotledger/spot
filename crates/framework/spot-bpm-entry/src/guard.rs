//! The `process_only` guard (ADR-080 section 2.3): in a `process_only` scope a record type is
//! created only by starting its entry process.
//!
//! # No bypass
//!
//! In this mode there is **no standing outside-process permission** and no override route
//! (decided 17 Oct 2026). The guard therefore takes no principal and consults no permission:
//! the only thing that lets a create through is a request context for the type's entry
//! process, at a step that may create the type. The desk's New action, a list's add button, the
//! API's create endpoint, an import and a script all create through the record type's own
//! lifecycle, so all of them reach the same `before_create` hook and are refused the same way.
//! The refusal tells the caller the entry process to start ([`EntryError::StartProcess`]), so an
//! API client can start it. The outside-process permission with a reason remains only for
//! `governed` mode and is the governance crate's.
//!
//! Replication is not creation: the apply path stores what it received and runs no lifecycle
//! hooks (ADR-062), so this guard is never asked about it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use spot_app_api::context::Actor;
use spot_app_api::lifecycle::{HookPoint, Operation, Phase, Target};
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_app_api::{AppResult, BoxFuture, Controller, HookContext, StepDecl, StepName};
use spot_bpm_governance::{AuditSink, GovernanceAudit, GovernanceMode, PolicyStore, actor_text};

// The authenticated-grant port lives in `spot-bpm-governance` (WP-BPM-08.T2), so the `governed`
// guard there and this `process_only` guard share one definition instead of two copies that can
// drift. It is re-exported here for the callers of this crate.
pub use spot_bpm_governance::{MemoryRequests, NoRequests, RequestAuthority, VerifiedRequest};

use crate::entry::{EntryProcesses, ProcessRef};
use crate::error::{EntryError, EntryResult};

/// Decides the scope a create is judged in. The scope must come from somewhere the creator does
/// not control: a company the document names is only a claim, and it is checked against what
/// the acting person may use.
pub trait ScopeResolver: Send + Sync {
    /// The validated scope for `actor` creating in `claimed` (the document's own company field,
    /// when it has one), or `None` when the claim names an unknown scope or one the person has
    /// no right to use.
    fn resolve(&self, actor: &str, claimed: Option<&str>) -> Option<String>;
}

/// An in-memory [`ScopeResolver`]: each person has the scopes they may create in, and a claim
/// is valid when it is one of them or below one of them. No claim means the person's first
/// scope.
#[derive(Debug, Default)]
pub struct MemoryScopes {
    allowed: RwLock<BTreeMap<String, Vec<String>>>,
    known: RwLock<BTreeSet<String>>,
}

impl MemoryScopes {
    /// An empty resolver: every claim is refused.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares a scope that exists.
    pub fn define(&self, scope: &str) {
        self.known
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(scope.to_owned());
    }

    /// Lets `actor` create in `scope` and below.
    pub fn grant(&self, actor: &str, scope: &str) {
        self.define(scope);
        self.allowed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(actor.to_owned())
            .or_default()
            .push(scope.to_owned());
    }

    /// Wraps the resolver for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl ScopeResolver for MemoryScopes {
    fn resolve(&self, actor: &str, claimed: Option<&str>) -> Option<String> {
        let allowed = self
            .allowed
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mine = allowed.get(actor)?;
        let Some(claim) = claimed.map(str::trim).filter(|text| !text.is_empty()) else {
            return mine.first().cloned();
        };
        let known = self
            .known
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let inside = crate::scope::scope_chain(claim)
            .iter()
            .any(|unit| mine.iter().any(|own| own == unit));
        (known.contains(claim) && inside).then(|| claim.to_owned())
    }
}

/// One create attempt.
#[derive(Clone, Debug)]
pub struct EntryAttempt<'a> {
    /// The scope path the record is created in.
    pub scope: &'a str,
    /// The record type.
    pub doctype: &'a str,
    /// The record being created.
    pub document_id: RecordId,
    /// Who is acting.
    pub actor: &'a Actor,
    /// The request step the authority verified for this actor, if any.
    pub request: Option<&'a VerifiedRequest>,
    /// The invocation's command id.
    pub command_id: Option<&'a str>,
    /// The business time.
    pub now: OffsetDateTime,
}

/// Where a create should go: straight to the lifecycle, or through the entry process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateRoute {
    /// The record type is not `process_only` here; create directly.
    Direct,
    /// Start this process, as the calling principal.
    StartProcess(ProcessRef),
}

/// The `process_only` decision and its audit.
pub struct ProcessOnlyGuard {
    modes: Arc<dyn PolicyStore>,
    entries: Arc<EntryProcesses>,
    requests: Arc<dyn RequestAuthority>,
    audit: Arc<dyn AuditSink>,
}

impl ProcessOnlyGuard {
    /// A guard over the mode source (the same store the governance guard reads), the entry
    /// process resolver, the running requests and the audit sink.
    #[must_use]
    pub fn new(
        modes: Arc<dyn PolicyStore>,
        entries: Arc<EntryProcesses>,
        requests: Arc<dyn RequestAuthority>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            modes,
            entries,
            requests,
            audit,
        }
    }

    /// The mode of `doctype` in `scope` at `now`.
    ///
    /// # Errors
    /// A settings error.
    pub fn mode(
        &self,
        scope: &str,
        doctype: &str,
        now: OffsetDateTime,
    ) -> EntryResult<GovernanceMode> {
        Ok(self.modes.mode_at(scope, doctype, now)?)
    }

    /// Where a create of `doctype` in `scope` should go. A generic import, a script or an API
    /// client asks this first and starts the process in `process_only` scopes, as the calling
    /// principal.
    ///
    /// # Errors
    /// A settings error or an invalid entry setting.
    pub fn route(
        &self,
        scope: &str,
        doctype: &str,
        now: OffsetDateTime,
    ) -> EntryResult<CreateRoute> {
        if self.mode(scope, doctype, now)? == GovernanceMode::ProcessOnly {
            return Ok(CreateRoute::StartProcess(
                self.entries.entry_process(doctype, scope)?,
            ));
        }
        Ok(CreateRoute::Direct)
    }

    /// Decides one create attempt.
    ///
    /// Outside `process_only` the guard allows (governance decides `governed`). In
    /// `process_only` it allows only an attempt that carries a request context whose request
    /// runs the type's entry process and whose node may create the type; otherwise it audits
    /// the refusal and fails with [`EntryError::StartProcess`].
    ///
    /// # Errors
    /// [`EntryError::StartProcess`] for a refused attempt, [`EntryError::Audit`] when the
    /// refusal cannot be audited (the create fails closed), or a settings error.
    pub fn check_create(&self, attempt: &EntryAttempt<'_>) -> EntryResult<()> {
        let mode = self.mode(attempt.scope, attempt.doctype, attempt.now)?;
        if mode != GovernanceMode::ProcessOnly {
            return Ok(());
        }
        let entry = self.entries.entry_process(attempt.doctype, attempt.scope)?;
        let valid = attempt.request.is_some_and(|step| {
            self.requests.process_of(&step.request).as_deref() == Some(entry.id.as_str())
                && self.requests.may_create(step, attempt.doctype)
        });
        if valid {
            return Ok(());
        }
        let detail = format!(
            "the record type is process_only here; start the process `{}` instead",
            entry.id
        );
        self.audit.write(&GovernanceAudit::refused(
            attempt.actor,
            attempt.scope,
            attempt.doctype,
            attempt.document_id,
            GovernanceMode::ProcessOnly,
            &detail,
            attempt.command_id,
            attempt.now,
        ))?;
        Err(EntryError::StartProcess {
            doctype: attempt.doctype.to_owned(),
            process: entry.id,
        })
    }
}

/// The name of the `before_create` step.
pub const GUARD_STEP: &str = "bpm:entry_process_only";

/// The engine hook: a `before_create` step on one record type, so every channel that creates a
/// record is subject to the guard because they all run the record's own create lifecycle.
///
/// The scope the mode is judged in is not read from the document: the document's company is a
/// claim, resolved through the [`ScopeResolver`] against what the acting person may use, and a
/// claim that does not resolve is refused ([`EntryError::ScopeInvalid`]) before any mode is
/// consulted. The request context is whatever the authority verifies for this actor; the
/// command id itself proves nothing.
pub struct ProcessOnlyController {
    doctype: String,
    guard: Arc<ProcessOnlyGuard>,
    scopes: Arc<dyn ScopeResolver>,
}

impl ProcessOnlyController {
    /// A controller for `doctype`.
    #[must_use]
    pub fn new(
        doctype: &str,
        guard: Arc<ProcessOnlyGuard>,
        scopes: Arc<dyn ScopeResolver>,
    ) -> Self {
        Self {
            doctype: doctype.to_owned(),
            guard,
            scopes,
        }
    }

    fn decide(&self, ctx: &dyn HookContext) -> EntryResult<()> {
        let doc = ctx.doc();
        let inv = ctx.invocation();
        let actor = actor_text(&inv.actor);
        let claimed = doc.text("company").ok().flatten();
        let Some(scope) = self.scopes.resolve(&actor, claimed) else {
            return Err(EntryError::ScopeInvalid {
                doctype: self.doctype.clone(),
            });
        };
        let command_id = inv.command_id.as_ref().map(|c| c.as_str().to_owned());
        let request = command_id
            .as_deref()
            .and_then(|key| self.guard.requests.authenticate(key, &actor));
        self.guard.check_create(&EntryAttempt {
            scope: &scope,
            doctype: &self.doctype,
            document_id: doc.id(),
            actor: &inv.actor,
            request: request.as_ref(),
            command_id: command_id.as_deref(),
            now: inv.now,
        })
    }
}

impl Controller for ProcessOnlyController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        let Ok(name) = StepName::new(GUARD_STEP) else {
            return Vec::new();
        };
        vec![StepDecl {
            name,
            point: HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
            priority: 110,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if step.as_str() != GUARD_STEP {
                return Ok(());
            }
            if ctx.point() != &HookPoint::of(Phase::Before, Target::Op(Operation::Create)) {
                return Ok(());
            }
            self.decide(&*ctx).map_err(|error| error.to_app_error())
        })
    }
}
