//! The report catalogue seam of `GET /api/v1/report` and `GET /api/v1/report/{report}`
//! (`report_list` and `report_describe`, WP-PLT-14.T2): the reports the signed-in user may see,
//! and one report described for that user.
//!
//! `spot-http` does not depend on the report engine. It defines [`ReportCatalogPort`], the two
//! reads in the shapes the router needs, and the host writes the adapter over `spot-reports`.
//! Everything that decides is the port's: who the caller is (from `ctx.credential` on
//! `ctx.site`), which reports the caller's roles admit, which columns the caller may report on.
//! The routes pass **no user and no role**: there is no way to ask for another caller's
//! catalogue through this trait.
//!
//! A report the caller's roles do not admit is not in the list and is answered
//! `report.not_found` by `describe`, exactly as a report that does not exist.
//!
//! `report_run` is not here: it is served by the engine behind `ResourceService::run_report`.

use serde_json::Value as Json;

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, ReportColumn, RequestCtx};

/// The port key of [`ReportCatalogPort`].
pub const KEY: PortKey = PortKey::of::<dyn ReportCatalogPort>();

/// What a port call answers.
pub type CatalogResult<T> = Result<T, ApiError>;

/// One report in the list (the contract's `ReportSummary`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportSummaryOut {
    /// The report's storage name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The app that ships it.
    pub app: String,
    /// The base DocType.
    pub base_doctype: String,
}

/// Where a `select` or `link` parameter's options come from (the contract's `options`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParameterOptionsOut {
    /// A fixed list (`source: values`).
    Values(Vec<String>),
    /// The candidates of a DocType (`source: link`).
    Link(String),
}

/// One declared parameter (the contract's `ReportParameter`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReportParameterOut {
    /// The name a run keys its `filters` by.
    pub name: String,
    /// Human label.
    pub label: String,
    /// `text`, `int`, `decimal`, `date`, `link` or `select`.
    pub kind: String,
    /// Whether a run must supply it.
    pub required: bool,
    /// The default in the contract's value encoding; `None` when there is none.
    pub default: Option<Json>,
    /// Present for `select` and `link` parameters only.
    pub options: Option<ParameterOptionsOut>,
}

/// A report described for one caller (the contract's `ReportDescription`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReportDescriptionOut {
    /// The report's storage name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The app that ships it.
    pub app: String,
    /// The base DocType.
    pub base_doctype: String,
    /// The declared parameters.
    pub parameters: Vec<ReportParameterOut>,
    /// The columns this caller may report on, in result order. Never a column removed for the
    /// caller.
    pub columns: Vec<ReportColumn>,
    /// The largest `limit` a run honours.
    pub max_rows: u32,
}

/// The report catalogue of one site, for the signed-in caller.
pub trait ReportCatalogPort: Send + Sync {
    /// The reports whose roles admit the caller, ordered by label.
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, CatalogResult<Vec<ReportSummaryOut>>>;

    /// One report described for the caller. A report that does not exist, and one the caller's
    /// roles do not admit, answer the same `report.not_found`.
    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
    ) -> BoxFuture<'a, CatalogResult<ReportDescriptionOut>>;
}
