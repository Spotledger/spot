//! The app manifest (ADR-17): one `spot-app.toml` per app — the typed model, the TOML
//! parser and the shared checks.
//!
//! Every manifest becomes an `AppManifest`. Native Rust apps build one in code and call
//! `validate`; every other artifact ships TOML, and its host calls `parse`, which reports
//! all problems at once as `ManifestError`s with stable `manifest.*` codes, a message for
//! the app author, and a line and column when the manifest came from a file.

mod cron;
mod error;
mod event;
mod model;
mod parse;
mod validate;

pub use crate::cron::Cron;
pub use crate::error::ManifestError;
pub use crate::event::{SESSION_EVENT_NAMES, parse_hook_point};
pub use crate::model::{
    AppDependency, AppManifest, AppMeta, AppPermissionDecl, AppPermissionName, AssetSub, BootSub,
    Capabilities, DocTypeAccess, EVENT_BUDGET_DEFAULT_MS, EVENT_BUDGET_LIMIT_MS, Endpoint,
    EventSub, FileAccess, FilePath, FixtureSub, FunctionSub, HttpVerb, MAX_PERMISSION_NAME_BYTES,
    MAX_PERMISSION_TITLE_CHARS, MethodSub, OverrideDecl, PatchIntent, PatchSub, PermissionSub,
    Queue, RoutePath, RouteSub, SCHEDULE_BUDGET_DEFAULT_MS, SCHEDULE_BUDGET_LIMIT_MS, ScheduleSub,
    SeamDecl, SeamKind, SessionEvent, SessionSub,
};
pub use crate::parse::parse;
pub use crate::validate::validate;
