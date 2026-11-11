//! Satisfaction evidence: one range event when the applied prefix crosses mandatory events.

use crate::error::ObligationError;
use crate::model::{MandatoryEvent, ObligationSatisfied};

/// The evidence for a prefix move from `prefix_before` to `prefix_after`.
///
/// Returns `None` when the move crossed no mandatory event (including no move at all), otherwise
/// one [`ObligationSatisfied`] covering the whole range. The clock values are arguments: the
/// node's reported time and the hub's observed time.
///
/// # Errors
/// [`ObligationError::PrefixRegressed`] when `prefix_after < prefix_before`.
pub fn evidence(
    node: &str,
    prefix_before: u64,
    prefix_after: u64,
    events: &[MandatoryEvent],
    applied_at_reported_millis: i64,
    received_at_observed_millis: i64,
) -> Result<Option<ObligationSatisfied>, ObligationError> {
    if prefix_after < prefix_before {
        return Err(ObligationError::PrefixRegressed {
            before: prefix_before,
            after: prefix_after,
        });
    }
    let mut positions: Vec<u64> = events
        .iter()
        .map(|e| e.position)
        .filter(|p| *p > prefix_before && *p <= prefix_after)
        .collect();
    positions.sort_unstable();
    positions.dedup();
    if positions.is_empty() {
        return Ok(None);
    }
    Ok(Some(ObligationSatisfied {
        node: node.to_owned(),
        through_position: prefix_after,
        count: positions.len() as u64,
        applied_at_reported_millis,
        received_at_observed_millis,
    }))
}
