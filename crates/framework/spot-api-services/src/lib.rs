//! Adapters between the HTTP service traits and the document engine (WP-PLT-05.T2).
//!
//! This crate connects the `/api/v1` service traits (`spot-http`) to the real document
//! engine (`spot-document` implementing `spot-app-api::context::DocumentService`), the
//! effective metadata (`spot-meta::EffectiveMeta`) and the permission decision core
//! (`spot-permissions`).
//!
//! - [`EngineResources<S>`] implements `spot-http`'s `ResourceService` by adapting
//!   `RequestCtx` → `Invocation`, `ApiDocument` ↔ `Document`, `DocumentPatchApi` →
//!   `DocumentPatch`, and mapping every operation through the engine.
//! - [`MetaResources`] implements `MetaService` from `spot-meta`'s effective metadata, read
//!   per request from a [`MetaSource`] so a host can swap metadata while it runs.
//! - [`AccessControl`] is the permission view both WP-UI-01 adapters decide with.
//! - [`EngineFormEvents`] serves `POST /api/v1/resource/{doctype}/form-event` over the
//!   engine's `field_changed` (WP-UI-01.T1).
//! - [`PermissionResources`] serves `GET /api/v1/auth/permissions` from the decision core
//!   (WP-UI-01.T1).

pub mod access;
pub mod engine_resources;
pub mod error;
pub mod form_event;
pub mod meta_resources;
pub mod permissions;
pub mod query;
pub mod resources;
pub mod util;

pub use access::{AccessControl, PrincipalSource, engine_for};
pub use conversion::to_api_document;
pub use conversion::{to_document, to_patch, to_write_result};
pub use engine_resources::EngineResources;
pub use error::ApiErrorExt;
pub use form_event::{
    EngineFormEvents, EveryField, FieldChangedPlan, FormEvent, FormEventService, Limits,
};
pub use meta_resources::{CurrentMeta, MetaResources, MetaSource};
pub use permissions::PermissionResources;
pub use resources::{Delegates, FunctionDelegate, ReportDelegate};
pub use util::build_invocation;

mod conversion;
