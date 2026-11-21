//! The derived ledger: which mandatory events a destination still owes.

use crate::error::ObligationError;
use crate::model::{MandatoryEvent, Obligation};

/// Mandatory events with a position above `applied_prefix`, plus those named in `holds`.
///
/// `holds` lists the positions of mandatory records the destination holds back (a hold does not
/// lower the prefix, ADR-065 section 1), so a held record at or below the prefix is still
/// outstanding. The result is in position order with no duplicates, and nothing is stored.
///
/// # Errors
/// [`ObligationError::HoldUnknown`] when a hold names a position not among `events`.
pub fn outstanding(
    events: &[MandatoryEvent],
    applied_prefix: u64,
    holds: &[u64],
) -> Result<Vec<Obligation>, ObligationError> {
    for hold in holds {
        if !events.iter().any(|e| e.position == *hold) {
            return Err(ObligationError::HoldUnknown(*hold));
        }
    }
    let mut out: Vec<Obligation> = events
        .iter()
        .filter(|e| e.position > applied_prefix || holds.contains(&e.position))
        .map(|e| Obligation {
            event: e.clone(),
            held: e.position <= applied_prefix,
        })
        .collect();
    out.sort_by_key(|o| o.event.position);
    out.dedup_by_key(|o| o.event.position);
    Ok(out)
}
