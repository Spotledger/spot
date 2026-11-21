//! The adapters that give `spot_api_services::search::SearchService` its two seams on a running
//! site: [`GuardAccess`] over the field guard (channel `Search`) and [`PgProjection`] over the
//! backend's search capability, the start-up state and the projection lag.
//!
//! Nothing here decides a right. The field guard asks the permission engine; this file only
//! chooses the channel and turns the guard's answer into the one `permission.field_denied` the
//! contract names. A name the guard cannot resolve to a field is refused the same way, so a
//! protected field and an unknown one are indistinguishable.

use std::sync::{Arc, RwLock};

use spot_api_services::search::{ProjectionStatus, SearchAccess, SearchProjection};
use spot_app_api::context::{Actor, Invocation};
use spot_core::Clock;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{
    AuditRecord as GuardAudit, Channel, FieldAccessRecord, FieldGuard, MetaProtection, Offer,
    TraceSink,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_storage_contract::{Isolation, Name, StorageBackend, TxOptions};

use crate::live::SiteLive;

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

/// [`SearchAccess`] over the site's field guard.
pub struct GuardAccess {
    /// The site's live handles (served metadata, decision core, principals).
    pub live: Arc<SiteLive>,
    /// The clock the guard stamps records with.
    pub clock: Arc<dyn Clock>,
}

impl GuardAccess {
    async fn principal(&self, inv: &Invocation) -> Option<Principal> {
        let Actor::User { id, .. } = &inv.actor else {
            return None;
        };
        let principals = self.live.principals()?;
        Some(DocPrincipals::resolve(&*principals, id).await)
    }

    fn guard(&self) -> Option<FieldGuard> {
        let served = self.live.served()?;
        let protection = Arc::new(MetaProtection::new(&served.meta));
        Some(FieldGuard::new(
            Arc::clone(&served.decision),
            protection as Arc<dyn spot_field_guard::ProtectionSource>,
            Arc::new(Sinks),
            Arc::new(Sinks),
            Arc::clone(&self.clock),
        ))
    }

    /// The paths the guard lets the principal use on the search channel, from `fields`.
    fn usable_of(
        guard: &FieldGuard,
        principal: &Principal,
        doctype: &str,
        fields: &[String],
    ) -> Vec<String> {
        // `columns` refuses (an error) when any requested path is hidden on a channel that does
        // not degrade to masked, so ask one path at a time and keep those that pass.
        fields
            .iter()
            .filter(|field| {
                guard
                    .columns(
                        Channel::Search,
                        principal,
                        doctype,
                        std::slice::from_ref(field),
                    )
                    .is_ok_and(|c| c.refused.is_empty() && c.masked.is_empty())
            })
            .cloned()
            .collect()
    }
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionFieldDenied)
}

impl SearchAccess for GuardAccess {
    fn check<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            // No guard, no principal, no answer: fail closed.
            let (Some(guard), Some(principal)) = (self.guard(), self.principal(inv).await) else {
                return Err(denied());
            };
            if Self::usable_of(&guard, &principal, doctype, fields).len() == fields.len() {
                Ok(())
            } else {
                Err(denied())
            }
        })
    }

    fn usable<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let (Some(guard), Some(principal)) = (self.guard(), self.principal(inv).await) else {
                return Vec::new();
            };
            Self::usable_of(&guard, &principal, doctype, fields)
        })
    }
}

/// What the start-up check found: `None` is healthy, `Some(text)` the degraded reason.
#[derive(Clone, Default)]
pub struct SearchState(Arc<RwLock<Option<String>>>);

impl SearchState {
    /// Records the start-up outcome.
    pub fn set(&self, problem: Option<String>) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = problem;
    }

    /// The degraded reason, if any.
    pub fn problem(&self) -> Option<String> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// [`SearchProjection`] over the backend.
pub struct PgProjection {
    /// The site's backend.
    pub backend: PgBackend,
    /// The start-up state; a degraded service serves no search.
    pub state: SearchState,
    /// The lag bound in milliseconds.
    pub bound_ms: u64,
}

impl SearchProjection for PgProjection {
    fn status<'a>(
        &'a self,
        _inv: &'a Invocation,
        doctype: &'a str,
    ) -> BoxFuture<'a, ProjectionStatus> {
        Box::pin(async move {
            let available = self.state.problem().is_none() && self.backend.capabilities().search;
            if !available {
                return ProjectionStatus {
                    available: false,
                    lag: None,
                    bound: None,
                };
            }
            ProjectionStatus {
                available: true,
                lag: self.lag(doctype).await,
                bound: Some(self.bound_ms),
            }
        })
    }
}

impl PgProjection {
    /// The projection's age in milliseconds, when it can be measured (an empty projection has
    /// none). A measuring failure is reported as no lag, never as an invented figure.
    async fn lag(&self, doctype: &str) -> Option<u64> {
        let name = Name::new(doctype).ok()?;
        let options = TxOptions::new(Name::new("search_lag").ok()?, Isolation::ReadCommitted);
        let catalog = Arc::new(
            spot_storage_contract::Catalog::new(
                0,
                spot_storage_contract::Collation::new("und").ok()?,
                Vec::new(),
            )
            .ok()?,
        );
        let mut tx = self.backend.begin(catalog, options).await.ok()?;
        let lag = spot_pg_search::measure_doctype(&mut tx, &name)
            .await
            .ok();
        let _ = spot_storage_contract::Transaction::rollback(tx).await;
        lag.map(|l| l.value())
    }
}
