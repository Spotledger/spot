//! `spot-nocode-builders` — the server-side model of the no-code report and dashboard
//! builder (WP-DX-07.T1).
//!
//! A business user builds a report, a dashboard and a simple approval workflow without
//! writing code. Everything a builder does is **data**: a draft over the platform's own
//! metadata and the storage query IR, never a hand-written query and never a script.
//!
//! - [`report`]: a [`ReportDraft`](report::ReportDraft) — a base DocType, columns (including
//!   fields reached through a link), filters with typed parameters, grouping with aggregates,
//!   sorting, footer totals and a chart — changed one pure [`ReportOp`](report::ReportOp) at a
//!   time, with [`ReportEditor`](report::ReportEditor) keeping the command log so undo and
//!   redo are exact.
//! - [`compile`]: the draft compiled to a `spot-reports` definition and the storage query IR,
//!   under ADR-069's limits, with every refusal stated in plain language. The friendly codes
//!   live in [`codes`]; the underlying `report.*` code is kept beside each issue.
//! - [`fields`]: the palette a builder may offer — only fields the author may read **and
//!   report on**, and only links into record types the author may report on. A published
//!   report is still checked for each viewer by the report engine, so no viewer sees more
//!   than its own rights.
//! - [`dashboard`]: a [`DashboardDraft`](dashboard::DashboardDraft) with number, chart, table
//!   and list tiles from published reports, a layout grid, filters shared across tiles and a
//!   refresh policy, compiled to the [`DashboardDef`](dashboard::DashboardDef) this crate
//!   defines as the platform contract draft (no dashboard definition existed when it was
//!   written).
//!
//!   Like the report draft, a dashboard draft changes one pure [`DashboardOp`](dashboard_edit::DashboardOp)
//!   at a time, with [`DashboardEditor`](dashboard_edit::DashboardEditor) keeping the command log so
//!   undo and redo are exact.
//!
//!   The dashboard op log has eight ops, named as the Avalonia dashboard builder writes them
//!   (`add_tile`, `remove_tile`, `move_tile`, `set_tile`, `add_filter`, `remove_filter`,
//!   `set_refresh`, `set_roles`), one JSON object each tagged by `"op"`; the files under
//!   `tests/golden/dashboard_ops/` are the wire contract and the tests load every one.
//!   [`apply_dashboard_op`] is pure and a refused op changes nothing (overlap or off-grid
//!   places are `bad_layout`, a duplicate tile or filter is `duplicate`, an unknown tile or
//!   filter is refused, a zero refresh interval is `bad_refresh`, a blank tile title or role
//!   name is `bad_value`). Replaying the log on the initial draft reproduces the draft, undo
//!   then redo restores it, and a new op after undo drops the redo future.
//! - [`approval`]: an [`ApprovalTemplate`](approval::ApprovalTemplate) (record type, approvers,
//!   SLA, escalation, what approval and rejection do) built into a `spot-bpm-model` process
//!   from the *approval with escalation* template, validated by the BPM validator and produced
//!   as a **draft** process version.
//! - [`preview`]: a bounded preview (a row cap and a time cap the host measures) and the
//!   sample rows a builder canvas shows, without running anything unrestricted.
//! - [`publish`]: publish through [`ReportSink`](publish::ReportSink) and
//!   [`Designer`](publish::Designer) — the `designer.publish` permission, provenance recorded
//!   for bundling, and the published artifact handed to the report engine's registry.
//!
//! # What this crate does not do
//!
//! It never builds SQL (ADR-45), never reads the wall clock (time is an argument), never uses
//! floats for money or quantities (ADR-12), never applies a schema change, contains no
//! `unsafe`, and never logs or returns a secret. Every code it raises is catalogued in
//! `crates/framework/spot-errors/catalogue/nocode_builders.toml`.

#![forbid(unsafe_code)]

pub mod approval;
pub mod chart;
pub mod codes;
pub mod compile;
pub mod dashboard;
pub mod dashboard_edit;
pub mod error;
pub mod fields;
pub mod preview;
pub mod publish;
pub mod report;
mod report_edit;
pub mod validate;

pub use approval::{ApprovalAction, ApprovalPlan, ApprovalTemplate, Approver, StartOn, build};
pub use chart::{ChartKind, ChartSeries, ChartSpec};
pub use codes as error_codes;
pub use compile::{ReportPlan, base_alias, compile, plan, plan_for, to_definition};
pub use dashboard::{
    DASHBOARD_SCHEMA_VERSION, DashboardDef, DashboardDraft, Grid, Place, RefreshPolicy,
    SharedFilter, Tile, TileDef, TileKind, TileSource, compile_dashboard,
};
pub use dashboard_edit::apply_op as apply_dashboard_op;
pub use dashboard_edit::{DashboardEditor, DashboardOp};
pub use error::{Issue, OpError, Severity, is_blocked};
pub use fields::{FieldOffer, FieldOffers, JoinOffer, OfferKind, offers};
pub use preview::{PreviewBudget, PreviewRun, check_time, preview, sample_rows};
pub use publish::{
    DashboardRelease, DashboardSink, Designer, MemorySink, PUBLISH_PERMISSION, ProcessRelease,
    ProcessSink, Provenance, ReportRelease, ReportSink, publish_dashboard, publish_process,
    publish_report,
};
pub use report::{
    ABSOLUTE_MAX_ROWS, Aggregate, Compare, DEFAULT_LIMIT, DEFAULT_MAX_ROWS, Direction, DraftColumn,
    DraftFilter, DraftJoin, DraftParamType, DraftParameter, DraftSort, DraftStatus, FilterTarget,
    FilterValue, Nulls, REPORT_MAX_COLUMNS, REPORT_MAX_JOINS, ReportDraft, ReportEditor, ReportOp,
    apply_op, is_exact_decimal, is_name,
};
pub use validate::{dashboard_issues, report_issues};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-nocode-builders";
