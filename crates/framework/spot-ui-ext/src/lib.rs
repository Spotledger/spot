//! Tier A UI extensions (WP-UI-13.T2; ADR-069 amendment A1).
//!
//! An app extends the client by **declaring data**: actions, panels, tabs, tiles and composite
//! components, written as one `ui` document (`contracts/ui-ext/`). This crate is the server half:
//! it parses what the apps declare, validates every reference against what their manifests,
//! metadata and the destination registry provide, and merges the apps of a site into one
//! canonical, hashable description that the client renders.
//!
//! [`parse_document`] reads JSON or TOML into a closed [`UiDocument`]; [`validate`] reports every
//! problem of one app at once, with the stable `uiext.*` code of `contracts/ui-ext/errors.md` and
//! expressions parsed by [`spot_expr`] and resolved against the item's DocType or props;
//! [`resolve`] merges the apps of a site in dependency order into a hashable [`Resolved`], and
//! [`Resolved::for_permissions`] hides what a permission set may not see — for display only, never
//! authorisation (ADR-069 §7). The crate is a pure library: no storage, no HTTP, no clock, no
//! floats, no `unsafe`.
//!
//! ```
//! use spot_ui_ext::{AppContribution, AppFacts, validate};
//!
//! let document = r#"{
//!     "contract_version": "0.1.0",
//!     "requires_shell": "0.1.0",
//!     "app": "sample"
//! }"#;
//! let contribution = AppContribution::new("sample", "1.0.0")
//!     .with_document("ui.json", document)
//!     .with_facts(AppFacts::default());
//! assert!(validate(&contribution).is_empty());
//! ```

mod code;
mod components;
mod composite;
mod expr;
mod facts;
mod limits;
mod markdown;
mod model;
mod panels;
mod parse;
mod resolve;
mod schema;
mod validate;
mod view;

pub use code::{UiExtError, codes};
pub use facts::{
    AllowAll, AppContribution, AppFacts, DoctypeFacts, InstalledApp, MethodFacts, PermissionList,
    PermissionSet, QueryFacts, QueryKind, SiteContext,
};
pub use limits::{
    CONTRACT_VERSION, ICON_TOKENS, ItemKind, MAX_COMPONENT_DEPTH, MAX_COMPONENT_NODES, MAX_REPEAT,
    SLOTS, SlotSpec, id_app, is_app_name, is_icon_token, is_id, is_name, is_origin,
    is_permission_ref, slot,
};
pub use model::*;
pub use parse::{Format, MAX_DOCUMENT_BYTES, parse_document, parse_document_as};
pub use resolve::{DroppedItem, Resolved, ResolvedApp, ResolvedItem, ResolvedSlot, resolve};
pub use validate::validate;
pub use view::field_list_items;
