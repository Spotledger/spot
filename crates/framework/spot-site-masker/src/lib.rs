//! The host's [`FieldMasker`] for the audit participant (WP-PRD-01.T20, ADR-070 section 12
//! rule 9).
//!
//! `spot_doc_audit::AuditParticipant` stores the **masked** form of every protected field. The
//! masker it needs is a [`GuardMasker`] over a [`FieldGuard`] and a [`ProtectionSource`]; this
//! crate builds both from the effective metadata the site is actually serving.
//!
//! # One instance per metadata generation
//!
//! A [`SiteMasker`] is immutable. [`SiteMasker::new`] reads the protection facts of one
//! [`EffectiveMeta`] once into a single [`MetaProtection`], which the guard (write path) and the
//! masker (reader path) share, so both see the same facts. There is no cell, no cache and no
//! invalidation: a metadata swap is "build a new `SiteMasker` and hand it to the new site".
//! `spot_cli::host::build_site`, which already builds a new engine per metadata generation,
//! calls [`SiteMasker::new`] exactly once per generation. A request that still holds the old
//! instance keeps the old facts until it finishes; the next request sees the new ones.
//!
//! # Fail closed
//!
//! * A DocType the snapshot does not hold is refused with
//!   [`DocAuditError::UnknownDoctype`]. `AuditStep::build` propagates that error, so no audit
//!   row is written unmasked and the write that was being audited fails with the refusal.
//! * A table whose child DocType the snapshot does not hold has its changes dropped and
//!   `hidden_changes` set, as [`GuardMasker`] does; a child column is judged on the child
//!   DocType's own facts.
//! * `DocProtection` holds the protection facts of a DocType's fields (groups and mask), so a
//!   field with no facts, or none naming a group or a mask, is unprotected by definition. The fail-closed cases are the unknown DocType and the unknown
//!   child DocType, not an unlisted field.
//! * The mask is applied whatever the acting identity, the system principal included.
//! * Every failure maps to a [`DocAuditError`]; no document value, diff value or mask input is
//!   in any error, `Debug` output or log line of this crate (its `Debug` prints no facts).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::fmt;
use std::sync::Arc;

use spot_activity::FieldChange;
use spot_app_api::prelude::Clock;
use spot_doc_audit::{AuditMask, DocAuditError, FieldMasker, GuardMasker};
use spot_field_guard::{
    AuditSink, FieldGuard, MetaProtection, ProtectionSource, RedactedDiff, TraceSink,
};
use spot_meta::EffectiveMeta;
use spot_permissions::{DocFacts, PermissionEngine, Principal};

/// What a [`FieldGuard`] needs besides the protection source; the host supplies them.
#[derive(Clone)]
pub struct GuardDeps {
    /// The permission engine of the site's current permission snapshot.
    pub engine: Arc<PermissionEngine>,
    /// Where field access trace records go.
    pub trace: Arc<dyn TraceSink>,
    /// Where field guard audit records go.
    pub audit: Arc<dyn AuditSink>,
    /// The clock the records are stamped with.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for GuardDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardDeps").finish_non_exhaustive()
    }
}

/// The masker of one metadata generation. Immutable; build a new one on every swap.
pub struct SiteMasker {
    inner: GuardMasker,
    source: Arc<MetaProtection>,
}

impl fmt::Debug for SiteMasker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SiteMasker").finish_non_exhaustive()
    }
}

impl SiteMasker {
    /// The masker for the metadata `meta`. The host calls this once per metadata generation.
    #[must_use]
    pub fn new(meta: &EffectiveMeta, deps: GuardDeps) -> SiteMasker {
        let source = Arc::new(MetaProtection::new(meta));
        let as_source: Arc<dyn ProtectionSource> = source.clone();
        let guard = Arc::new(FieldGuard::new(
            deps.engine,
            Arc::clone(&as_source),
            deps.trace,
            deps.audit,
            deps.clock,
        ));
        SiteMasker {
            inner: GuardMasker::new(guard, as_source),
            source,
        }
    }

    /// The field guard of this generation, for the host's other field-guard users.
    #[must_use]
    pub fn guard(&self) -> &Arc<FieldGuard> {
        self.inner.guard()
    }

    /// The protection facts of this generation, the same the guard reads.
    #[must_use]
    pub fn source(&self) -> &Arc<MetaProtection> {
        &self.source
    }
}

impl FieldMasker for SiteMasker {
    fn mask_for_audit(
        &self,
        doctype: &str,
        changes: &[FieldChange],
    ) -> Result<AuditMask, DocAuditError> {
        self.inner.mask_for_audit(doctype, changes)
    }

    fn redact_diff(
        &self,
        principal: &Principal,
        doctype: &str,
        facts: &DocFacts,
        changes: &[FieldChange],
    ) -> Result<RedactedDiff, DocAuditError> {
        self.inner.redact_diff(principal, doctype, facts, changes)
    }

    fn can_read(&self, principal: &Principal, doctype: &str, facts: &DocFacts) -> bool {
        self.inner.can_read(principal, doctype, facts)
    }
}
