//! [`Seams`] over the site's live hub: the document engine, the permission decision core, the
//! protection facts and the principal directory of the runtime serving now, read on every call so
//! a metadata swap, a revoked right or a rebuilt runtime shows on the very next read.
//!
//! The guard and the protection facts come from the retrieval service's public live adapters
//! ([`LiveGuards`], [`LiveProtection`]), so retrieval and prompts agree on what is protected. The
//! load goes to the engine directly rather than through `ports::LiveLoader`, because that loader
//! reports a permission denial and an outage the same way, and this reader must tell them apart.

use std::sync::Arc;

use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::error::ErrorClass;
use spot_app_api::prelude::RecordId;
use spot_core::{Clock, SiteId};
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_field_guard::{DocProtection, FieldGuard, ProtectionSource};
use spot_permissions::Principal;
use time::OffsetDateTime;

use super::reader::{Fut, Loaded, Seams};
use crate::live::SiteLive;
use crate::services::retrieval::access::{LiveGuards, LiveProtection};
use crate::services::retrieval::ports::GuardSource;

/// [`Seams`] over one site's [`SiteLive`].
pub struct LiveSeams {
    live: Arc<SiteLive>,
    site: SiteId,
    clock: Arc<dyn Clock>,
}

impl LiveSeams {
    /// Seams over `live` for `site`, with `clock` for the invocation time and guard records.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, site: SiteId, clock: Arc<dyn Clock>) -> Self {
        Self { live, site, clock }
    }
}

impl Seams for LiveSeams {
    fn site(&self) -> SiteId {
        self.site.clone()
    }

    fn now(&self) -> OffsetDateTime {
        self.clock.now_utc()
    }

    fn guard(&self) -> Option<Arc<FieldGuard>> {
        LiveGuards::new(Arc::clone(&self.live), Arc::clone(&self.clock)).guard()
    }

    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>> {
        LiveProtection::new(Arc::clone(&self.live)).protection(doctype)
    }

    fn principal(&self, id: RecordId) -> Fut<'_, Option<Principal>> {
        Box::pin(async move {
            let principals = self.live.principals()?;
            Some(DocPrincipals::resolve(&*principals, &id).await)
        })
    }

    fn load<'a>(&'a self, inv: &'a Invocation, doctype: &'a str, id: RecordId) -> Fut<'a, Loaded> {
        Box::pin(async move {
            let Some(engine) = self.live.engine() else {
                return Loaded::Unavailable;
            };
            match engine.load(inv, doctype, id).await {
                Ok(outcome) => Loaded::Document(outcome.value),
                // The engine's own classes for "not a document for this user".
                Err(error)
                    if matches!(
                        error.class,
                        ErrorClass::NotFound
                            | ErrorClass::PermissionDenied
                            | ErrorClass::UnknownDocType
                    ) =>
                {
                    Loaded::Absent
                }
                Err(_) => Loaded::Unavailable,
            }
        })
    }
}
