#![forbid(unsafe_code)]
//! `spot-devserver` — the local development server with hot reload (WP-DX-02.T1).
//!
//! The acceptance sentence of the work package is the design: **code and metadata changes are
//! visible without restart**, on Fedora and Windows. The runtime already owns the reload
//! mechanisms (`spot-meta-live` for live metadata, `spot-ts-transpile` for scripts, `spot-views`
//! for views, `spot-wasm` for components); this crate wires file changes to them for the
//! developer.
//!
//! The pieces, in the order the server drives them:
//!
//! 1. [`watch`] runs a [`Watcher`](watch::Watcher) over an
//!    [`EventSource`](watch::EventSource). The platform source is `notify` (inotify on Fedora,
//!    `ReadDirectoryChangesW` on Windows); a `VecEventSource` is the fake the tests drive. The
//!    watcher debounces with an injected monotonic time and normalises the events across
//!    platforms: path case is folded, an editor's rename-as-write and atomic temp files are
//!    handled, and `target/`, `node_modules/` and `.git/` are ignored.
//! 2. [`classify`] turns an event into a [`Change`](classify::Change) in one of the classes the
//!    pipeline understands.
//! 3. [`reload`] runs the matching [`Step`](reload::Step): metadata is rebuilt and swapped with
//!    `spot-meta-live` (a failed build leaves the old version serving), views are parsed and
//!    validated with `spot-views`, scripts are transpiled and checked against the generated
//!    `.d.ts` with `spot-ts-transpile`/`spot-dts`, and rebuilt components are validated and
//!    hashed through `spot-wasm`.
//! 4. [`publish`] announces `dev.reloaded` on the `spot-realtime` bus, and [`status`] serves the
//!    last reloads and errors on a small HTTP endpoint.
//! 5. [`site`] owns the scratch development tenant — create, seed, reset and snapshot/restore —
//!    behind the `SiteControl` seam so the host can delegate to `spot-cli` and
//!    `spot-pg-supervisor` without a dependency cycle.
//! 6. [`server`] ties the watcher and the pipeline together; [`cli`] is the `spot dev` command
//!    registration and the `spot-devserver` binary.
//!
//! # Time and paths
//!
//! Time is always an argument: [`Watcher::poll`](watch::Watcher::poll) takes the current
//! monotonic `Duration`, and [`Reloader::reload`](reload::Reloader::reload) takes a
//! [`Clock`](spot_core::Clock). Nothing here reads the wall clock or sleeps. Paths are UTF-8
//! (`camino`) and both separators are accepted; nothing is Windows-only or Linux-only.

pub mod classify;
pub mod cli;
pub mod error;
pub mod paths;
pub mod publish;
pub mod reload;
pub mod server;
pub mod site;
pub mod status;
pub mod watch;

pub use classify::{Change, ChangeKind, Classifier};
pub use error::{DevError, DevResult};
pub use publish::{RealtimeNotifier, ReloadNotifier, record_payload};
pub use reload::{
    BuildCommand, BuildOutput, BuildRunner, ComponentHost, ComponentStep, DeclarationNameChecker,
    DtsProvider, InMemoryComponentHost, InMemoryScriptHost, LiveDefinitions, LiveDtsProvider,
    MetaStep, ReloadContext, ReloadRecord, ReloadSummary, Reloader, ScriptHost, ScriptStep, Step,
    StepName, StepOutcome, TypeChecker, ViewStep, WasmHostComponentHost,
};
pub use server::DevServer;
pub use site::{DevLayout, DevSite, ScratchSiteControl, SiteControl, SiteOp};
pub use status::{HttpResponse, StatusServer, StatusSnapshot, StatusStore};
pub use watch::{
    EventSource, NotifyEventSource, Op, RawEvent, VecEventSource, Watcher, WatcherConfig,
};
