//! Assertion helpers shared by the scenarios: nothing panics mid-scenario; every failure
//! becomes a recorded [`Outcome`](crate::Outcome) so one failing scenario does not hide the
//! others' results.
//!
//! The `apply`/`probe` helpers map a host answer into the scenario's `Result`; the `check*`
//! helpers turn an assertion into a failure message naming what was checked.

use crate::change::Refusal;
use crate::{Change, LivenessHost, Probe, ProbeOutcome};

/// Applies `change`, expecting success; back for further assertions (`?`-able).
pub(crate) fn apply_ok(host: &mut dyn LivenessHost, change: &Change) -> Result<u64, String> {
    host.apply(change)
        .map(|e| e.new_version)
        .map_err(|e| format!("the change was refused: {e}"))
}

/// Asserts that applying `change` is refused with `expected` and that the metadata does not
/// move (same version, same `Arc`).
pub(crate) fn apply_refused(
    host: &mut dyn LivenessHost,
    change: &Change,
    expected: &Refusal,
    what: &str,
) -> Result<(), String> {
    let before = host.meta();
    match host.apply(change) {
        Ok(effect) => Err(format!(
            "{what}: the change was accepted (new version {}), but it must be refused",
            effect.new_version
        )),
        Err(e) => {
            if &e.refusal == expected {
                let after = host.meta();
                check_eq(
                    before.version(),
                    after.version(),
                    format!("{what}: the metadata version"),
                )?;
                check(
                    std::sync::Arc::ptr_eq(&before, &after),
                    format!("{what}: the effective metadata must be completely untouched"),
                )?;
                Ok(())
            } else {
                Err(format!(
                    "{what}: refused as {:?}, expected {expected:?}",
                    e.refusal
                ))
            }
        }
    }
}

/// Consults `probe` after a change landed at `expected`; records the status and hands the
/// scenario the notes, or names the staleness as a failure.
pub(crate) fn probe(
    host: &mut dyn LivenessHost,
    layer: Probe,
    expected: u64,
) -> Result<(Vec<String>, Option<String>), String> {
    match host.probe(layer) {
        ProbeOutcome::NotSupported => Ok((
            vec![format!("the {layer:?} probe is not supported on this host")],
            Some("not supported".to_owned()),
        )),
        ProbeOutcome::Live { version } if version == expected => Ok((
            vec![format!(
                "the {layer:?} probe saw version {expected} within the same apply"
            )],
            Some("live".to_owned()),
        )),
        ProbeOutcome::Live { version } => Err(format!(
            "the {layer:?} probe saw version {version}, but the metadata is at {expected}"
        )),
        ProbeOutcome::Stale { observed, seen } => Err(format!(
            "the {layer:?} probe is stale: it saw {observed} against {seen}"
        )),
    }
}

/// Asserts `condition`, recording `what` on failure.
pub(crate) fn check(condition: bool, what: impl Into<String>) -> Result<(), String> {
    if condition { Ok(()) } else { Err(what.into()) }
}

/// Asserts `left == right`, showing both on failure.
pub(crate) fn check_eq<T: PartialEq + std::fmt::Debug>(
    left: T,
    right: T,
    what: impl Into<String>,
) -> Result<(), String> {
    if left == right {
        Ok(())
    } else {
        Err(format!(
            "{}: {left:?} != {right:?}",
            Into::<String>::into(what)
        ))
    }
}

/// The app views of every DocType, in a deterministic, comparable form (their debug render, in
/// name order). The determinism scenario compares two runs' lists byte for byte.
pub(crate) fn views_debug(meta: &spot_meta::EffectiveMeta) -> Vec<String> {
    meta.doctypes().map(|d| format!("{d:?}")).collect()
}
