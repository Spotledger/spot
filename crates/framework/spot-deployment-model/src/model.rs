//! The deployment model itself: all the facts, the canonical form and the digest, with the
//! epoch every fact entered under.
//!
//! The model answers one question for an agent: **how is this installation deployed right
//! now?** Edition, platform, topology, backend, cache and coordination, sync profile, runtime
//! version, installed apps, seam overrides, the last migration runs, connectivity, site
//! policies and the agent's own tier. Every fact entered the model with an epoch:
//! [`DeploymentModel::epoch`] moves forward once per event or refresh, and
//! [`DeploymentModel::sources`] records the version of each authoritative source the last
//! refresh read.
//!
//! The digest covers the facts and nothing else. Two models that describe the same deployment
//! have the same digest even when they reached it through different event orders, because the
//! digest is computed over [`DeploymentModel::canonical_json`] (sorted keys, integer numbers,
//! no whitespace). The epoch ledger and the applied-event window are version metadata, not
//! facts, and are deliberately outside the digest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_self_model::ChangeKind;

use crate::DeploymentError;
use crate::canonical::canonical_json;
use crate::error::{MAX_TEXT_BYTES, check_text};
use crate::facts::Facts;
pub use crate::types::*;

/// A live, versioned description of how this installation is deployed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentModel {
    /// The model's version. Every applied event or refresh moves it forward by one.
    pub epoch: u64,
    /// The licensed edition.
    pub edition: Edition,
    /// The platform the runtime runs on.
    pub platform: Platform,
    /// The shape, placement and nodes.
    pub topology: Topology,
    /// The database backend and its replicas.
    pub backend: Backend,
    /// The cache and coordination profile.
    pub coordination: Coordination,
    /// The sync profile.
    pub sync: SyncProfile,
    /// The Spotledger runtime version.
    pub runtime_version: String,
    /// The installed apps, by name.
    pub apps: BTreeMap<String, AppEntry>,
    /// The seam overrides the site has applied, sorted.
    pub overrides: Vec<Override>,
    /// The last migration runs, oldest first.
    pub migrations: Vec<MigrationRun>,
    /// Whether the hub is reachable and when the last sync succeeded.
    pub connectivity: Connectivity,
    /// The site policies.
    pub policies: SitePolicies,
    /// The tier of the agent reading the model.
    pub agent_tier: Tier,
    /// The version of each source the last refresh read.
    pub sources: BTreeMap<FactSourceKind, u64>,
    /// The recent epoch ledger, oldest first.
    pub ledger: Vec<EpochEntry>,
    /// Applied event ids with their epoch, for idempotent re-delivery.
    pub seen: BTreeMap<String, u64>,
}

impl DeploymentModel {
    /// Builds a model from complete facts at `epoch`.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::FactsIncomplete`] when a required fact is absent, and
    /// [`DeploymentError::Invalid`] when `epoch` is zero or a fact is malformed.
    pub fn from_facts(
        epoch: u64,
        facts: &Facts,
        sources: BTreeMap<FactSourceKind, u64>,
    ) -> Result<Self, DeploymentError> {
        if epoch == 0 {
            return Err(DeploymentError::invalid(
                "epoch",
                "the first model epoch is 1",
            ));
        }
        let model = Self {
            epoch,
            edition: required(&facts.edition, "edition")?,
            platform: required(&facts.platform, "platform")?,
            topology: required(&facts.topology, "topology")?,
            backend: required(&facts.backend, "backend")?,
            coordination: required(&facts.coordination, "coordination")?,
            sync: required(&facts.sync, "sync")?,
            runtime_version: required(&facts.runtime_version, "runtime_version")?,
            apps: required(&facts.apps, "apps")?,
            overrides: required(&facts.overrides, "overrides")?,
            migrations: required(&facts.migrations, "migrations")?,
            connectivity: required(&facts.connectivity, "connectivity")?,
            policies: required(&facts.policies, "policies")?,
            agent_tier: required(&facts.agent_tier, "agent_tier")?,
            sources,
            ledger: Vec::new(),
            seen: BTreeMap::new(),
        };
        model.validate()?;
        Ok(model)
    }

    /// Checks every fact's shape.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] naming the first field that is not usable.
    pub fn validate(&self) -> Result<(), DeploymentError> {
        if self.epoch == 0 {
            return Err(DeploymentError::invalid(
                "epoch",
                "the first model epoch is 1",
            ));
        }
        Edition::new(self.edition.as_str())?;
        Platform::new(self.platform.os, self.platform.version.clone())?;
        Topology::new(
            self.topology.mode,
            self.topology.placement,
            self.topology.nodes.clone(),
        )?;
        Backend::new(
            self.backend.engine,
            self.backend.version.clone(),
            self.backend.replicas.clone(),
        )?;
        check_text("runtime_version", &self.runtime_version, 128)?;
        for (name, app) in &self.apps {
            check_text("apps name", name, MAX_TEXT_BYTES)?;
            check_text("apps version", &app.version, 128)?;
        }
        for entry in &self.overrides {
            Override::new(
                entry.app.clone(),
                entry.target.clone(),
                entry.kind,
                entry.by.clone(),
            )?;
        }
        if self.migrations.len() > MIGRATION_HISTORY_LIMIT {
            return Err(DeploymentError::invalid(
                "migrations",
                format!("the model keeps at most {MIGRATION_HISTORY_LIMIT} runs"),
            ));
        }
        for run in &self.migrations {
            MigrationRun::new(run.run.clone(), run.outcome.clone(), run.applied_at)?;
        }
        if self.ledger.len() > EPOCH_LEDGER_LIMIT {
            return Err(DeploymentError::invalid(
                "ledger",
                format!("the ledger keeps at most {EPOCH_LEDGER_LIMIT} entries"),
            ));
        }
        if self
            .ledger
            .last()
            .is_some_and(|entry| entry.epoch > self.epoch)
        {
            return Err(DeploymentError::invalid(
                "ledger",
                "a ledger entry is newer than the model",
            ));
        }
        Ok(())
    }

    /// The facts as ground-truth-shaped data.
    #[must_use]
    pub fn facts(&self) -> Facts {
        Facts {
            edition: Some(self.edition.clone()),
            platform: Some(self.platform.clone()),
            topology: Some(self.topology.clone()),
            backend: Some(self.backend.clone()),
            coordination: Some(self.coordination.clone()),
            sync: Some(self.sync.clone()),
            runtime_version: Some(self.runtime_version.clone()),
            apps: Some(self.apps.clone()),
            overrides: Some(self.overrides.clone()),
            migrations: Some(self.migrations.clone()),
            connectivity: Some(self.connectivity.clone()),
            policies: Some(self.policies.clone()),
            agent_tier: Some(self.agent_tier),
        }
    }

    /// The canonical JSON of the facts (the epoch, ledger, source versions and applied-event
    /// window are excluded).
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Serialise`] when a fact cannot be represented.
    pub fn canonical_json(&self) -> Result<String, DeploymentError> {
        let view = FactsView {
            format: FORMAT,
            edition: &self.edition,
            platform: &self.platform,
            topology: &self.topology,
            backend: &self.backend,
            coordination: &self.coordination,
            sync: &self.sync,
            runtime_version: &self.runtime_version,
            apps: &self.apps,
            overrides: &self.overrides,
            migrations: &self.migrations,
            connectivity: &self.connectivity,
            policies: &self.policies,
            agent_tier: self.agent_tier,
        };
        canonical_json(&view)
    }

    /// The SHA-256 commitment to the facts.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Serialise`] when a fact cannot be represented.
    pub fn digest(&self) -> Result<Digest, DeploymentError> {
        Ok(Digest::of(self.canonical_json()?.as_bytes()))
    }

    /// One installed app's entry.
    #[must_use]
    pub fn app(&self, name: &str) -> Option<&AppEntry> {
        self.apps.get(name)
    }

    /// One node's entry.
    #[must_use]
    pub fn node(&self, name: &str) -> Option<&DeploymentNode> {
        self.topology.node(name)
    }

    /// The newest migration run, when the history is not empty.
    #[must_use]
    pub fn last_migration(&self) -> Option<&MigrationRun> {
        self.migrations.last()
    }

    /// Appends a ledger entry, keeping the ledger bounded, and sets the epoch.
    pub(crate) fn record(
        &mut self,
        epoch: u64,
        event: impl Into<String>,
        subject: impl Into<String>,
        change: ChangeKind,
        at: i64,
        evidence: Option<spot_self_model::EvidenceRef>,
    ) {
        self.epoch = epoch;
        self.ledger.push(EpochEntry {
            epoch,
            event: event.into(),
            subject: subject.into(),
            change,
            at,
            evidence,
        });
        if self.ledger.len() > EPOCH_LEDGER_LIMIT {
            let excess = self.ledger.len() - EPOCH_LEDGER_LIMIT;
            self.ledger.drain(0..excess);
        }
        self.prune_seen(epoch);
    }

    /// Forgets applied event ids that fall outside the idempotency window.
    pub(crate) fn prune_seen(&mut self, epoch: u64) {
        let floor = epoch.saturating_sub(SEEN_EVENT_LIMIT);
        self.seen.retain(|_, seen_epoch| *seen_epoch > floor);
    }

    /// Truncates the migration history to its limit, keeping the newest runs.
    pub(crate) fn trim_migrations(&mut self) {
        if self.migrations.len() > MIGRATION_HISTORY_LIMIT {
            let excess = self.migrations.len() - MIGRATION_HISTORY_LIMIT;
            self.migrations.drain(0..excess);
        }
    }
}

/// A required fact, or the typed "incomplete" error naming it.
pub(crate) fn required<T: Clone>(value: &Option<T>, field: &str) -> Result<T, DeploymentError> {
    value
        .clone()
        .ok_or_else(|| DeploymentError::FactsIncomplete {
            field: field.to_owned(),
        })
}

/// The digest projection: the facts, in one fixed order.
#[derive(Serialize)]
struct FactsView<'a> {
    format: &'static str,
    edition: &'a Edition,
    platform: &'a Platform,
    topology: &'a Topology,
    backend: &'a Backend,
    coordination: &'a Coordination,
    sync: &'a SyncProfile,
    runtime_version: &'a String,
    apps: &'a BTreeMap<String, AppEntry>,
    overrides: &'a Vec<Override>,
    migrations: &'a Vec<MigrationRun>,
    connectivity: &'a Connectivity,
    policies: &'a SitePolicies,
    agent_tier: Tier,
}
