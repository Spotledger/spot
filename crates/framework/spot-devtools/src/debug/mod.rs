//! The Debug Adapter Protocol server for TypeScript scripts, and the line-hook shim it runs on.
//!
//! `spot-ts` runs one QuickJS runtime per execution and exposes no debug hooks, so the adapter
//! injects line/frame hooks into the transpiled JavaScript ([`instrument`]) and blocks the
//! script's own execution thread inside them. Breakpoints are set in TypeScript and mapped to the
//! generated lines through the transpiler's source map ([`sourcemap`]).
//!
//! The session is safe by construction: [`crate::safety::DebugPolicy`] refuses a production site,
//! a release build, a wrong token and a non-loopback bind; a paused script blocks only its own
//! thread; and a pause nobody continues is resumed after the policy's pause timeout with a warning.

pub mod instrument;
pub mod protocol;
pub mod runtime;
pub mod server;
pub mod sourcemap;
pub mod variables;
pub mod wire;

pub use instrument::{HOOK_ENTER, HOOK_EXIT, HOOK_LINE, Instrumented, instrument};
pub use runtime::{
    BreakpointResult, DebugCommand, DebugEvent, DebugHost, EventSink, Frame, LaunchRequest,
    NoopSink, PausedSnapshot, ScriptDebugger, SessionActor, SessionPhase, StopReason,
};
pub use server::{DebugServer, DebugServerConfig};
pub use sourcemap::{OriginalPosition, Remap};
pub use variables::{Variable, VariableStore};
