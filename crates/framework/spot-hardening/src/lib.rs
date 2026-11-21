//! spot-hardening — the hardening baseline (WP-ADM-10.T1, blueprint WS22, ADR-44).
//!
//! A fresh server must reach a known, safe state automatically, and every deviation must be
//! reported. This crate is the declarative half of that: a **baseline** of controls per OS
//! family, deterministic **checks** over the facts the host agent reports, **remediation**
//! through the typed operation catalogue, and a structured **compliance report** with a diff.
//!
//! # Shape
//!
//! - [`Baseline`] as data: the compiled-in [`LINUX_BASELINE`] and [`WINDOWS_BASELINE`] are
//!   TOML (`baselines/linux.toml`, `baselines/windows.toml`), each a list of [`Control`]s.
//!   A control has an id, a title, an OS family, a [`CheckSpec`], an optional remedy
//!   ([`OpRef`] into `spot-mgmt-ops`), a [`Severity`], and the `reversible` and `lockout_risk`
//!   flags the planner reads.
//! - [`evaluate`] turns a baseline and a [`FactSource`] into a [`Report`]: one [`Outcome`]
//!   (`Pass`, `Fail`, `NotApplicable`, `Error`) per control, in baseline order, plus a
//!   [`Score`] computed with integer arithmetic. Every fail names what was found and what was
//!   expected.
//! - [`remediation_plan`] returns the operations the policy permits now, and
//!   [`suggested_plan`] the ones that must be approved first. Only a failing, remediable,
//!   reversible control is a candidate; an SSH or firewall change needs the policy's
//!   permission and a connectivity guard.
//! - [`Report::to_json`] and [`Report::render_text`] are the structured and plain-text forms;
//!   [`diff`] compares two reports by control id.
//! - [`FakeServer`] is a test double: facts plus modelled remediation effects, so the whole
//!   loop runs without a host.
//!
//! # Purity
//!
//! No clock is read (an evaluation is a function of its inputs), no float is used (the score
//! is integer basis points), no network or filesystem is touched, and there is no `unsafe`
//! (the project coding rules). The baseline is the only compiled-in data.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod baseline;
pub mod check;
pub mod error;
pub mod evaluate;
pub mod facts;
pub mod fake;
pub mod model;
pub mod remediate;
pub mod report;

pub use baseline::{Baseline, LINUX_BASELINE, WINDOWS_BASELINE};
pub use check::check_outcome;
pub use error::HardeningError;
pub use evaluate::evaluate;
pub use facts::{FactSource, Facts};
pub use fake::FakeServer;
pub use model::{
    CheckSpec, Control, ControlId, FactKey, FactValue, OpRef, OsFamily, Policy, PolicyMode,
    Severity,
};
pub use remediate::{remediation_plan, suggested_plan};
pub use report::{
    ControlChange, ControlResult, Direction, Outcome, Report, ReportDiff, Score, Status, diff,
};
// The types a remedy's fields are made of, so a caller can build an [`OpRef`] without naming
// `spot-mgmt-ops` directly.
pub use spot_mgmt_ops::{OpId, ParamValue};

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-hardening";
