//! The operational series of the journal, sync, quarantine, conditions and migration subsystems
//! (WP-OPS-01.T2).
//!
//! This crate owns the **names, units, labels and meanings** of the series so every host module
//! records them the same way, and none can add an unbounded label by accident:
//!
//! - every label value is a closed enum with `as_str()`; a helper takes the enum, never a
//!   `&str`;
//! - there is no label for a site, tenant, node, user, document, topic or condition name:
//!   per-node and per-condition detail belongs to the admin screens and the audit;
//! - counts are `u64`, converted to `f64` once at the facade, clamped to 2^53 (see
//!   [`MAX_EXACT_COUNT`]); durations are [`std::time::Duration`], converted once.
//!
//! Every function does nothing when no recorder is installed (the `metrics` facade guarantees
//! it). [`describe_all`] registers the descriptions once, after the recorder exists.
//!
//! The service that fills the gauges in `spot serve` is WP-OPS-01.T3; [`catalogue::SERIES`] is
//! the single list the documentation (`docs/running-spot/signals.md`) and the tests
//! read.
#![forbid(unsafe_code)]

/// Defines a closed label enum: its variants, `ALL` and `as_str()`.
macro_rules! label_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }
        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            /// The label value this variant records as.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }
    };
}

pub mod catalogue;
pub mod conditions;
pub mod journal;
pub mod migration;
pub mod quarantine;
pub mod sync;

use std::sync::atomic::{AtomicBool, Ordering};

/// The largest count a gauge or counter carries exactly: 2^53. A larger count loses precision as
/// an `f64`, so the helpers clamp to this value instead.
pub const MAX_EXACT_COUNT: u64 = 1 << 53;

/// A count as the facade's `f64`, clamped to [`MAX_EXACT_COUNT`].
#[allow(clippy::cast_precision_loss)]
pub(crate) fn count_value(count: u64) -> f64 {
    count.min(MAX_EXACT_COUNT) as f64
}

static DESCRIBED: AtomicBool = AtomicBool::new(false);

/// Registers the description and unit of every series, once; later calls do nothing.
///
/// Call it after the recorder exists. `spot-telemetry` has no way to ask whether the recorder is
/// installed, so this function installs it first ([`spot_telemetry::metrics::install`] is idempotent) and
/// returns that call's error when the recorder cannot be installed.
///
/// # Errors
///
/// The Prometheus recorder could not be installed in this process.
pub fn describe_all() -> Result<(), spot_telemetry::TelemetryError> {
    spot_telemetry::metrics::install()?;
    if DESCRIBED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    catalogue::describe();
    Ok(())
}
