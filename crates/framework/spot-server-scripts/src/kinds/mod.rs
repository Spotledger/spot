//! The other two kinds of Server Script (WP-EXT-06.T6): API scripts and scheduled scripts.
//!
//! - An **API script** is a TypeScript function a consultant exposes as the whitelisted
//!   function `scripting/<module>/<function>`; it runs as the function's caller and is never
//!   callable by a guest ([`ApiScript`], [`ApiScriptHandler`], [`ApiScriptRegistry`]).
//! - A **scheduled script** is a TypeScript function that runs on a cron schedule, as the user
//!   named in its `run_as` field and never as a system principal ([`ScheduledScript`],
//!   [`ScheduledScriptRunner`]).
//!
//! Both live in the optional `scripting` app (`apps/scripting/`), compile on save with the same
//! diagnostics as document-event scripts ([`ApiScriptController`], [`ScheduledScriptController`]),
//! run on the same engine with the same limits, host surface and deterministic clock, and
//! record a [`KindRun`] for every run. Document-event scripts are untouched.

mod api;
mod compile;
mod exec;
mod record;
mod scheduled;
mod store;

pub use api::{ApiScriptHandler, ApiScriptRegistry, KeyConflict, KindDeps};
pub use compile::{ApiScriptController, KindCompileController, ScheduledScriptController};
pub use record::{
    API_SCRIPT_DOCTYPE, ApiScript, ApiVerbs, FIELD_ARGUMENTS_SCHEMA, FIELD_CRON, FIELD_FUNCTION,
    FIELD_MISSED_RUN, FIELD_MODULE, FIELD_QUEUE, FIELD_RUN_AS, FIELD_TIME_ZONE, FIELD_VERBS,
    MissedRun, SCHEDULED_SCRIPT_DOCTYPE, SCRIPTING_APP, ScheduledScript,
};
pub use scheduled::{
    RUN_SCHEDULED_SCRIPT_JOB, ScheduleTarget, ScheduledScriptRunner, UserDirectory, UserRecord,
};
pub use store::{FakeKindStore, KindRun, KindStore};

/// Which of the two new kinds a script is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScriptKind {
    /// An API script: runs as the caller of a whitelisted function.
    Api,
    /// A scheduled script: runs on a cron schedule as its `run_as` user.
    Scheduled,
}

impl ScriptKind {
    /// The DocType the kind's records live in.
    pub fn doctype(self) -> &'static str {
        match self {
            ScriptKind::Api => API_SCRIPT_DOCTYPE,
            ScriptKind::Scheduled => SCHEDULED_SCRIPT_DOCTYPE,
        }
    }

    /// The exported function the engine calls.
    pub fn entry(self) -> &'static str {
        match self {
            ScriptKind::Api => "on_call",
            ScriptKind::Scheduled => "on_run",
        }
    }
}
