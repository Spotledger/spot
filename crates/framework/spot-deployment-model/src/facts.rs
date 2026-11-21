//! Ground truth: the `FactSource` trait, fact merging, refresh and verification.
//!
//! The model is never the source of truth. It is a cache of what the authoritative sources
//! said, with the epoch each refresh read. [`FactSource`] is how the self-check reads the
//! truth again: [`refresh`] builds a new model from it, and [`verify`] compares the model
//! with it and reports every [`Mismatch`]. The periodic self-check (in
//! [`crate::ModelHandle::self_check`]) refreshes on the first mismatch, so the model cannot
//! stay quiet about a deployment that moved.

use std::collections::BTreeMap;
use std::fmt;

use spot_agent_tiers::Tier;
use spot_self_model::{ChangeKind, EvidenceRef};

use crate::DeploymentError;
use crate::model::{
    AppEntry, Backend, Connectivity, Coordination, DeploymentModel, Edition, FactSourceKind,
    MigrationRun, Override, Platform, SitePolicies, SyncProfile, Topology,
};

/// What one fact source says, when it says anything.
///
/// Every field is optional because a source owns a slice of the truth: the installer knows
/// the apps, the runtime knows the topology and backend, fleet health knows connectivity.
/// [`refresh`] merges the slices and refuses two sources that disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    /// The licensed edition.
    pub edition: Option<Edition>,
    /// The platform.
    pub platform: Option<Platform>,
    /// The topology.
    pub topology: Option<Topology>,
    /// The backend.
    pub backend: Option<Backend>,
    /// The cache and coordination profile.
    pub coordination: Option<Coordination>,
    /// The sync profile.
    pub sync: Option<SyncProfile>,
    /// The runtime version.
    pub runtime_version: Option<String>,
    /// The installed apps.
    pub apps: Option<BTreeMap<String, AppEntry>>,
    /// The seam overrides.
    pub overrides: Option<Vec<Override>>,
    /// The migration history.
    pub migrations: Option<Vec<MigrationRun>>,
    /// Connectivity.
    pub connectivity: Option<Connectivity>,
    /// The site policies.
    pub policies: Option<SitePolicies>,
    /// The agent's tier.
    pub agent_tier: Option<Tier>,
}

impl Facts {
    /// Empty facts.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Merges another source's facts, refusing a disagreement.
    ///
    /// Equal values merge silently, which is what makes the merge order-independent: two
    /// sources that read the same truth agree, and two that read different truths are a
    /// deployment problem worth a typed error.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::FactsConflict`] naming the first fact the two sources disagree on.
    pub fn merge(&mut self, other: Facts) -> Result<(), DeploymentError> {
        merge_field("edition", &mut self.edition, other.edition)?;
        merge_field("platform", &mut self.platform, other.platform)?;
        merge_field("topology", &mut self.topology, other.topology)?;
        merge_field("backend", &mut self.backend, other.backend)?;
        merge_field("coordination", &mut self.coordination, other.coordination)?;
        merge_field("sync", &mut self.sync, other.sync)?;
        merge_field(
            "runtime_version",
            &mut self.runtime_version,
            other.runtime_version,
        )?;
        merge_field("apps", &mut self.apps, other.apps)?;
        merge_field("overrides", &mut self.overrides, other.overrides)?;
        merge_field("migrations", &mut self.migrations, other.migrations)?;
        merge_field("connectivity", &mut self.connectivity, other.connectivity)?;
        merge_field("policies", &mut self.policies, other.policies)?;
        merge_field("agent_tier", &mut self.agent_tier, other.agent_tier)?;
        Ok(())
    }

    /// The name of the first fact still missing, if any.
    #[must_use]
    pub fn missing(&self) -> Option<&'static str> {
        if self.edition.is_none() {
            return Some("edition");
        }
        if self.platform.is_none() {
            return Some("platform");
        }
        if self.topology.is_none() {
            return Some("topology");
        }
        if self.backend.is_none() {
            return Some("backend");
        }
        if self.coordination.is_none() {
            return Some("coordination");
        }
        if self.sync.is_none() {
            return Some("sync");
        }
        if self.runtime_version.is_none() {
            return Some("runtime_version");
        }
        if self.apps.is_none() {
            return Some("apps");
        }
        if self.overrides.is_none() {
            return Some("overrides");
        }
        if self.migrations.is_none() {
            return Some("migrations");
        }
        if self.connectivity.is_none() {
            return Some("connectivity");
        }
        if self.policies.is_none() {
            return Some("policies");
        }
        if self.agent_tier.is_none() {
            return Some("agent_tier");
        }
        None
    }

    /// Whether every required fact is present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing().is_none()
    }
}

fn merge_field<T: PartialEq>(
    field: &str,
    current: &mut Option<T>,
    other: Option<T>,
) -> Result<(), DeploymentError> {
    match (current.as_ref(), other) {
        (Some(left), Some(right)) if left != &right => Err(DeploymentError::FactsConflict {
            field: field.to_owned(),
        }),
        (None, Some(value)) => {
            *current = Some(value);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// One authoritative source of deployment facts.
///
/// Implementations are reads. They must not read the wall clock (the caller passes time into
/// [`refresh`]) and must not return a secret: a fact source describes the deployment, it
/// never carries credentials, connection strings or keys.
pub trait FactSource {
    /// Which source this is.
    fn source(&self) -> FactSourceKind;

    /// The source's own version (an installer generation, a settings revision, a telemetry
    /// sequence). The model records it with the facts, so no fact is ever cached without one.
    fn epoch(&self) -> u64;

    /// Reads the source's slice of the truth.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::FactsUnreadable`] when the source cannot be read, or
    /// [`DeploymentError::Invalid`] when what it read is malformed.
    fn facts(&self) -> Result<Facts, DeploymentError>;

    /// The record that proves the facts were read, when the source keeps one.
    fn evidence(&self) -> Option<EvidenceRef> {
        None
    }
}

/// Reads every source, merges their facts and returns the merged truth with each source's
/// version.
///
/// # Errors
///
/// [`DeploymentError::FactsUnreadable`] or [`DeploymentError::FactsConflict`] when the sources
/// disagree, and [`DeploymentError::FactsIncomplete`] when the merged truth lacks a fact.
pub fn read(
    sources: &[&dyn FactSource],
) -> Result<(Facts, BTreeMap<FactSourceKind, u64>), DeploymentError> {
    let mut facts = Facts::new();
    let mut versions: BTreeMap<FactSourceKind, u64> = BTreeMap::new();
    for source in sources {
        let kind = source.source();
        let slice = source.facts()?;
        facts.merge(slice)?;
        let epoch = source.epoch();
        versions
            .entry(kind)
            .and_modify(|seen| *seen = (*seen).max(epoch))
            .or_insert(epoch);
    }
    if let Some(field) = facts.missing() {
        return Err(DeploymentError::FactsIncomplete {
            field: field.to_owned(),
        });
    }
    Ok((facts, versions))
}

/// Builds a fresh model from the sources, moving the epoch forward by one.
///
/// Every fact enters with the epoch of the source that supplied it and the new model epoch:
/// the model never caches a fact without one.
///
/// # Errors
///
/// Any error [`read`] raises, plus [`DeploymentError::Invalid`] when the merged facts cannot
/// form a model.
pub fn refresh(
    previous: Option<&DeploymentModel>,
    sources: &[&dyn FactSource],
    at: i64,
) -> Result<DeploymentModel, DeploymentError> {
    let (facts, versions) = read(sources)?;
    let evidence = sources.iter().find_map(|source| source.evidence());
    build(previous, &facts, versions, evidence, at)
}

/// Builds the next model from facts that were already read.
///
/// [`refresh`] is this function after [`read`]; the self-check uses it so one timer tick reads
/// its sources once, verifies, and refreshes from the same reading rather than racing a second
/// read against a deployment that is changing under it.
///
/// # Errors
///
/// [`DeploymentError::FactsIncomplete`] when a required fact is absent, and
/// [`DeploymentError::Invalid`] when a fact is malformed.
pub fn build(
    previous: Option<&DeploymentModel>,
    facts: &Facts,
    versions: BTreeMap<FactSourceKind, u64>,
    evidence: Option<EvidenceRef>,
    at: i64,
) -> Result<DeploymentModel, DeploymentError> {
    let epoch = previous.map_or(1, |model| model.epoch.saturating_add(1));
    let mut model = DeploymentModel::from_facts(epoch, facts, versions)?;
    model.record(
        epoch,
        "refresh",
        "deployment",
        ChangeKind::Observed,
        at,
        evidence,
    );
    Ok(model)
}

/// One difference between the model and the ground truth.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mismatch {
    /// The fact that differs.
    pub field: String,
    /// What the source says.
    pub expected: String,
    /// What the model holds.
    pub found: String,
}

impl Mismatch {
    /// A difference, rendered from the two values' `Debug` form. Neither value is a secret:
    /// a fact source never carries one.
    #[must_use]
    pub fn new<F: fmt::Debug, T: fmt::Debug>(field: &str, expected: &F, found: &T) -> Self {
        Self {
            field: field.to_owned(),
            expected: format!("{expected:?}"),
            found: format!("{found:?}"),
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: expected {}, found {}",
            self.field, self.expected, self.found
        )
    }
}

/// Compares the model with the ground truth, fact by fact.
///
/// Only facts the source supplied are compared, so a caller can verify one slice (say, the
/// installed apps) without reading the rest of the deployment. An empty result means the model
/// agrees with everything it was shown.
#[must_use]
pub fn verify(model: &DeploymentModel, facts: &Facts) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();
    if let Some(edition) = &facts.edition
        && edition != &model.edition
    {
        mismatches.push(Mismatch::new("edition", edition, &model.edition));
    }
    if let Some(platform) = &facts.platform
        && platform != &model.platform
    {
        mismatches.push(Mismatch::new("platform", platform, &model.platform));
    }
    if let Some(topology) = &facts.topology
        && topology != &model.topology
    {
        mismatches.push(Mismatch::new("topology", topology, &model.topology));
    }
    if let Some(backend) = &facts.backend
        && backend != &model.backend
    {
        mismatches.push(Mismatch::new("backend", backend, &model.backend));
    }
    if let Some(coordination) = &facts.coordination
        && coordination != &model.coordination
    {
        mismatches.push(Mismatch::new(
            "coordination",
            coordination,
            &model.coordination,
        ));
    }
    if let Some(sync) = &facts.sync
        && sync != &model.sync
    {
        mismatches.push(Mismatch::new("sync", sync, &model.sync));
    }
    if let Some(version) = &facts.runtime_version
        && version != &model.runtime_version
    {
        mismatches.push(Mismatch::new(
            "runtime_version",
            version,
            &model.runtime_version,
        ));
    }
    if let Some(apps) = &facts.apps
        && apps != &model.apps
    {
        mismatches.push(Mismatch::new("apps", apps, &model.apps));
    }
    if let Some(overrides) = &facts.overrides
        && overrides != &model.overrides
    {
        mismatches.push(Mismatch::new("overrides", overrides, &model.overrides));
    }
    if let Some(migrations) = &facts.migrations
        && migrations != &model.migrations
    {
        mismatches.push(Mismatch::new("migrations", migrations, &model.migrations));
    }
    if let Some(connectivity) = &facts.connectivity
        && connectivity != &model.connectivity
    {
        mismatches.push(Mismatch::new(
            "connectivity",
            connectivity,
            &model.connectivity,
        ));
    }
    if let Some(policies) = &facts.policies
        && policies != &model.policies
    {
        mismatches.push(Mismatch::new("policies", policies, &model.policies));
    }
    if let Some(tier) = facts.agent_tier
        && tier != model.agent_tier
    {
        mismatches.push(Mismatch::new("agent_tier", &tier, &model.agent_tier));
    }
    mismatches
}

/// The result of a periodic self-check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfCheck {
    /// The differences found before any refresh.
    pub mismatches: Vec<Mismatch>,
    /// The epoch the model moved to after refreshing on drift, when it refreshed.
    pub refreshed_to: Option<u64>,
}

impl SelfCheck {
    /// Whether the model already agreed with the ground truth.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// An in-memory fact source: the adapter tests' and simple hosts' implementation.
#[derive(Clone, Debug)]
pub struct MemoryFacts {
    epoch: u64,
    facts: Facts,
    evidence: Option<EvidenceRef>,
}

impl MemoryFacts {
    /// A source at `epoch` with no facts yet.
    #[must_use]
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            facts: Facts::new(),
            evidence: None,
        }
    }

    /// A source at `epoch` that already holds `facts`.
    #[must_use]
    pub fn with_facts(epoch: u64, facts: Facts) -> Self {
        Self {
            epoch,
            facts,
            evidence: None,
        }
    }

    /// Sets one fact.
    #[must_use]
    pub fn with(mut self, facts: Facts) -> Self {
        self.facts = facts;
        self
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// The facts it holds.
    #[must_use]
    pub fn facts_ref(&self) -> &Facts {
        &self.facts
    }
}

impl FactSource for MemoryFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Memory
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(self.facts.clone())
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
