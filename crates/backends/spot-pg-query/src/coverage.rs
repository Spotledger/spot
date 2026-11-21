//! The index-coverage check over query plans (ADR-045, ADR-087 section 13 task 9).
//!
//! [`CompiledQuery::path`](crate::CompiledQuery::path) is passed through by the compiler; this
//! module is the half that reads a plan. A caller runs [`explain_statement`] on the compiled SQL
//! (with the compiled parameters bound), hands the JSON text to [`analyse_plan`], and asks
//! [`check`] whether the plan is acceptable for the query's [`QueryPath`].
//!
//! # What counts as indexed
//!
//! | Plan node | Counted as |
//! | --- | --- |
//! | `Index Scan`, `Index Only Scan` | indexed (B-tree, or GiST for a radius or overlap test) |
//! | `Bitmap Index Scan` | indexed (B-tree, GIN for a search, GiST) |
//! | `Bitmap Heap Scan` with a `Recheck Cond` | the recheck node of a bitmap scan: indexed, never a scan of its own |
//! | `Seq Scan` | a sequential scan |
//! | anything else (joins, sorts, aggregates, `Result`, function and values scans) | neutral |
//!
//! The plan does not say which access method an index uses, only its name; the check therefore
//! asks "was an index used", not "was a GIN index used". A search with no GIN index has no other
//! index to use, so it plans as a sequential scan and fails.
//!
//! # The two paths
//!
//! - [`QueryPath::Transactional`]: any sequential scan fails ([`Coverage::Unindexed`]). This is
//!   the rule for a form, a lookup, a search box and a geo or range test that serves a
//!   transaction.
//! - [`QueryPath::Report`]: a scan is allowed but **marked** ([`Coverage::ReportScan`]), so a
//!   report that scans is visible in the gate's output and never silent.
//!
//! # Small tables
//!
//! On a table of a few rows the planner prefers a scan even when an index exists, so a test
//! loads enough rows and runs `ANALYZE`. It should not set `enable_seqscan = off`: every
//! compiled query ends with `ORDER BY id`, so a full scan of the primary key then stands in for
//! the sequential scan and hides exactly the failure this check is for. This module reads plans
//! and changes no setting.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::Value;
use spot_storage_contract::QueryPath;

/// How an index was used by a plan node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexScanKind {
    /// `Index Scan`.
    Index,
    /// `Index Only Scan`.
    IndexOnly,
    /// `Bitmap Index Scan` (under a `Bitmap Heap Scan`, whose recheck is counted in
    /// [`PlanCoverage::rechecks`]).
    Bitmap,
}

/// One index a plan reads.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexUse {
    /// The index name from the plan.
    pub index: String,
    /// The relation the plan node scans, when the node names one (a bitmap index scan does not).
    pub relation: Option<String>,
    /// How the index was read.
    pub kind: IndexScanKind,
}

/// What a plan does, as far as index coverage is concerned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanCoverage {
    /// Every index the plan reads, in plan order.
    pub indexed: Vec<IndexUse>,
    /// The relation of every sequential scan, in plan order (a repeat for each scan).
    pub sequential: Vec<String>,
    /// The number of `Bitmap Heap Scan` nodes with a recheck condition.
    pub rechecks: usize,
}

impl PlanCoverage {
    /// Whether the plan reads the index called `name`.
    #[must_use]
    pub fn uses_index(&self, name: &str) -> bool {
        self.indexed.iter().any(|i| i.index == name)
    }
}

/// The verdict of [`check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// No sequential scan.
    Covered,
    /// A transactional-path query scans: the gate fails. Names the relations scanned.
    Unindexed {
        /// The relations read by a sequential scan, sorted, without repeats.
        relations: Vec<String>,
    },
    /// A report-path query scans: allowed, and marked so it is visible.
    ReportScan {
        /// The relations read by a sequential scan, sorted, without repeats.
        relations: Vec<String>,
    },
}

impl Coverage {
    /// Whether the gate passes: covered, or a marked report scan.
    #[must_use]
    pub fn passes(&self) -> bool {
        !matches!(self, Self::Unindexed { .. })
    }
}

impl fmt::Display for Coverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Covered => write!(f, "covered by an index"),
            Self::Unindexed { relations } => write!(
                f,
                "a transactional-path query falls to a sequential scan of {}",
                relations.join(", ")
            ),
            Self::ReportScan { relations } => write!(
                f,
                "report-path query scans {} (allowed, marked)",
                relations.join(", ")
            ),
        }
    }
}

/// The plan text could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageError {
    reason: &'static str,
}

impl fmt::Display for CoverageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the query plan could not be read: {}", self.reason)
    }
}

impl std::error::Error for CoverageError {}

fn bad(reason: &'static str) -> CoverageError {
    CoverageError { reason }
}

/// The statement that produces the plan of `sql` as JSON. Bind the compiled parameters when
/// running it; the statement is only planned, never executed.
#[must_use]
pub fn explain_statement(sql: &str) -> String {
    format!("EXPLAIN (FORMAT JSON) {sql}")
}

/// Reads the text `EXPLAIN (FORMAT JSON)` returns and classifies every node.
///
/// # Errors
///
/// [`CoverageError`] when the text is not JSON or is not an `EXPLAIN` plan.
pub fn analyse_plan(explain_json: &str) -> Result<PlanCoverage, CoverageError> {
    let document: Value =
        serde_json::from_str(explain_json).map_err(|_| bad("it is not valid JSON"))?;
    let roots = document
        .as_array()
        .ok_or_else(|| bad("the top level is not an array"))?;
    let mut out = PlanCoverage::default();
    let mut found = false;
    for root in roots {
        let plan = root
            .get("Plan")
            .ok_or_else(|| bad("an entry has no `Plan`"))?;
        found = true;
        walk(plan, &mut out)?;
    }
    if found {
        Ok(out)
    } else {
        Err(bad("it holds no plan"))
    }
}

fn walk(node: &Value, out: &mut PlanCoverage) -> Result<(), CoverageError> {
    let kind = node
        .get("Node Type")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("a plan node has no `Node Type`"))?;
    let relation = node
        .get("Relation Name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let index = node
        .get("Index Name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match kind {
        "Seq Scan" => out
            .sequential
            .push(relation.unwrap_or_else(|| "(unnamed relation)".to_owned())),
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" => {
            let scan = match kind {
                "Index Scan" => IndexScanKind::Index,
                "Index Only Scan" => IndexScanKind::IndexOnly,
                _ => IndexScanKind::Bitmap,
            };
            out.indexed.push(IndexUse {
                index: index.ok_or_else(|| bad("an index scan names no index"))?,
                relation,
                kind: scan,
            });
        }
        "Bitmap Heap Scan" if node.get("Recheck Cond").is_some() => out.rechecks += 1,
        _ => {}
    }
    if let Some(children) = node.get("Plans") {
        for child in children
            .as_array()
            .ok_or_else(|| bad("`Plans` is not an array"))?
        {
            walk(child, out)?;
        }
    }
    Ok(())
}

/// Decides whether `plan` is acceptable for a query on `path`.
#[must_use]
pub fn check(path: QueryPath, plan: &PlanCoverage) -> Coverage {
    if plan.sequential.is_empty() {
        return Coverage::Covered;
    }
    let relations: Vec<String> = plan
        .sequential
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    match path {
        QueryPath::Transactional => Coverage::Unindexed { relations },
        QueryPath::Report => Coverage::ReportScan { relations },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(node: &str) -> String {
        format!("[{{\"Plan\": {node}}}]")
    }

    #[test]
    fn a_bitmap_scan_on_a_gin_index_with_its_recheck_is_indexed() {
        let json = plan(
            r#"{"Node Type": "Bitmap Heap Scan", "Relation Name": "spot_search_doc",
                "Recheck Cond": "(terms @@ q)", "Plans": [
                  {"Node Type": "Bitmap Index Scan", "Index Name": "spot_search_doc_terms_gin"}]}"#,
        );
        let c = analyse_plan(&json).unwrap_or_else(|e| panic!("{e}"));
        assert!(c.uses_index("spot_search_doc_terms_gin"));
        assert_eq!(c.rechecks, 1);
        assert!(c.sequential.is_empty());
        assert_eq!(check(QueryPath::Transactional, &c), Coverage::Covered);
    }

    #[test]
    fn an_index_scan_and_an_index_only_scan_are_indexed_at_any_depth() {
        let json = plan(
            r#"{"Node Type": "Nested Loop", "Plans": [
                  {"Node Type": "Index Scan", "Relation Name": "a", "Index Name": "a_gix"},
                  {"Node Type": "Gather", "Plans": [
                     {"Node Type": "Index Only Scan", "Relation Name": "b", "Index Name": "b_pk"}]}]}"#,
        );
        let c = analyse_plan(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.indexed.len(), 2);
        assert_eq!(c.indexed[0].kind, IndexScanKind::Index);
        assert_eq!(c.indexed[1].kind, IndexScanKind::IndexOnly);
        assert_eq!(c.indexed[1].relation.as_deref(), Some("b"));
        assert!(check(QueryPath::Transactional, &c).passes());
    }

    #[test]
    fn a_sequential_scan_fails_a_transactional_path_and_marks_a_report() {
        let json = plan(r#"{"Node Type": "Seq Scan", "Relation Name": "spot_search_doc"}"#);
        let c = analyse_plan(&json).unwrap_or_else(|e| panic!("{e}"));
        let transactional = check(QueryPath::Transactional, &c);
        assert_eq!(
            transactional,
            Coverage::Unindexed {
                relations: vec!["spot_search_doc".to_owned()]
            }
        );
        assert!(!transactional.passes());
        let report = check(QueryPath::Report, &c);
        assert!(matches!(report, Coverage::ReportScan { .. }));
        assert!(report.passes());
        assert!(report.to_string().contains("marked"));
    }

    #[test]
    fn repeated_scans_of_one_relation_are_named_once() {
        let json = plan(
            r#"{"Node Type": "Hash Join", "Plans": [
                  {"Node Type": "Seq Scan", "Relation Name": "t"},
                  {"Node Type": "Seq Scan", "Relation Name": "t"}]}"#,
        );
        let c = analyse_plan(&json).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.sequential.len(), 2);
        let Coverage::Unindexed { relations } = check(QueryPath::Transactional, &c) else {
            panic!("expected a failure");
        };
        assert_eq!(relations, vec!["t".to_owned()]);
    }

    #[test]
    fn neutral_nodes_and_a_planless_statement_are_handled() {
        let c = analyse_plan(&plan(r#"{"Node Type": "Result"}"#)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c, PlanCoverage::default());
        assert!(analyse_plan("[]").is_err());
        assert!(analyse_plan("not json").is_err());
        assert!(analyse_plan(r#"{"Plan": {}}"#).is_err());
        assert!(analyse_plan(r#"[{"Plan": {"Node Type": "Index Scan"}}]"#).is_err());
    }

    #[test]
    fn the_statement_wraps_the_sql_without_touching_it() {
        assert_eq!(
            explain_statement("SELECT 1 FROM t WHERE a = $1"),
            "EXPLAIN (FORMAT JSON) SELECT 1 FROM t WHERE a = $1"
        );
    }
}
