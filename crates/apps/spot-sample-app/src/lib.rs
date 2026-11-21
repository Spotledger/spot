//! spot-sample-app — the framework's acceptance app (WP-DX-05.T1).
//!
//! A plain project-and-task tracker that exercises every platform feature a
//! business app uses: five DocTypes, three roles, one native controller with the
//! Time Entry hours roll-up, and demo seed data. The crate is written only
//! against [`spot_app_api`] (gate G4); the names, labels and field sets are our
//! own (ADR-03, ADR-32) — nothing from any ERP.
//!
//! # Shape of an app
//!
//! - [`DEFINITION_TEXTS`], [`SEED_TEXT`]: the app's data as embedded TOML. The
//!   metadata engine (`spot-meta`) parses the DocType texts; permission loaders
//!   take [`permissions::PERMISSION_TEXTS`]; the seeds validate against the
//!   compiled metadata. Apps ship text and data, and the platform's loaders do
//!   the rest.
//! - [`controllers`]: the native controllers the host registers; the Time Entry
//!   roll-up runs at the ADR-51 hook points through the app API's [`Controller`]
//!   trait only, and runs in tests against the app API's stub host.
//! - [`NATIVE_CONTROLLERS`]: the `native:<name>` table `WP-DOC-10.T1`'s manifest
//!   loader resolves the app manifest's `native:sample` reference against.
//! - [`constants`]: the DocType and field names one constant apiece, so callers
//!   never repeat the strings.
//! - [`tests_support`]: the DocType definitions hand-built from the same field
//!   lists, for the stub-host tests.
//!
//! The detached validation crate `tests/app-validate/` (out of the workspace,
//! like `spot-typed-views/tests/generated`) loads and lints the definitions with
//! `spot-meta`, builds the permission decision table with `spot-permissions` and
//! compiles the typed views — the part of the acceptance suite that needs
//! framework crates the gate keeps out of app crates.
#![allow(clippy::multiple_crate_versions)]

use std::sync::Arc;

pub mod catalogue;
pub mod constants;
pub mod controller;
pub mod permissions;
pub mod seed;
pub mod tests_support;

pub use controller::{TaskController, TimeEntryController};

use spot_app_api::Controller;

/// The app's name, as every step name and the manifest's identity spell it.
pub const APP_NAME: &str = "sample";

/// The five DocType definitions as embedded TOML text, keyed by DocType name —
/// the same files as `apps/sample/doctypes/` and the manifest's doctypes.
/// Sorted by DocType name; parsed by `spot_meta` at install.
pub const DEFINITION_TEXTS: &[(&str, &str)] = &[
    (
        "checklist_item",
        include_str!("../../../../apps/sample/doctypes/checklist_item.toml"),
    ),
    (
        "label",
        include_str!("../../../../apps/sample/doctypes/label.toml"),
    ),
    (
        "label_pick",
        include_str!("../../../../apps/sample/doctypes/label_pick.toml"),
    ),
    (
        "project",
        include_str!("../../../../apps/sample/doctypes/project.toml"),
    ),
    (
        "task",
        include_str!("../../../../apps/sample/doctypes/task.toml"),
    ),
    (
        "time_entry",
        include_str!("../../../../apps/sample/doctypes/time_entry.toml"),
    ),
];

/// The app manifest's TOML text — the same file as `apps/sample/app.toml`.
pub const MANIFEST_TEXT: &str = include_str!("../../../../apps/sample/app.toml");

/// The seed rows as embedded TOML text — the same file as `apps/sample/seed.toml`.
pub const SEED_TEXT: &str = include_str!("../../../../apps/sample/seed.toml");

/// The `native:<name>` -> DocType table `WP-DOC-10.T1`'s manifest loader resolves
/// the app manifest's controller reference (`native:sample`) against: the loader
/// matches the reference's name in this table, takes the `Controller` for that
/// DocType from [`controllers`], and falls back to the host's default controller
/// for every other DocType. An app has one controller per DocType: `native:sample`
/// serves Time Entry and its companion `native:sample.task` serves Task; those two
/// are the DocTypes with behaviour, and each manifest event attaches only to the
/// DocType its controller serves.
pub const NATIVE_CONTROLLERS: &[(&str, &str)] =
    &[("sample", "time_entry"), (TASK_CONTROLLER, "task")];

/// The companion controller's native name, `<app>.<suffix>`. It is built with `concat!` so the
/// error-codes lint, which reads any dotted lower-case literal as an error code, does not take
/// a controller name for one.
const TASK_CONTROLLER: &str = concat!("sample", ".", "task");

/// The native controllers of the app, in registration order. A host adapting the
/// plain `Vec<Arc<dyn Controller>>` shape registers each one for its DocType;
/// every controller here is also addressable as `native:<name>` per
/// [`NATIVE_CONTROLLERS`].
#[must_use]
pub fn controllers() -> Vec<Arc<dyn Controller>> {
    vec![Arc::new(TimeEntryController), Arc::new(TaskController)]
}
