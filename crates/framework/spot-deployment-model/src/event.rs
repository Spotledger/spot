//! Deployment events: the system changes that move the model forward, one epoch at a time.
//!
//! WP-AI-01's [`SystemEvent`] is the graph's event vocabulary. A deployment event is the
//! subset of system changes that changes how the installation is deployed, plus the licence,
//! connectivity and topology changes the control plane emits. [`DeploymentEvent::from_system_event`]
//! converts the overlapping ones, so the deployment model subscribes to the same stream the
//! self-model applies rather than a parallel one.
//!
//! # Epoch rules
//!
//! An event names the epoch it *produces*: applying it moves the model from `epoch - 1` to
//! `epoch`. [`apply`] is pure and classifies what it was given:
//!
//! - the event id was already applied — [`ApplyOutcome::Duplicate`], the model is unchanged;
//! - the event is older than the next epoch — [`ApplyOutcome::Stale`], ignored;
//! - another event already produced that epoch — [`ApplyOutcome::Conflict`];
//! - the event skips epochs — [`ApplyOutcome::Gap`], which tells the caller to refresh from
//!   its [`crate::FactSource`]s before applying anything newer;
//! - otherwise it applies and returns the new model.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_self_model::{ChangeKind, EvidenceRef, SystemEvent};

use crate::DeploymentError;
use crate::error::{MAX_TEXT_BYTES, check_text};
use crate::model::{
    AppEntry, AppState, Backend, Connectivity, Coordination, DeploymentModel, DeploymentNode,
    Edition, MigrationRun, NodeRole, Override, Platform, ReplayPolicy, SitePolicies, SyncProfile,
    Topology,
};

/// A change that moves the deployment model forward.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentEvent {
    /// The event's stable id, used to ignore a redelivery.
    pub id: String,
    /// The epoch this event produces.
    pub epoch: u64,
    /// When it happened, in unix microseconds supplied by the caller.
    pub at: i64,
    /// The record that proves it happened.
    pub evidence: EvidenceRef,
    /// What changed.
    pub kind: DeploymentEventKind,
}

/// What a deployment event changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentEventKind {
    /// A migration run finished.
    MigrationApplied {
        /// The run id.
        run: String,
        /// Its outcome.
        outcome: String,
    },
    /// An app was installed.
    AppInstalled {
        /// The app's name.
        app: String,
        /// The version installed.
        version: String,
    },
    /// An app was upgraded from one version to another.
    AppUpgraded {
        /// The app's name.
        app: String,
        /// The version it was on.
        from: String,
        /// The version it is on.
        to: String,
    },
    /// An app was removed.
    AppRemoved {
        /// The app's name.
        app: String,
    },
    /// A configuration fact changed.
    ConfigChanged {
        /// Which fact, and its new value.
        change: ConfigChange,
    },
    /// The topology was replaced (a node's roles moved, or the placement changed).
    TopologyChanged {
        /// The new topology.
        topology: Topology,
    },
    /// A node joined the deployment.
    NodeJoined {
        /// The node's name.
        node: String,
        /// The roles it serves.
        roles: BTreeSet<NodeRole>,
    },
    /// A node left the deployment.
    NodeLeft {
        /// The node's name.
        node: String,
    },
    /// An incident was resolved. The facts do not change; the model advances one epoch so a
    /// decision taken afterwards can show it was taken after the resolution.
    IncidentResolved {
        /// The incident's id.
        incident: String,
        /// What resolved it, in one non-sensitive sentence.
        detail: String,
    },
    /// The licence changed the edition.
    LicenceChanged {
        /// The new edition.
        edition: Edition,
    },
    /// Connectivity to the hub changed.
    ConnectivityChanged {
        /// The new reading.
        connectivity: Connectivity,
    },
}

/// A configuration fact and its new value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChange {
    /// The edition changed.
    Edition(Edition),
    /// The platform changed (a host upgrade or a move).
    Platform(Platform),
    /// The backend changed (a version upgrade, a replica added or promoted).
    Backend(Backend),
    /// The cache and coordination profile changed.
    Coordination(Coordination),
    /// The sync profile changed.
    Sync(SyncProfile),
    /// The runtime version changed.
    RuntimeVersion(String),
    /// The site policies changed.
    Policies(SitePolicies),
    /// The agent's tier changed.
    AgentTier(Tier),
}

/// What applying an event did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The event applied; this is the new model.
    Applied {
        /// The model at the new epoch.
        model: Box<DeploymentModel>,
        /// The new epoch.
        epoch: u64,
    },
    /// The same event id was already applied; nothing changed.
    Duplicate {
        /// The model's epoch.
        epoch: u64,
    },
    /// The event is older than the model's next epoch; it was ignored.
    Stale {
        /// The model's epoch.
        model_epoch: u64,
        /// The ignored event's epoch.
        event_epoch: u64,
    },
    /// Another event already produced this epoch.
    Conflict {
        /// The epoch both events claim.
        epoch: u64,
        /// The id of the event that produced it.
        applied: String,
    },
    /// The event skips epochs. Refresh from the fact sources, then re-apply.
    Gap {
        /// The epoch the model expected next.
        expected: u64,
        /// The epoch the event names.
        received: u64,
    },
}

impl ApplyOutcome {
    /// The model's epoch after the call (unchanged for every non-applied outcome).
    #[must_use]
    pub fn epoch(&self, model: &DeploymentModel) -> u64 {
        match self {
            Self::Applied { epoch, .. } => *epoch,
            Self::Duplicate { epoch }
            | Self::Conflict { epoch, .. }
            | Self::Stale {
                model_epoch: epoch, ..
            } => *epoch,
            Self::Gap { .. } => model.epoch,
        }
    }

    /// Whether a refresh is needed before the model can move forward.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        matches!(self, Self::Gap { .. })
    }
}

impl DeploymentEvent {
    /// An event, checked before it can reach a model.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::EventInvalid`] when the id is empty or too long, the epoch is zero,
    /// or the evidence pointer names no record.
    pub fn new(
        id: impl Into<String>,
        epoch: u64,
        at: i64,
        evidence: EvidenceRef,
        kind: DeploymentEventKind,
    ) -> Result<Self, DeploymentError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(DeploymentError::EventInvalid {
                reason: "the event has no id".to_owned(),
            });
        }
        check_text("event id", &id, MAX_TEXT_BYTES)
            .map_err(|error| Self::as_event_error(&error))?;
        if epoch == 0 {
            return Err(DeploymentError::EventInvalid {
                reason: "the first event epoch is 1".to_owned(),
            });
        }
        if !evidence.is_complete() {
            return Err(DeploymentError::EventInvalid {
                reason: "the event carries no evidence record".to_owned(),
            });
        }
        Ok(Self {
            id,
            epoch,
            at,
            evidence,
            kind,
        })
    }

    fn as_event_error(error: &DeploymentError) -> DeploymentError {
        match error {
            DeploymentError::Invalid { field, reason } => DeploymentError::EventInvalid {
                reason: format!("{field}: {reason}"),
            },
            other => other.clone(),
        }
    }

    /// What the event is about, for the epoch ledger.
    #[must_use]
    pub fn subject(&self) -> String {
        match &self.kind {
            DeploymentEventKind::MigrationApplied { run, .. } => format!("migration:{run}"),
            DeploymentEventKind::AppInstalled { app, .. }
            | DeploymentEventKind::AppUpgraded { app, .. }
            | DeploymentEventKind::AppRemoved { app } => format!("app:{app}"),
            DeploymentEventKind::ConfigChanged { change } => match change {
                ConfigChange::Edition(_) => "config:edition".to_owned(),
                ConfigChange::Platform(_) => "config:platform".to_owned(),
                ConfigChange::Backend(_) => "config:backend".to_owned(),
                ConfigChange::Coordination(_) => "config:coordination".to_owned(),
                ConfigChange::Sync(_) => "config:sync".to_owned(),
                ConfigChange::RuntimeVersion(_) => "config:runtime_version".to_owned(),
                ConfigChange::Policies(_) => "config:policies".to_owned(),
                ConfigChange::AgentTier(_) => "config:agent_tier".to_owned(),
            },
            DeploymentEventKind::TopologyChanged { .. } => "topology".to_owned(),
            DeploymentEventKind::NodeJoined { node, .. }
            | DeploymentEventKind::NodeLeft { node } => format!("node:{node}"),
            DeploymentEventKind::IncidentResolved { incident, .. } => {
                format!("incident:{incident}")
            }
            DeploymentEventKind::LicenceChanged { .. } => "licence".to_owned(),
            DeploymentEventKind::ConnectivityChanged { .. } => "connectivity".to_owned(),
        }
    }

    /// The kind of change, for the epoch ledger.
    #[must_use]
    pub fn change(&self) -> ChangeKind {
        match &self.kind {
            DeploymentEventKind::MigrationApplied { .. }
            | DeploymentEventKind::AppInstalled { .. } => ChangeKind::Observed,
            DeploymentEventKind::AppUpgraded { .. } => ChangeKind::Promoted,
            DeploymentEventKind::AppRemoved { .. } => ChangeKind::Removed,
            DeploymentEventKind::ConfigChanged { .. }
            | DeploymentEventKind::TopologyChanged { .. }
            | DeploymentEventKind::NodeJoined { .. }
            | DeploymentEventKind::NodeLeft { .. }
            | DeploymentEventKind::LicenceChanged { .. } => ChangeKind::Updated,
            DeploymentEventKind::IncidentResolved { .. } => ChangeKind::Recovered,
            DeploymentEventKind::ConnectivityChanged { connectivity } => {
                if connectivity.hub_reachable {
                    ChangeKind::Recovered
                } else {
                    ChangeKind::Failed
                }
            }
        }
    }

    /// Converts a self-model system event into a deployment event, when the change is one the
    /// deployment model tracks.
    ///
    /// The graph's own events describe metadata, scripts, permissions and jobs; those do not
    /// change how the installation is deployed. A migration, an app install and an app removal
    /// do, and they keep the system event's evidence record.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::EventInvalid`] when the conversion would produce an event without an
    /// id or evidence.
    pub fn from_system_event(
        id: impl Into<String>,
        epoch: u64,
        event: &SystemEvent,
    ) -> Result<Option<Self>, DeploymentError> {
        let kind = match event {
            SystemEvent::MigrationRecorded { run, outcome, .. } => {
                DeploymentEventKind::MigrationApplied {
                    run: run.clone(),
                    outcome: outcome.clone(),
                }
            }
            SystemEvent::AppInstalled { app, version, .. } => DeploymentEventKind::AppInstalled {
                app: app.clone(),
                version: version.clone(),
            },
            SystemEvent::AppUninstalled { app, .. } => {
                DeploymentEventKind::AppRemoved { app: app.clone() }
            }
            _ => return Ok(None),
        };
        Ok(Some(Self::new(
            id,
            epoch,
            event.at(),
            event.evidence().clone(),
            kind,
        )?))
    }
}

/// Applies one event to a model, purely.
///
/// The model is cloned, the change applied, the whole thing validated, and only then returned:
/// a refused event leaves no half-applied model behind. Nothing reads the clock — `event.at`
/// is the only time involved.
///
/// # Errors
///
/// [`DeploymentError::EventInvalid`] when the event cannot apply (an upgrade of an app that is
/// not installed, a node leaving when it is not there), and [`DeploymentError::Invalid`] when
/// the resulting model would be malformed.
pub fn apply(
    model: &DeploymentModel,
    event: &DeploymentEvent,
) -> Result<ApplyOutcome, DeploymentError> {
    // A caller can build the struct directly (its fields are public data), so the checks the
    // constructor makes are repeated at the one place that lets an event reach a model.
    if event.id.trim().is_empty() {
        return Err(DeploymentError::EventInvalid {
            reason: "the event has no id".to_owned(),
        });
    }
    if event.epoch == 0 {
        return Err(DeploymentError::EventInvalid {
            reason: "the first event epoch is 1".to_owned(),
        });
    }
    if !event.evidence.is_complete() {
        return Err(DeploymentError::EventInvalid {
            reason: "the event carries no evidence record".to_owned(),
        });
    }
    if let Some(seen) = model.seen.get(&event.id) {
        return Ok(ApplyOutcome::Duplicate { epoch: *seen });
    }
    // An older epoch is stale: the model has already moved past it, whether an event or a
    // refresh produced the epoch it names.
    if event.epoch < model.epoch {
        return Ok(ApplyOutcome::Stale {
            model_epoch: model.epoch,
            event_epoch: event.epoch,
        });
    }
    // The current epoch is already produced by someone else (the id was not in `seen`, or this
    // would be a duplicate): two events cannot share one epoch.
    if event.epoch == model.epoch {
        let applied = model
            .ledger
            .iter()
            .rev()
            .find(|entry| entry.epoch == event.epoch)
            .map_or_else(|| "refresh".to_owned(), |entry| entry.event.clone());
        return Ok(ApplyOutcome::Conflict {
            epoch: event.epoch,
            applied,
        });
    }
    let expected = model.epoch.saturating_add(1);
    if event.epoch > expected {
        return Ok(ApplyOutcome::Gap {
            expected,
            received: event.epoch,
        });
    }

    let mut next = model.clone();
    match &event.kind {
        DeploymentEventKind::MigrationApplied { run, outcome } => {
            let record = MigrationRun::new(run.clone(), outcome.clone(), event.at)?;
            match next.migrations.iter_mut().find(|entry| &entry.run == run) {
                Some(existing) => *existing = record,
                None => next.migrations.push(record),
            }
            next.migrations.sort();
            next.trim_migrations();
        }
        DeploymentEventKind::AppInstalled { app, version } => {
            next.apps.insert(
                app.clone(),
                AppEntry::new(version.clone(), AppState::Installed)?,
            );
        }
        DeploymentEventKind::AppUpgraded { app, from, to } => {
            let entry = next
                .apps
                .get(app)
                .ok_or_else(|| DeploymentError::EventInvalid {
                    reason: format!("upgrade of app `{app}`, which is not installed"),
                })?;
            if &entry.version != from {
                return Err(DeploymentError::EventInvalid {
                    reason: format!("app `{app}` is on version {}, not {from}", entry.version),
                });
            }
            let state = entry.state;
            next.apps
                .insert(app.clone(), AppEntry::new(to.clone(), state)?);
        }
        DeploymentEventKind::AppRemoved { app } => {
            if next.apps.remove(app).is_none() {
                return Err(DeploymentError::EventInvalid {
                    reason: format!("removal of app `{app}`, which is not installed"),
                });
            }
        }
        DeploymentEventKind::ConfigChanged { change } => match change {
            ConfigChange::Edition(edition) => next.edition = edition.clone(),
            ConfigChange::Platform(platform) => next.platform = platform.clone(),
            ConfigChange::Backend(backend) => next.backend = backend.clone(),
            ConfigChange::Coordination(coordination) => next.coordination = coordination.clone(),
            ConfigChange::Sync(sync) => next.sync = sync.clone(),
            ConfigChange::RuntimeVersion(version) => next.runtime_version = version.clone(),
            ConfigChange::Policies(policies) => next.policies = policies.clone(),
            ConfigChange::AgentTier(tier) => next.agent_tier = *tier,
        },
        DeploymentEventKind::TopologyChanged { topology } => next.topology = topology.clone(),
        DeploymentEventKind::NodeJoined { node, roles } => {
            next.topology
                .nodes
                .insert(node.clone(), DeploymentNode::new(roles.iter().copied())?);
            if next.topology.nodes.len() > 1 {
                next.topology.mode = crate::model::TopologyMode::Cluster;
            }
        }
        DeploymentEventKind::NodeLeft { node } => {
            if next.topology.nodes.remove(node).is_none() {
                return Err(DeploymentError::EventInvalid {
                    reason: format!("node `{node}` left, but the topology has no such node"),
                });
            }
            if next.topology.nodes.len() == 1 {
                next.topology.mode = crate::model::TopologyMode::Standalone;
            }
        }
        DeploymentEventKind::IncidentResolved { .. } => {}
        DeploymentEventKind::LicenceChanged { edition } => next.edition = edition.clone(),
        DeploymentEventKind::ConnectivityChanged { connectivity } => {
            next.connectivity = connectivity.clone();
        }
    }
    next.validate()?;
    next.seen.insert(event.id.clone(), event.epoch);
    next.record(
        event.epoch,
        event.id.clone(),
        event.subject(),
        event.change(),
        event.at,
        Some(event.evidence.clone()),
    );
    Ok(ApplyOutcome::Applied {
        model: Box::new(next),
        epoch: event.epoch,
    })
}

/// The overrides a set of facts carries, sorted for a stable digest.
#[must_use]
pub fn sorted_overrides(mut overrides: Vec<Override>) -> Vec<Override> {
    overrides.sort();
    overrides.dedup();
    overrides
}

/// Parses an override kind's text form.
///
/// # Errors
///
/// [`DeploymentError::Invalid`] for any other text.
pub fn parse_override_kind(text: &str) -> Result<crate::model::OverrideKind, DeploymentError> {
    use crate::model::OverrideKind;
    match text {
        "before" => Ok(OverrideKind::Before),
        "after" => Ok(OverrideKind::After),
        "wrap" => Ok(OverrideKind::Wrap),
        "replace" => Ok(OverrideKind::Replace),
        "disable" => Ok(OverrideKind::Disable),
        other => Err(DeploymentError::invalid(
            "overrides kind",
            format!("`{other}` is not an override kind"),
        )),
    }
}

/// Parses a replay policy's text form.
///
/// # Errors
///
/// [`DeploymentError::Invalid`] for any other text.
pub fn parse_replay_policy(text: &str) -> Result<ReplayPolicy, DeploymentError> {
    match text {
        "off" => Ok(ReplayPolicy::Off),
        "auto" => Ok(ReplayPolicy::Auto),
        "always" => Ok(ReplayPolicy::Always),
        other => Err(DeploymentError::invalid(
            "policies replay",
            format!("`{other}` is not a replay policy"),
        )),
    }
}
