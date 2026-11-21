//! The live implementations of the read-time seams: the permission-checked read as the calling
//! user, the field guard over the metadata served now, and the index policy loaded against the
//! permission snapshot served now.
//!
//! Nothing here decides a right. [`LoaderAccess`] asks the document engine to load the document
//! *as the caller* (the engine applies the role grants, the user permissions, the share and field
//! rules of the live decision core), and treats anything but a document coming back as "may not
//! read": a denial, a missing document, an engine that cannot answer. The answer is asked on every
//! query, so a permission revoked a moment ago applies to the very next retrieval with no
//! re-index.

use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::RecordId;
use spot_core::{Clock, SiteId};
use spot_field_guard::{
    AuditRecord as GuardAudit, DocProtection, FieldAccessRecord, FieldGuard, MetaProtection, Offer,
    ProtectionSource, TraceSink,
};
use spot_retrieval::{IndexPolicy, RetrievalError};

use super::ports::{Access, Fut, GuardSource};
use crate::live::SiteLive;
use crate::services::ports::DocumentLoader;

/// [`Access`] over a permission-checked document load as the caller.
pub struct LoaderAccess {
    loader: Arc<dyn DocumentLoader>,
    site: SiteId,
    clock: Arc<dyn Clock>,
}

impl LoaderAccess {
    /// An access check that loads through `loader` for `site`.
    #[must_use]
    pub fn new(loader: Arc<dyn DocumentLoader>, site: SiteId, clock: Arc<dyn Clock>) -> Self {
        Self {
            loader,
            site,
            clock,
        }
    }
}

impl Access for LoaderAccess {
    fn can_read<'a>(&'a self, actor: &'a Actor, doctype: &'a str, id: RecordId) -> Fut<'a, bool> {
        Box::pin(async move {
            // Only a signed-in user (or an agent acting as one) reads. The platform and a consumer
            // principal never retrieve through this path: the caller's own identity is the rule.
            if !matches!(actor, Actor::User { .. }) {
                return false;
            }
            let invocation = Invocation {
                site: self.site.clone(),
                actor: actor.clone(),
                command_id: None,
                language: None,
                now: self.clock.now_utc(),
            };
            matches!(
                self.loader.load(&invocation, doctype, id).await,
                Ok(Some(_))
            )
        })
    }
}

/// The guard's sinks: a record names who, what and through which door, never a value or a term.
struct Sinks;

impl TraceSink for Sinks {
    fn offer(&self, record: FieldAccessRecord) -> Offer {
        tracing::info!(
            target: "spot::audit::field_access",
            who = %record.who,
            doctype = %record.doctype,
            field = %record.field,
            via = record.via.as_str(),
            authorised = record.authorised,
            "field access"
        );
        Offer::Accepted
    }
}

impl spot_field_guard::AuditSink for Sinks {
    fn offer(&self, _record: GuardAudit) -> Offer {
        tracing::info!(target: "spot::audit::field_guard", "field guard record");
        Offer::Accepted
    }
}

/// [`ProtectionSource`] over the metadata the site serves now: a metadata swap shows on the next
/// call, and nothing served means "unknown", which the guard and the indexer both treat as
/// "release and index nothing".
pub struct LiveProtection {
    live: Arc<SiteLive>,
}

impl LiveProtection {
    /// A source over `live`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> Self {
        Self { live }
    }
}

impl ProtectionSource for LiveProtection {
    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>> {
        let served = self.live.served()?;
        MetaProtection::new(&served.meta).protection(doctype)
    }
}

/// [`GuardSource`] over the live hub: a guard on the decision core served now.
pub struct LiveGuards {
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
}

impl LiveGuards {
    /// A source over `live`, stamping guard records with `clock`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, clock: Arc<dyn Clock>) -> Self {
        Self { live, clock }
    }
}

impl GuardSource for LiveGuards {
    fn guard(&self) -> Option<Arc<FieldGuard>> {
        let served = self.live.served()?;
        let protection = Arc::new(MetaProtection::new(&served.meta));
        Some(Arc::new(FieldGuard::new(
            Arc::clone(&served.decision),
            protection as Arc<dyn ProtectionSource>,
            Arc::new(Sinks),
            Arc::new(Sinks),
            Arc::clone(&self.clock),
        )))
    }
}

/// Why an index policy could not be built yet.
#[derive(Debug)]
pub enum PolicyError {
    /// The site serves no metadata yet (it is still starting): ask again later.
    NotReady,
    /// The policy text is refused; the error carries only policy keys, never a document value.
    Invalid(RetrievalError),
}

/// Builds the index policy the indexer applies.
pub trait PolicySource: Send + Sync {
    /// The policy, validated against the permission snapshot served now.
    ///
    /// # Errors
    ///
    /// [`PolicyError::NotReady`] before the site serves metadata, [`PolicyError::Invalid`] when
    /// the policy is refused (default deny: nothing is indexed until it is fixed).
    fn policy(&self) -> Result<IndexPolicy, PolicyError>;
}

/// [`PolicySource`] over the live hub and the inline policy text of `[services.retrieval]`.
pub struct LivePolicy {
    live: Arc<SiteLive>,
    text: String,
}

impl LivePolicy {
    /// The policy `text`, validated against `live`'s snapshot.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, text: String) -> Self {
        Self { live, text }
    }
}

impl PolicySource for LivePolicy {
    fn policy(&self) -> Result<IndexPolicy, PolicyError> {
        let served = self.live.served().ok_or(PolicyError::NotReady)?;
        IndexPolicy::load(&self.text, served.decision.snapshot()).map_err(PolicyError::Invalid)
    }
}
