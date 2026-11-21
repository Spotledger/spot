//! Engine-side resource files: one file per operation group that `EngineResources` does not
//! serve itself.
//!
//! Every `src/resources/<name>.rs` (except this file) is declared by `build.rs`, which writes
//! `resources_registry.rs`; a task adds a group by adding a file, with no edit to a shared list.
//!
//! * `function.rs` and `report.rs` hold the [`FunctionDelegate`] and [`ReportDelegate`] traits.
//! * Any other file is an implementation file its owning task adds (for example
//!   `resources/webhooks.rs`: the engine-backed implementation of that group's `spot-http` port
//!   trait, handed to the router by the group's hosting module). No trait is created for it here.
//!
//! The one shared line left is the [`Delegates`] struct below: a later group that has an
//! `EngineResources` delegate adds its own trait file and one `Option` field here.
//! Installing delegates in `spot serve` is the host's job (WP-PRD-01.T48); until then no
//! delegate is installed and both operations answer as they always have.

use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/resources_registry.rs"));

pub use self::function::FunctionDelegate;
pub use self::report::ReportDelegate;

/// The delegates an `EngineResources` hands operations to. An operation with no delegate
/// answers exactly as it did before delegates existed; it never answers success.
#[derive(Clone, Default)]
pub struct Delegates {
    pub(crate) function: Option<Arc<dyn FunctionDelegate>>,
    pub(crate) report: Option<Arc<dyn ReportDelegate>>,
}

impl Delegates {
    /// No delegate installed.
    pub fn none() -> Self {
        Self::default()
    }

    /// Installs the function delegate.
    #[must_use]
    pub fn with_function(mut self, delegate: Arc<dyn FunctionDelegate>) -> Self {
        self.function = Some(delegate);
        self
    }

    /// Installs the report delegate.
    #[must_use]
    pub fn with_report(mut self, delegate: Arc<dyn ReportDelegate>) -> Self {
        self.report = Some(delegate);
        self
    }
}

impl std::fmt::Debug for Delegates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delegates")
            .field("function", &self.function.is_some())
            .field("report", &self.report.is_some())
            .finish()
    }
}
