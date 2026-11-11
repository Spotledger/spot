//! The entry mode per record type, narrowed by organisation-unit scope (ADR-080 section 2.1).
//!
//! A scope is a path of organisation units, root first, joined with `/` (`acme`,
//! `acme/north`, `acme/north/shop1`). A mode set on a unit applies to every unit below it until
//! a deeper unit sets its own: the most specific explicit setting wins, and a type with no
//! setting anywhere is `free`.
//!
//! [`ScopedPolicyStore`] implements the governance crate's [`PolicyStore`], reading the
//! `company` argument as the scope path. The governance guard and this crate's guard therefore
//! read one source and cannot disagree about a record type's mode. A change is effective-dated
//! (records created before its instant keep the earlier mode) and audited through the
//! governance audit sink ([`ModeAdmin`]).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_app_api::context::Actor;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_governance::{
    AuditSink, GovernanceAudit, GovernanceError, GovernanceMode, GovernancePolicy, PolicyChange,
    PolicyStore, effective_mode,
};

use crate::error::EntryResult;

/// The separator between organisation units in a scope path.
pub const SCOPE_SEPARATOR: char = '/';

/// The scope and each of its ancestors, most specific first: `a/b/c` gives `a/b/c`, `a/b`, `a`.
#[must_use]
pub fn scope_chain(scope: &str) -> Vec<&str> {
    let mut chain = Vec::new();
    let mut end = scope.len();
    loop {
        let prefix = &scope[..end];
        if !prefix.is_empty() {
            chain.push(prefix);
        }
        match prefix.rfind(SCOPE_SEPARATOR) {
            Some(index) => end = index,
            None => break,
        }
    }
    chain
}

/// An in-memory, effective-dated, scope-narrowing policy store.
#[derive(Debug, Default)]
pub struct ScopedPolicyStore {
    // (scope, doctype) -> its changes.
    changes: RwLock<BTreeMap<(String, String), Vec<PolicyChange>>>,
    min_reason: usize,
}

impl ScopedPolicyStore {
    /// An empty store (every type is `free` everywhere). `min_reason` is the governed-mode
    /// reason floor the governance guard reads.
    #[must_use]
    pub fn new(min_reason: usize) -> Self {
        Self {
            changes: RwLock::default(),
            min_reason,
        }
    }

    /// Wraps the store for sharing between the guards.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Sets a mode from the beginning of time without an audit entry: for a host's bootstrap
    /// and for tests. A runtime change goes through [`ModeAdmin`], which is audited.
    pub fn seed(&self, scope: &str, doctype: &str, mode: GovernanceMode) {
        let epoch = OffsetDateTime::UNIX_EPOCH;
        let change = PolicyChange::new(
            scope,
            doctype,
            GovernanceMode::Free,
            mode,
            epoch,
            "seed",
            epoch,
        );
        self.changes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry((scope.to_owned(), doctype.to_owned()))
            .or_default()
            .push(change);
    }

    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, BTreeMap<(String, String), Vec<PolicyChange>>> {
        self.changes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The mode the most specific explicit setting gives at `at`, or `None` when no unit on the
    /// chain has a setting in force yet.
    fn explicit_at(
        &self,
        scope: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Option<GovernanceMode> {
        let map = self.read();
        for unit in scope_chain(scope) {
            let Some(changes) = map.get(&(unit.to_owned(), doctype.to_owned())) else {
                continue;
            };
            if changes.iter().any(|change| change.effective_at <= at) {
                return Some(effective_mode(changes, at, GovernanceMode::Free));
            }
        }
        None
    }
}

impl PolicyStore for ScopedPolicyStore {
    fn mode_at(
        &self,
        company: &str,
        doctype: &str,
        at: OffsetDateTime,
    ) -> Result<GovernanceMode, GovernanceError> {
        Ok(self
            .explicit_at(company, doctype, at)
            .unwrap_or(GovernanceMode::Free))
    }

    fn min_reason_chars(&self, _company: &str) -> Result<usize, GovernanceError> {
        Ok(self.min_reason)
    }

    fn current(&self, company: &str, doctype: &str) -> Result<GovernancePolicy, GovernanceError> {
        let map = self.read();
        for unit in scope_chain(company) {
            if let Some(change) = map
                .get(&(unit.to_owned(), doctype.to_owned()))
                .and_then(|changes| changes.iter().max_by_key(|change| change.effective_at))
            {
                return Ok(GovernancePolicy::new(company, doctype, change.to));
            }
        }
        Ok(GovernancePolicy::free(company, doctype))
    }

    fn changes(&self, company: &str, doctype: &str) -> Result<Vec<PolicyChange>, GovernanceError> {
        Ok(self
            .read()
            .get(&(company.to_owned(), doctype.to_owned()))
            .cloned()
            .unwrap_or_default())
    }

    fn record_change(&self, change: PolicyChange) -> Result<(), GovernanceError> {
        self.changes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry((change.company.clone(), change.doctype.clone()))
            .or_default()
            .push(change);
        Ok(())
    }
}

/// Changes a record type's mode in a scope: audited, effective-dated, never retroactive.
pub struct ModeAdmin {
    store: Arc<ScopedPolicyStore>,
    audit: Arc<dyn AuditSink>,
}

impl ModeAdmin {
    /// An admin over the store and the audit sink.
    #[must_use]
    pub fn new(store: Arc<ScopedPolicyStore>, audit: Arc<dyn AuditSink>) -> Self {
        Self { store, audit }
    }

    /// Sets `doctype` to `mode` in `scope` from `effective_at` on. The audit entry is written
    /// first: when it cannot be written the change is not made.
    ///
    /// # Errors
    /// [`crate::EntryError::Audit`] when the audit write fails, or a settings error.
    pub fn set_mode(
        &self,
        scope: &str,
        doctype: &str,
        mode: GovernanceMode,
        effective_at: OffsetDateTime,
        actor: &Actor,
        now: OffsetDateTime,
    ) -> EntryResult<()> {
        let from = self.store.mode_at(scope, doctype, now)?;
        let detail = format!("{from} to {mode} effective {effective_at}");
        self.audit.write(&GovernanceAudit::policy_change(
            actor, scope, doctype, mode, &detail, now,
        ))?;
        self.store.record_change(PolicyChange::new(
            scope,
            doctype,
            from,
            mode,
            effective_at,
            &spot_bpm_governance::actor_text(actor),
            now,
        ))?;
        Ok(())
    }
}
