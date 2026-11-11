//! `projection_lag`: how far the projection is behind the entity table, as a figure every search
//! answer carries (ADR-087 sections 7.5 and 7.8).
//!
//! On the database path the lag is zero by construction — the projection is written in the same
//! transaction as the document, so a committed write is always projected. The ADR still requires
//! the figure, for two reasons that both matter here:
//!
//! - a **rebuild** (or a migration that adds a searchable field) leaves documents unprojected
//!   until it finishes, and a search must be able to say so rather than answer as if the
//!   projection were complete;
//! - the future external engine is fed from the outbox, where lag is real and unbounded unless it
//!   is measured; the contract, the metric name and the refusal are the same, so switching the
//!   engine does not change what a caller sees.
//!
//! The figure is two integers and never a float: `missing` is how many entity rows have no
//! projection row, and `age_ms` is how long ago the DocType's newest projection row was written
//! (whole milliseconds, `None` when nothing is projected yet). Both are exposed as fields of
//! [`ProjectionLag`], and both are logged once per measurement under the metric name
//! `projection_lag`.

use spot_postgres::PgTransaction;
use spot_storage_contract::Name;

use crate::error::SearchError;
use crate::plan::ProjectionPlan;
use crate::schema::{DOC_ID, DOCTYPE, TABLE, UPDATED_AT, quote};

/// The metric name the lag is reported under, one line per measurement.
pub const METRIC: &str = "projection_lag";

/// The default staleness bound: a search refuses to answer when the projection's newest row is
/// older than this and documents are missing. One minute is generous for a same-transaction
/// projection; it is a configurable default, not a measured value (ADR-069's rule for the
/// construct limits).
pub const DEFAULT_BOUND_MS: u64 = 60_000;

/// How far a DocType's projection is behind its entity table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionLag {
    /// The DocType measured.
    pub doctype: Name,
    /// Projection rows for this DocType.
    pub projected: u64,
    /// Entity rows with no projection row — the only true staleness signal.
    pub missing: u64,
    /// Whole milliseconds since the DocType's newest projection row was written; `None` when the
    /// DocType has no projection row at all.
    pub age_ms: Option<u64>,
}

impl ProjectionLag {
    /// Whether the projection is complete and fresh enough to answer a read.
    #[must_use]
    pub fn within_bound(&self, bound_ms: u64) -> bool {
        self.missing == 0 && self.age_ms.is_none_or(|age| age <= bound_ms)
    }

    /// The metric as a number for the log line: `missing` documents behind.
    #[must_use]
    pub fn value(&self) -> u64 {
        self.missing
    }

    /// Records the figure once, as the `projection_lag` metric. Names and counts only.
    pub fn record(&self, bound_ms: u64) {
        tracing::info!(
            metric = METRIC,
            doctype = %self.doctype,
            projected = self.projected,
            missing = self.missing,
            age_ms = self.age_ms.unwrap_or(0),
            bound_ms,
            within_bound = self.within_bound(bound_ms),
            "projection lag"
        );
    }
}

/// Measures the lag of `plan`'s DocType.
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn measure(
    tx: &mut PgTransaction,
    plan: &ProjectionPlan,
) -> Result<ProjectionLag, SearchError> {
    measure_doctype(tx, plan.doctype()).await
}

/// Measures the lag of `doctype`, which need not have a projection planned (a DocType that was
/// searchable and no longer is still has rows to clean up).
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn measure_doctype(
    tx: &mut PgTransaction,
    doctype: &Name,
) -> Result<ProjectionLag, SearchError> {
    let sql = format!(
        "SELECT (SELECT count(*) FROM {table} WHERE {doctype_col} = $1::text), \
         (SELECT count(*) FROM {entity} e WHERE NOT EXISTS (SELECT 1 FROM {table} d \
           WHERE d.{doc_id} = e.\"id\" AND d.{doctype_col} = $1::text)), \
         (SELECT (extract(epoch FROM (now() - max({updated}))) * 1000)::bigint FROM {table} \
           WHERE {doctype_col} = $1::text)",
        table = quote(TABLE),
        doctype_col = quote(DOCTYPE),
        entity = quote(doctype.as_str()),
        doc_id = quote(DOC_ID),
        updated = quote(UPDATED_AT),
    );
    let row = tx
        .raw_query_opt(&sql, &[&doctype.as_str()], None)
        .await?
        .ok_or_else(|| SearchError::internal("the projection-lag statement returned no row"))?;
    let projected = count(&row, 0)?;
    let missing = count(&row, 1)?;
    let age_ms = row
        .try_get::<_, Option<i64>>(2)
        .unwrap_or(None)
        .and_then(|ms| u64::try_from(ms).ok());
    Ok(ProjectionLag {
        doctype: doctype.clone(),
        projected,
        missing,
        age_ms,
    })
}

fn count(row: &tokio_postgres::Row, index: usize) -> Result<u64, SearchError> {
    let value = row.try_get::<_, i64>(index).unwrap_or(0);
    Ok(u64::try_from(value).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lag(missing: u64, age_ms: Option<u64>) -> ProjectionLag {
        ProjectionLag {
            doctype: Name::new("listing").unwrap_or_else(|e| panic!("{e}")),
            projected: 0,
            missing,
            age_ms,
        }
    }

    #[test]
    fn a_complete_fresh_projection_is_within_the_bound() {
        assert!(lag(0, Some(10)).within_bound(1000));
        assert!(
            lag(0, None).within_bound(0),
            "nothing projected yet, nothing missing"
        );
        assert_eq!(lag(0, Some(10)).value(), 0);
    }

    #[test]
    fn a_missing_row_or_a_stale_age_is_outside_the_bound() {
        assert!(!lag(1, Some(10)).within_bound(1000));
        assert!(!lag(0, Some(2000)).within_bound(1000));
        assert_eq!(lag(3, Some(1)).value(), 3);
        assert_eq!(METRIC, "projection_lag");
    }

    #[test]
    fn the_figures_are_integers_never_floats() {
        let lag = lag(3, Some(1));
        let _: u64 = lag.value();
        let _: u64 = lag.projected;
        let _: Option<u64> = lag.age_ms;
        let _: u64 = DEFAULT_BOUND_MS;
    }
}
