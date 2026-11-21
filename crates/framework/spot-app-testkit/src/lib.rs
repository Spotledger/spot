//! The extension SDK's test kit (WP-EXT-07.T2).
//!
//! A partner who has written an app (manifest, DocTypes, permissions, a native controller) writes
//! `cargo test` cases that exercise it the way the server would: real validation, real lifecycle,
//! real permission checks, **no PostgreSQL and no HTTP**. [`AppHarness::load`] runs the app check
//! (every problem at once), loads the DocTypes and permission files into the real document engine
//! over the in-memory backend, and [`AppHarness::as_user`] gives an [`Actor`] that calls the
//! engine as a declared user. The kit adds no permission check and removes none: every operation
//! is the engine's.
//!
//! ```ignore
//! let harness = AppHarness::builder()
//!     .app("apps/sample")
//!     .controllers("sample", spot_sample_app::controllers())
//!     .build()
//!     .await?;
//! let pam_id = harness.user("pam").with_roles(&["Project Manager"]).await?;
//! let pam = harness.as_user("pam")?;
//! let project = pam.create("project", &json!({ "code": "P-1", "title": "First" })).await;
//! assert::assert_ok(&project);
//! ```
//!
//! # Determinism
//!
//! Time is a manual clock ([`AppHarness::set_now`], [`AppHarness::advance`]); record ids count up
//! from one; command ids are numbered. Nothing reads the wall clock or a random source, so a test
//! gives the same result on every run.
//!
//! # What else the kit runs (WP-EXT-07.T3)
//!
//! - **TypeScript**: [`AppHarness::run_script`] runs an exported function of an app's script on
//!   the real engine (no host functions, the harness clock, a zero random seed).
//! - **Lists**: [`Actor::list`] filters and orders the documents the actor can read.
//! - **Functions**: [`Actor::call_function`] dispatches a whitelisted function through the real
//!   registry, with handlers registered by [`HarnessBuilder::function`].
//! - **Fixtures**: [`AppHarness::load_fixtures`] loads the manifest's plain fixtures.
//! - **Permissions**: [`UserDecl::with_permissions`] declares a user with exactly the rights of
//!   a list of [`Grant`]s.
//!
//! # What the kit does not do
//!
//! No HTTP, no PostgreSQL, no WASM components, and no fixture that names an import step. Use the
//! `l3-http` suite against a running host for those.

pub mod actor;
pub mod assertions;
pub mod error;
pub mod functions;
mod grants;
pub mod harness;
mod ids;
mod json;
mod listing;
mod load;
pub mod script;

pub use actor::{Actor, DocResult};
pub use error::{TestkitError, codes};
pub use functions::sync_handler;
pub use grants::Grant;
pub use harness::{ADMINISTRATOR, AppHarness, HarnessBuilder, UserDecl};
pub use json::parse_id;
pub use script::ScriptFailure;

/// The assertions, under the name the docs use: `spot_app_testkit::assert::assert_ok(..)`.
pub mod assert {
    pub use crate::assertions::*;
}
