//! `GET /api/v1/auth/permissions` over the permission core (WP-UI-01.T1).
//!
//! The client-safe projection of the decision engine (ADR-070 §12): for the signed-in user and
//! each DocType, the eight document-independent rights and the field summary. The HTTP layer
//! carries what this service returns and computes nothing; `304` on `If-None-Match` is the
//! route's job, over the `etag` this service's version produces.
//!
//! Document-dependent grants are summarised optimistically (see [`AccessControl`]); a named
//! DocType that does not exist is `meta.unknown_doctype`, and a known one the user holds no
//! right on answers an empty rights set rather than `403`. The answer names no protection
//! group and no other user.

use spot_http::api::permissions::PermissionSummary;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, PermissionService, RequestCtx};
use spot_storage_contract::Name;

use crate::access::AccessControl;

/// The most DocTypes one request may ask about, from the contract's `maxItems`.
pub const MAX_DOCTYPES: usize = 100;

/// Implements [`PermissionService`] over the decision core.
pub struct PermissionResources {
    access: std::sync::Arc<AccessControl>,
}

impl PermissionResources {
    /// A summary service over one permission view.
    pub fn new(access: std::sync::Arc<AccessControl>) -> Self {
        Self { access }
    }
}

impl PermissionService for PermissionResources {
    fn summary<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctypes: &'a [Name],
    ) -> BoxFuture<'a, Result<PermissionSummary, ApiError>> {
        Box::pin(async move {
            if doctypes.len() > MAX_DOCTYPES {
                return Err(ApiError::new(
                    Code::ApiInvalidRequest,
                    format!("At most {MAX_DOCTYPES} DocTypes can be asked about at once."),
                ));
            }
            let inv = self.access.invocation(ctx)?;
            let principal = self.access.principal(&inv).await;
            let mut entries = Vec::new();
            if doctypes.is_empty() {
                // An empty request means "everything readable", bounded like an explicit one.
                for def in self.access.meta().doctypes() {
                    if entries.len() >= MAX_DOCTYPES {
                        break;
                    }
                    if self.access.has_any_right(&principal, def) {
                        entries.push(self.access.doctype_permissions(&principal, def));
                    }
                }
            } else {
                for doctype in doctypes {
                    let def = self.access.doctype(doctype.as_str())?;
                    entries.push(self.access.doctype_permissions(&principal, &def));
                }
            }
            Ok(PermissionSummary {
                doctypes: entries,
                version: self.access.version(),
            })
        })
    }
}
