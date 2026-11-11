//! The duration estimate: integer seconds from table statistics and a [`CostModel`].
//!
//! The estimate is deliberately conservative and explainable. For a **row-bound** change — a
//! backfill, an index build, a `SET NOT NULL` sweep, a constraint validation, a foreign-key
//! validation — the expected seconds are `rows / rate`, rounded up, with the rate taken from
//! the cost model (`rows_per_second`). The low bound is a fraction of that and the high bound a
//! multiple, both from the model. For every other change the expected seconds are the model's
//! fixed cost for the change's kind. A change on an entity with **no statistics** is marked
//! `unknown`: its low bound is zero, its expected bound is computed from a conservative
//! fallback row count, and its high bound is the fallback times the model's
//! `unknown_high_factor` — the range widens rather than the estimate guessing zero.
//!
//! Each estimate also carries the change's [`BlockingClass`], so the plan says whether users
//! are affected. The plan's blocking class is the maximum over its changes.

use serde::{Deserialize, Serialize};

use crate::change::{BlockingClass, Change, ChangeKind};
use crate::error::PlanError;
use crate::model::TableStats;
use crate::plan::ReleasePlan;

/// The rates and fixed costs the estimate is computed from. Every field is an integer; there
/// is no float arithmetic (ADR-12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostModel {
    /// Rows a batched backfill fills per second.
    pub backfill_rows_per_second: u64,
    /// Rows an index build, a validation or a sweep checks per second.
    pub index_rows_per_second: u64,
    /// The cost in seconds of a change that needs a brief lock.
    pub brief_lock_seconds: u64,
    /// The cost in seconds of an online schema change.
    pub online_seconds: u64,
    /// The cost in seconds of a change that needs a maintenance window.
    pub maintenance_seconds: u64,
    /// The cost in seconds of one metadata change.
    pub metadata_change_seconds: u64,
    /// The cost in seconds of one patch.
    pub patch_seconds: u64,
    /// The cost in seconds of one behaviour change.
    pub behaviour_change_seconds: u64,
    /// The fallback row count used when an entity has no statistics.
    pub unknown_rows: u64,
    /// How many times the fallback the high bound of an unknown estimate is.
    pub unknown_high_factor: u64,
    /// The low bound as a percentage of the expected seconds.
    pub spread_low_percent: u64,
    /// The high bound as a percentage of the expected seconds.
    pub spread_high_percent: u64,
}

impl Default for CostModel {
    /// Conservative defaults: a batched backfill at 5,000 rows/s, an index build at 50,000
    /// rows/s, a 5-second brief lock, a 60-second maintenance step, small fixed costs for
    /// metadata, patches and behaviour, a one-million-row fallback for unknown statistics, and
    /// a 50%–200% spread around the expected value.
    fn default() -> Self {
        Self {
            backfill_rows_per_second: 5_000,
            index_rows_per_second: 50_000,
            brief_lock_seconds: 5,
            online_seconds: 1,
            maintenance_seconds: 60,
            metadata_change_seconds: 2,
            patch_seconds: 30,
            behaviour_change_seconds: 5,
            unknown_rows: 1_000_000,
            unknown_high_factor: 10,
            spread_low_percent: 50,
            spread_high_percent: 200,
        }
    }
}

impl CostModel {
    /// The conservative defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads a model from TOML; absent keys keep their default.
    ///
    /// ```toml
    /// backfill_rows_per_second = 2000
    /// index_rows_per_second = 20000
    /// ```
    pub fn from_toml(text: &str) -> Result<Self, PlanError> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Partial {
            backfill_rows_per_second: Option<u64>,
            index_rows_per_second: Option<u64>,
            brief_lock_seconds: Option<u64>,
            online_seconds: Option<u64>,
            maintenance_seconds: Option<u64>,
            metadata_change_seconds: Option<u64>,
            patch_seconds: Option<u64>,
            behaviour_change_seconds: Option<u64>,
            unknown_rows: Option<u64>,
            unknown_high_factor: Option<u64>,
            spread_low_percent: Option<u64>,
            spread_high_percent: Option<u64>,
        }
        let p: Partial = toml::from_str(text).map_err(|e| PlanError::BadCostModel {
            message: e.to_string(),
        })?;
        let d = Self::default();
        let m = Self {
            backfill_rows_per_second: p
                .backfill_rows_per_second
                .unwrap_or(d.backfill_rows_per_second),
            index_rows_per_second: p.index_rows_per_second.unwrap_or(d.index_rows_per_second),
            brief_lock_seconds: p.brief_lock_seconds.unwrap_or(d.brief_lock_seconds),
            online_seconds: p.online_seconds.unwrap_or(d.online_seconds),
            maintenance_seconds: p.maintenance_seconds.unwrap_or(d.maintenance_seconds),
            metadata_change_seconds: p
                .metadata_change_seconds
                .unwrap_or(d.metadata_change_seconds),
            patch_seconds: p.patch_seconds.unwrap_or(d.patch_seconds),
            behaviour_change_seconds: p
                .behaviour_change_seconds
                .unwrap_or(d.behaviour_change_seconds),
            unknown_rows: p.unknown_rows.unwrap_or(d.unknown_rows),
            unknown_high_factor: p.unknown_high_factor.unwrap_or(d.unknown_high_factor),
            spread_low_percent: p.spread_low_percent.unwrap_or(d.spread_low_percent),
            spread_high_percent: p.spread_high_percent.unwrap_or(d.spread_high_percent),
        };
        if m.backfill_rows_per_second == 0
            || m.index_rows_per_second == 0
            || m.spread_high_percent == 0
        {
            return Err(PlanError::BadCostModel {
                message: "a rate or spread is zero".to_owned(),
            });
        }
        Ok(m)
    }
}

/// The estimate of one change, in seconds, with its inputs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEstimate {
    /// The change's id.
    pub change_id: String,
    /// The low bound, in seconds.
    pub low: u64,
    /// The expected value, in seconds.
    pub expected: u64,
    /// The high bound, in seconds.
    pub high: u64,
    /// How the change affects users.
    pub blocking: BlockingClass,
    /// Whether the entity had no statistics, so the range is widened.
    pub unknown: bool,
    /// The row count used, when the change is row-bound and the count is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// The rate used, when the change is row-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_per_second: Option<u64>,
}

/// The whole plan's estimate, in integer seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    /// The low bound: every change at its low bound.
    pub low: u64,
    /// The expected value.
    pub expected: u64,
    /// The high bound: every change at its high bound.
    pub high: u64,
    /// One entry per change, sorted by change id.
    pub per_change: Vec<ChangeEstimate>,
    /// The most disruptive blocking class in the plan.
    pub blocking: BlockingClass,
    /// Whether any change had unknown statistics.
    pub unknown: bool,
}

impl Estimate {
    /// An estimate with nothing in it.
    pub fn empty() -> Self {
        Self {
            low: 0,
            expected: 0,
            high: 0,
            per_change: Vec::new(),
            blocking: BlockingClass::Online,
            unknown: false,
        }
    }
}

/// Estimates a plan from table statistics and a cost model.
pub fn estimate(plan: &ReleasePlan, stats: &TableStats, model: &CostModel) -> Estimate {
    estimate_changes(&plan.changes, stats, model)
}

/// Estimates a list of changes. The public [`estimate`] wraps this with a plan.
pub(crate) fn estimate_changes(
    changes: &[Change],
    stats: &TableStats,
    model: &CostModel,
) -> Estimate {
    let mut per_change: Vec<ChangeEstimate> = changes
        .iter()
        .map(|c| estimate_change(c, stats, model))
        .collect();
    per_change.sort_by(|a, b| a.change_id.cmp(&b.change_id));
    let mut total = Estimate::empty();
    total.blocking = per_change
        .iter()
        .map(|c| c.blocking)
        .max()
        .unwrap_or(BlockingClass::Online);
    total.unknown = per_change.iter().any(|c| c.unknown);
    for c in &per_change {
        total.low = total.low.saturating_add(c.low);
        total.expected = total.expected.saturating_add(c.expected);
        total.high = total.high.saturating_add(c.high);
    }
    total.per_change = per_change;
    total
}

/// The rate a row-bound change is measured against, if it is row-bound.
fn row_rate(kind: ChangeKind) -> Option<bool> {
    // `true` selects the backfill rate; `false` the index/validation rate.
    match kind {
        ChangeKind::SchemaBackfill | ChangeKind::SchemaSetNotNull => Some(true),
        ChangeKind::SchemaAddIndex
        | ChangeKind::SchemaValidate
        | ChangeKind::SchemaAddForeignKey => Some(false),
        _ => None,
    }
}

/// The fixed cost of a change that is not row-bound.
fn fixed_seconds(change: &Change, model: &CostModel) -> u64 {
    match change.area {
        crate::change::Area::Metadata => model.metadata_change_seconds,
        crate::change::Area::Patch => model.patch_seconds,
        crate::change::Area::Behaviour => model.behaviour_change_seconds,
        crate::change::Area::Schema => match change.blocking {
            BlockingClass::Online => model.online_seconds,
            BlockingClass::BriefLock => model.brief_lock_seconds,
            BlockingClass::MaintenanceWindow => model.maintenance_seconds,
        },
    }
}

/// The entity a change affects, for a statistics lookup: the first segment of its target.
fn entity_of(change: &Change) -> &str {
    change
        .target
        .split(['.', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(change.target.as_str())
}

/// Estimates one change.
fn estimate_change(change: &Change, stats: &TableStats, model: &CostModel) -> ChangeEstimate {
    let spread_low = |expected: u64| -> u64 {
        let low = expected.saturating_mul(model.spread_low_percent) / 100;
        low.max(u64::from(expected > 0))
    };
    let spread_high =
        |expected: u64| -> u64 { expected.saturating_mul(model.spread_high_percent) / 100 };

    let Some(backfill) = row_rate(change.kind) else {
        let expected = fixed_seconds(change, model);
        return ChangeEstimate {
            change_id: change.id.clone(),
            low: spread_low(expected),
            expected,
            high: spread_high(expected),
            blocking: change.blocking,
            unknown: false,
            rows: None,
            rows_per_second: None,
        };
    };
    let rate = if backfill {
        model.backfill_rows_per_second
    } else {
        model.index_rows_per_second
    };
    match stats.rows(entity_of(change)) {
        Some(rows) => {
            let expected = rows.div_ceil(rate);
            ChangeEstimate {
                change_id: change.id.clone(),
                low: spread_low(expected),
                expected,
                high: spread_high(expected),
                blocking: change.blocking,
                unknown: false,
                rows: Some(rows),
                rows_per_second: Some(rate),
            }
        }
        None => {
            let expected = model.unknown_rows.div_ceil(rate);
            ChangeEstimate {
                change_id: change.id.clone(),
                low: 0,
                expected,
                high: expected.saturating_mul(model.unknown_high_factor),
                blocking: change.blocking,
                unknown: true,
                rows: None,
                rows_per_second: Some(rate),
            }
        }
    }
}
