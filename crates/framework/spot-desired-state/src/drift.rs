//! Drift: what the desired state says, what the agent observed, and how to fix it.
//!
//! [`detect_drift`] is deterministic and pure: it takes the validated desired fleet and the
//! agent reports passed in, compares settings, package versions and service states, and returns
//! one [`Drift`] per difference, sorted by server and then subject. Every drift carries the
//! risk from its settings spec (or the package/service default) and the typed catalogue
//! operations that would remove it.
//!
//! An observed setting the agent did not report is at its schema default, so a drift is only
//! reported when the effective desired value differs from that default. Extra packages and
//! services the desired state does not mention are not drifts: a desired state governs what it
//! names, it does not imply the absence of everything else.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_fleet_rollout::NodeId;
use spot_mgmt_ops::{ParamValue, Risk};

use crate::exemption::ExemptionBook;
use crate::ids::SettingKey;
use crate::plan::{TypedOperation, operation};
use crate::state::{
    DesiredFleet, Liveness, ObservedState, PACKAGE_RISK, SERVICE_RISK, SILENT_RISK, liveness,
};

/// What drifted on one server.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DriftSubject {
    /// One setting.
    Setting(SettingKey),
    /// One package's installed version.
    Package(String),
    /// Whether one service is running now.
    ServiceRunning(String),
    /// Whether one service is enabled at boot.
    ServiceEnabled(String),
    /// The server is silent: its agent has not reported within the deadline (or never has).
    /// The drift's `actual` is the last-seen time in microseconds, or `None` when never seen.
    Silent,
}

impl DriftSubject {
    /// A short label for reports.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Setting(key) => format!("setting {key}"),
            Self::Package(name) => format!("package {name}"),
            Self::ServiceRunning(name) => format!("service {name} running"),
            Self::ServiceEnabled(name) => format!("service {name} enabled"),
            Self::Silent => "agent silent".to_owned(),
        }
    }

    /// Whether this is a package drift.
    #[must_use]
    pub fn is_package(&self) -> bool {
        matches!(self, Self::Package(_))
    }

    /// Whether this is a service drift.
    #[must_use]
    pub fn is_service(&self) -> bool {
        matches!(self, Self::ServiceRunning(_) | Self::ServiceEnabled(_))
    }

    /// The setting key, when the subject is a setting.
    #[must_use]
    pub fn setting(&self) -> Option<&SettingKey> {
        match self {
            Self::Setting(key) => Some(key),
            Self::Package(_) | Self::ServiceRunning(_) | Self::ServiceEnabled(_) | Self::Silent => {
                None
            }
        }
    }
}

/// One difference between a server's desired and observed state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    /// The server.
    pub server: NodeId,
    /// The server's group.
    pub group: String,
    /// What drifted.
    pub subject: DriftSubject,
    /// The risk of changing it.
    pub risk: Risk,
    /// What the desired state says, when it says something.
    pub desired: Option<ParamValue>,
    /// What the agent observed; `None` when the agent reported the setting absent.
    pub actual: Option<ParamValue>,
    /// The typed catalogue operations that remove the drift, in application order.
    pub remediation: Vec<TypedOperation>,
}

impl Drift {
    /// The setting key, when the drift is a setting.
    #[must_use]
    pub fn setting(&self) -> Option<&SettingKey> {
        self.subject.setting()
    }

    /// Whether the server is silent (unknown), not drifted in a known way.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        self.subject == DriftSubject::Silent
    }

    /// When a silent server was last seen (unix microseconds); `None` when never seen or when
    /// the drift is not a silence.
    #[must_use]
    pub fn silent_since(&self) -> Option<i64> {
        match (&self.subject, &self.actual) {
            (DriftSubject::Silent, Some(ParamValue::Int(seen))) => Some(*seen),
            _ => None,
        }
    }

    /// A secret-free sentence for the console.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.is_silent() {
            return match self.silent_since() {
                Some(seen) => format!(
                    "agent silent on {}: last seen at {seen} (unix microseconds), state unknown",
                    self.server
                ),
                None => format!("agent silent on {}: never seen, state unknown", self.server),
            };
        }
        format!(
            "{} on {}: desired {}, observed {}",
            self.subject.label(),
            self.server,
            describe_value(self.desired.as_ref()),
            describe_value(self.actual.as_ref())
        )
    }
}

/// A short value description for reports, never used for a secret (secrets are not compared).
fn describe_value(value: Option<&ParamValue>) -> String {
    match value {
        None => "unset".to_owned(),
        Some(ParamValue::Bool(value)) => value.to_string(),
        Some(ParamValue::Int(value)) => value.to_string(),
        Some(ParamValue::Text(value)) => format!("'{value}'"),
        Some(ParamValue::Duration(seconds)) => format!("{seconds}s"),
        Some(ParamValue::List(values)) => format!("{} item(s)", values.len()),
    }
}

/// Detects drift on every server the desired state covers, deterministically grouped by server.
///
/// A server whose agent has not reported within `deadline` microseconds of `now`, or never has,
/// yields one [`DriftSubject::Silent`] drift (state unknown) and is not compared: a stale
/// report is not evidence that the server is in line. Time is an argument; see
/// [`crate::state::liveness`] for the exact rule, which fails closed.
#[must_use]
pub fn detect_drift(
    desired: &DesiredFleet,
    observed: &dyn ObservedState,
    now: i64,
    deadline: i64,
) -> Vec<Drift> {
    let mut drifts = Vec::new();
    for server in desired.servers() {
        let Some(group) = desired.group_of(&server) else {
            continue;
        };
        let snapshot = match liveness(observed, &server, now, deadline) {
            Liveness::Silent { since } => {
                drifts.push(Drift {
                    server: server.clone(),
                    group: group.clone(),
                    subject: DriftSubject::Silent,
                    risk: SILENT_RISK,
                    desired: None,
                    actual: since.map(ParamValue::Int),
                    remediation: Vec::new(),
                });
                continue;
            }
            Liveness::Fresh => match observed.snapshot(&server) {
                Some(snapshot) => snapshot,
                None => continue,
            },
        };
        let effective = desired.effective(&server, None);
        let operations = desired.operations();

        for spec in desired.registry().specs() {
            if spec.secret || spec.read_only {
                continue;
            }
            let Ok(key) = SettingKey::new(spec.key.clone()) else {
                continue;
            };
            let Some(desired_value) = effective.settings.get(&key) else {
                continue;
            };
            let reported = snapshot.settings.get(&key);
            let actual_effective = reported.cloned().or_else(|| spec.default.clone());
            if actual_effective.as_ref() == Some(desired_value) {
                continue;
            }
            drifts.push(Drift {
                server: server.clone(),
                group: group.clone(),
                subject: DriftSubject::Setting(key.clone()),
                risk: spec.risk,
                desired: Some(desired_value.clone()),
                actual: reported.cloned(),
                remediation: vec![operation(
                    &operations.set_setting,
                    [
                        ("key", ParamValue::Text(key.as_str().to_owned())),
                        ("value", desired_value.clone()),
                    ],
                )],
            });
        }

        for (name, desired_version) in &effective.packages {
            if snapshot.packages.get(name) == Some(desired_version) {
                continue;
            }
            drifts.push(Drift {
                server: server.clone(),
                group: group.clone(),
                subject: DriftSubject::Package(name.clone()),
                risk: PACKAGE_RISK,
                desired: Some(ParamValue::Text(desired_version.clone())),
                actual: snapshot
                    .packages
                    .get(name)
                    .map(|version| ParamValue::Text(version.clone())),
                remediation: vec![operation(
                    &operations.install_package,
                    [
                        ("name", ParamValue::Text(name.clone())),
                        ("version", ParamValue::Text(desired_version.clone())),
                    ],
                )],
            });
        }

        for (name, desired_state) in &effective.services {
            let actual = snapshot.services.get(name);
            if actual.map(|state| state.running) != Some(desired_state.running) {
                drifts.push(Drift {
                    server: server.clone(),
                    group: group.clone(),
                    subject: DriftSubject::ServiceRunning(name.clone()),
                    risk: SERVICE_RISK,
                    desired: Some(ParamValue::Bool(desired_state.running)),
                    actual: actual.map(|state| ParamValue::Bool(state.running)),
                    remediation: vec![operation(
                        if desired_state.running {
                            &operations.start_service
                        } else {
                            &operations.stop_service
                        },
                        [("name", ParamValue::Text(name.clone()))],
                    )],
                });
            }
            if actual.map(|state| state.enabled) != Some(desired_state.enabled) {
                drifts.push(Drift {
                    server: server.clone(),
                    group: group.clone(),
                    subject: DriftSubject::ServiceEnabled(name.clone()),
                    risk: SERVICE_RISK,
                    desired: Some(ParamValue::Bool(desired_state.enabled)),
                    actual: actual.map(|state| ParamValue::Bool(state.enabled)),
                    remediation: vec![operation(
                        if desired_state.enabled {
                            &operations.enable_service
                        } else {
                            &operations.disable_service
                        },
                        [("name", ParamValue::Text(name.clone()))],
                    )],
                });
            }
        }
    }
    drifts.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then_with(|| left.subject.cmp(&right.subject))
    });
    drifts
}

/// [`detect_drift`] with acknowledged drifts removed.
///
/// An exemption is active while `now < expires_at`; at the expiry instant the drift alarms
/// again. A silent server is never exempted: acknowledging "unknown" would let a dead server
/// read as healthy.
#[must_use]
pub fn detect_unexempted(
    desired: &DesiredFleet,
    observed: &dyn ObservedState,
    exemptions: &ExemptionBook,
    now: i64,
    deadline: i64,
) -> Vec<Drift> {
    let mut drifts = detect_drift(desired, observed, now, deadline);
    drifts.retain(|drift| {
        drift.is_silent() || !exemptions.is_exempt(&drift.server, &drift.subject, now)
    });
    drifts
}

/// One server's drifts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerDrift {
    /// The server.
    pub server: NodeId,
    /// The server's group.
    pub group: String,
    /// Its drifts, in subject order.
    pub drifts: Vec<Drift>,
}

/// Groups drifts by server, in server order.
#[must_use]
pub fn group_by_server(drifts: &[Drift]) -> Vec<ServerDrift> {
    let mut grouped: BTreeMap<NodeId, ServerDrift> = BTreeMap::new();
    for drift in drifts {
        let entry = grouped
            .entry(drift.server.clone())
            .or_insert_with(|| ServerDrift {
                server: drift.server.clone(),
                group: drift.group.clone(),
                drifts: Vec::new(),
            });
        entry.drifts.push(drift.clone());
    }
    for entry in grouped.values_mut() {
        entry
            .drifts
            .sort_by(|left, right| left.subject.cmp(&right.subject));
    }
    grouped.into_values().collect()
}
