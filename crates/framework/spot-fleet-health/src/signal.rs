//! The normalised inputs: signals, subjects and observations.
//!
//! Telemetry arrives from many places (the native health probes, the database backend, the job
//! engine, the licence guard), in different shapes. This module is the one shape they are
//! normalised into, so the health rules never need to know where a number came from.
//!
//! # What an observation is not
//!
//! An observation has no free-form labels. A subject is a tenant or a node, a signal is one of
//! the closed [`Signal`] variants, and the value is an integer in the signal's own unit. That
//! is the cardinality rule of ADR-076 part 3 expressed in the type: a request id, a user, a
//! path or any other unbounded value cannot ride along and multiply series.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::FleetHealthError;
use crate::time::{Timestamp, humanise_seconds};

/// Whether a subject is a tenant or a node. Rules may apply to one or to either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// A tenant database and everything the control plane runs for it.
    Tenant,
    /// One physical or virtual node in a cluster.
    Node,
    /// A rule that applies to both tenants and nodes (never a subject's own kind).
    Either,
}

impl SubjectKind {
    /// The kind as a stable lower-case word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Node => "node",
            Self::Either => "either",
        }
    }
}

impl fmt::Display for SubjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// The identifier of a tenant, as the control plane knows it.
///
/// Validated on construction and on deserialization: 1 to 128 characters from the ASCII
/// letters, digits and `-`, `_`, `.`, `:`. An empty or malformed id cannot exist.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantId(String);

impl TenantId {
    /// Validates and builds a tenant id.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidIdentifier`] when the text is empty, longer than 128 bytes
    /// or contains a character outside the allowed set.
    pub fn try_new(value: impl Into<String>) -> Result<Self, FleetHealthError> {
        let value = value.into();
        validate_identifier(&value, "tenant")?;
        Ok(Self(value))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TenantId {
    type Error = FleetHealthError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<TenantId> for String {
    fn from(value: TenantId) -> Self {
        value.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

/// The identifier of a node, as the control plane knows it.
///
/// Validated exactly like [`TenantId`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeId(String);

impl NodeId {
    /// Validates and builds a node id.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidIdentifier`] when the text is empty, longer than 128 bytes
    /// or contains a character outside the allowed set.
    pub fn try_new(value: impl Into<String>) -> Result<Self, FleetHealthError> {
        let value = value.into();
        validate_identifier(&value, "node")?;
        Ok(Self(value))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NodeId {
    type Error = FleetHealthError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

fn validate_identifier(value: &str, kind: &'static str) -> Result<(), FleetHealthError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(FleetHealthError::InvalidIdentifier { kind });
    }
    Ok(())
}

/// The thing an observation is about.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Subject {
    /// A tenant and its database.
    Tenant(TenantId),
    /// A node in a cluster.
    Node(NodeId),
}

impl Subject {
    /// The subject as a tenant, when it is one.
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Tenant(id) => Some(id),
            Self::Node(_) => None,
        }
    }

    /// The subject as a node, when it is one.
    #[must_use]
    pub fn node(&self) -> Option<&NodeId> {
        match self {
            Self::Node(id) => Some(id),
            Self::Tenant(_) => None,
        }
    }

    /// Whether the subject is a tenant or a node.
    #[must_use]
    pub const fn kind(&self) -> SubjectKind {
        match self {
            Self::Tenant(_) => SubjectKind::Tenant,
            Self::Node(_) => SubjectKind::Node,
        }
    }

    /// The subject in plain language, for a sentence: `tenant acme` or `node ln-1`.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Tenant(id) => format!("tenant {id}"),
            Self::Node(id) => format!("node {id}"),
        }
    }
}

/// The closed set of signals the fleet health engine understands.
///
/// The unit is part of the signal: `error_rate` and `disk_usage` are integer basis points
/// (10,000 = 100%), the lag signals and `certificate_expiry` are seconds, `job_failures` is a
/// count, `health_probe` is 1 for passing and 0 for failing, and `licence_state` is one of the
/// [`LicenceState`] codes. No signal carries a float or an arbitrary label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// The node or tenant answered its health probe (1) or did not (0).
    HealthProbe,
    /// Failed requests in the window, in basis points.
    ErrorRate,
    /// Age in seconds of the oldest waiting item in the queue.
    QueueLag,
    /// Failed jobs in the window.
    JobFailures,
    /// Replication delay in seconds behind the primary.
    ReplicationLag,
    /// Disk capacity in use, in basis points.
    DiskUsage,
    /// Seconds until the certificate expires; negative once it has expired.
    CertificateExpiry,
    /// The licence state code: active, grace or expired.
    LicenceState,
}

impl Signal {
    /// Every signal, in declaration order.
    pub const ALL: [Signal; 8] = [
        Self::HealthProbe,
        Self::ErrorRate,
        Self::QueueLag,
        Self::JobFailures,
        Self::ReplicationLag,
        Self::DiskUsage,
        Self::CertificateExpiry,
        Self::LicenceState,
    ];

    /// The signal as a stable lower-case name, as the rule files spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HealthProbe => "health_probe",
            Self::ErrorRate => "error_rate",
            Self::QueueLag => "queue_lag",
            Self::JobFailures => "job_failures",
            Self::ReplicationLag => "replication_lag",
            Self::DiskUsage => "disk_usage",
            Self::CertificateExpiry => "certificate_expiry",
            Self::LicenceState => "licence_state",
        }
    }

    /// The signal in plain language, for a sentence.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::HealthProbe => "health probe",
            Self::ErrorRate => "error rate",
            Self::QueueLag => "queue lag",
            Self::JobFailures => "job failures",
            Self::ReplicationLag => "replication lag",
            Self::DiskUsage => "disk usage",
            Self::CertificateExpiry => "certificate expiry",
            Self::LicenceState => "licence state",
        }
    }

    /// Whether a value lies in this signal's domain.
    #[must_use]
    pub const fn accepts(self, value: i64) -> bool {
        match self {
            Self::HealthProbe => value == 0 || value == 1,
            Self::ErrorRate | Self::DiskUsage => value >= 0 && value <= 10_000,
            Self::LicenceState => value >= LICENCE_ACTIVE && value <= LICENCE_EXPIRED,
            Self::CertificateExpiry => true,
            Self::QueueLag | Self::JobFailures | Self::ReplicationLag => value >= 0,
        }
    }

    /// A value in the signal's unit, in plain language.
    #[must_use]
    pub fn format_value(self, value: i64) -> String {
        match self {
            Self::HealthProbe => {
                if value == 0 {
                    "failing".to_owned()
                } else {
                    "passing".to_owned()
                }
            }
            Self::ErrorRate | Self::DiskUsage => format_basis_points(value),
            Self::QueueLag | Self::ReplicationLag => {
                if value < 0 {
                    format!("{} behind", humanise_seconds(value.unsigned_abs()))
                } else {
                    humanise_seconds(value.unsigned_abs())
                }
            }
            Self::JobFailures => {
                if value == 1 {
                    "1 failure".to_owned()
                } else {
                    format!("{value} failures")
                }
            }
            Self::CertificateExpiry => {
                if value < 0 {
                    format!("expired {} ago", humanise_seconds(value.unsigned_abs()))
                } else {
                    format!("{} left", humanise_seconds(value.unsigned_abs()))
                }
            }
            Self::LicenceState => LicenceState::from_code(value).map_or_else(
                || format!("unknown ({value})"),
                |state| state.label().to_owned(),
            ),
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Render basis points as a percentage without floating point.
fn format_basis_points(value: i64) -> String {
    let whole = value / 100;
    let fraction = value % 100;
    if fraction == 0 {
        format!("{whole}%")
    } else if fraction % 10 == 0 {
        format!("{whole}.{}%", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}%")
    }
}

/// The licence state codes an observation may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenceState {
    /// The licence is valid.
    Active,
    /// The licence has expired and a grace period is running.
    Grace,
    /// The grace period is over.
    Expired,
}

/// The [`LicenceState::Active`] code.
pub const LICENCE_ACTIVE: i64 = 0;
/// The [`LicenceState::Grace`] code.
pub const LICENCE_GRACE: i64 = 1;
/// The [`LicenceState::Expired`] code.
pub const LICENCE_EXPIRED: i64 = 2;

impl LicenceState {
    /// The integer code an observation carries.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::Active => LICENCE_ACTIVE,
            Self::Grace => LICENCE_GRACE,
            Self::Expired => LICENCE_EXPIRED,
        }
    }

    /// The state for an integer code, when it is one.
    #[must_use]
    pub const fn from_code(value: i64) -> Option<Self> {
        match value {
            LICENCE_ACTIVE => Some(Self::Active),
            LICENCE_GRACE => Some(Self::Grace),
            LICENCE_EXPIRED => Some(Self::Expired),
            _ => None,
        }
    }

    /// The state in plain language.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Grace => "in grace",
            Self::Expired => "expired",
        }
    }
}

/// One signal reading for one subject at one time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// What the reading is about.
    pub subject: Subject,
    /// Which signal was read.
    pub signal: Signal,
    /// The value in the signal's own unit.
    pub value: i64,
    /// When the reading was taken.
    pub observed_at: Timestamp,
}

impl Observation {
    /// Builds and validates an observation.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidObservation`] when the value is outside the signal's domain.
    pub fn new(
        subject: Subject,
        signal: Signal,
        value: i64,
        observed_at: Timestamp,
    ) -> Result<Self, FleetHealthError> {
        if !signal.accepts(value) {
            return Err(FleetHealthError::InvalidObservation {
                reason: format!(
                    "{} value {value} is outside the range this signal accepts",
                    signal.label()
                ),
            });
        }
        Ok(Self {
            subject,
            signal,
            value,
            observed_at,
        })
    }

    /// Builds a licence-state observation; the constructor cannot fail because the state is
    /// already one of the three codes.
    #[must_use]
    pub fn licence(subject: Subject, state: LicenceState, observed_at: Timestamp) -> Self {
        Self {
            subject,
            signal: Signal::LicenceState,
            value: state.code(),
            observed_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_refuse_empty_and_odd_values() {
        assert!(TenantId::try_new("acme").is_ok());
        assert!(TenantId::try_new("").is_err());
        assert!(TenantId::try_new("a b").is_err());
        assert!(TenantId::try_new("x".repeat(129)).is_err());
        assert!(NodeId::try_new("ln-1:primary").is_ok());
    }

    #[test]
    fn signals_reject_values_outside_their_domain() {
        let at = Timestamp::from_secs(1);
        let tenant = Subject::Tenant(TenantId::try_new("acme").expect("tenant"));
        assert!(Observation::new(tenant.clone(), Signal::HealthProbe, 2, at).is_err());
        assert!(Observation::new(tenant.clone(), Signal::ErrorRate, -1, at).is_err());
        assert!(Observation::new(tenant.clone(), Signal::DiskUsage, 10_001, at).is_err());
        assert!(Observation::new(tenant.clone(), Signal::LicenceState, 3, at).is_err());
        assert!(Observation::new(tenant, Signal::CertificateExpiry, -60, at).is_ok());
    }

    #[test]
    fn basis_points_render_without_floats() {
        assert_eq!(format_basis_points(0), "0%");
        assert_eq!(format_basis_points(500), "5%");
        assert_eq!(format_basis_points(1_250), "12.5%");
        assert_eq!(format_basis_points(1_234), "12.34%");
    }

    #[test]
    fn subjects_describe_themselves_in_plain_language() {
        let tenant = Subject::Tenant(TenantId::try_new("acme").expect("tenant"));
        assert_eq!(tenant.display(), "tenant acme");
        assert_eq!(tenant.kind(), SubjectKind::Tenant);
    }
}
