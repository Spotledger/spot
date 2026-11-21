//! Fleet health and monitoring (WP-CTL-03.T1, blueprint WS18, ADR-067, ADR-076).
//!
//! The control plane sees many tenants on many nodes. This crate turns the telemetry those
//! nodes produce into **one calm status per tenant and per node**: green, amber or red, with a
//! sentence in plain language. It owns the decision, not the collectors and not the console.
//!
//! # Shape
//!
//! - [`signal`]: the normalised inputs. A [`Observation`](signal::Observation) is one signal
//!   (health probe, error rate, queue lag, job failures, replication lag, disk, certificate
//!   expiry, licence state) for one tenant or node, stamped with the time it was taken.
//!   Timestamps are arguments: the crate never reads a wall clock.
//! - [`rules`]: the health rules, loaded from `rules/*.toml`. A rule names its signal, the
//!   comparison and threshold, how long the breach must last, the severity, the sentence
//!   template and the suggested action. [`RuleSet::builtin`] is the shipped default set.
//! - [`evaluate`]: a **pure** function over a window of observations. It produces a
//!   [`FleetHealth`](health::FleetHealth) with one [`SubjectHealth`](health::SubjectHealth)
//!   per subject. A subject with no recent data is **amber**, never green.
//! - [`alerts`]: the stateful alert side. [`AlertManager`](alerts::AlertManager) deduplicates
//!   per (subject, rule), damps flapping with a clear delay, groups notifications per tenant,
//!   honours silences with an expiry, and escalates a red alert through data-defined steps.
//! - [`sinks`]: the delivery boundary. An [`AlertSink`](sinks::AlertSink) receives one grouped
//!   notification; the `mail` feature adds the `spot-mail` adapter.
//! - [`cardinality`]: the ADR-076 part 3 rule. Series are keyed only by subject kind, subject
//!   id and signal; arbitrary labels cannot enter. A budget caps distinct series and folds the
//!   rest into one counted overflow bucket.
//! - [`settings`]: the evaluation cadence and window, with the defaults that make a seeded
//!   failure visible inside one minute.
//!
//! # Purity
//!
//! Rule evaluation has no clock, no I/O and no ambient state: `now` is passed to
//! [`evaluate`](evaluate::evaluate). There is no floating-point arithmetic (fractions are
//! basis points), no `unsafe`, and no secret is logged or returned. The alert manager is
//! stateful, but its inputs are still explicit (`observe` and `add_silence` take `now`).

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]
#![cfg_attr(test, allow(clippy::expect_used))]

pub mod alerts;
pub mod cardinality;
pub mod error;
pub mod evaluate;
pub mod health;
#[cfg(feature = "mail")]
pub mod mail;
pub mod rules;
pub mod settings;
pub mod signal;
pub mod sinks;
pub mod time;

pub use alerts::{
    Alert, AlertBatch, AlertKey, AlertManager, AlertNotification, AlertSettings, AlertSummary,
    EscalationStep, NotificationReason, Silence, SinkFailure,
};
pub use cardinality::{Admission, SeriesBudget, SeriesKey, TelemetryIngest};
pub use error::FleetHealthError;
pub use evaluate::evaluate;
pub use health::{Finding, FleetHealth, Health, HealthCounts, SubjectHealth};
pub use rules::{Comparison, FreshnessRule, Rule, RuleId, RuleSet};
pub use settings::Settings;
pub use signal::{
    LICENCE_ACTIVE, LICENCE_EXPIRED, LICENCE_GRACE, LicenceState, NodeId, Observation, Signal,
    Subject, SubjectKind, TenantId,
};
pub use sinks::{
    AlertSink, MemoryAlertSink, SinkError, WebhookAlertSink, WebhookPoster, WebhookPosterFn,
};
pub use time::{Timestamp, humanise_seconds};

/// The crate name, for diagnostics.
pub const CRATE_NAME: &str = "spot-fleet-health";

/// The crate version, for diagnostics and the error catalogue's `since` rows.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
