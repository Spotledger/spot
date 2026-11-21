//! The settings adapter: the node profile's cache and coordination shape, and site policies.
//!
//! The profile (`standalone`, `cluster-basic`, `cluster-redis`) is a bundle of ordinary
//! settings; this adapter reads the resolved profile value and maps it to the coordination
//! facts, exactly as `spot-agent-runtime`'s `apply_profile` would write them. It reads only a
//! [`Resolved`] value: a setting never reaches the model as a secret, because
//! [`EffectiveValue::Redacted`](spot_settings::EffectiveValue) is refused rather than mapped.

use spot_self_model::EvidenceRef;
use spot_settings::{ParamValue, Resolved};

use crate::DeploymentError;
use crate::facts::Facts;
use crate::model::{
    ApprovalPolicy, CacheBackend, Coordination, Coordinator, FactSourceKind, ReplayPolicy,
    SitePolicies,
};

/// The settings-derived facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsFacts {
    epoch: u64,
    coordination: Option<Coordination>,
    policies: Option<SitePolicies>,
    evidence: Option<EvidenceRef>,
}

impl SettingsFacts {
    /// An empty settings slice at `epoch`.
    #[must_use]
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            coordination: None,
            policies: None,
            evidence: None,
        }
    }

    /// Reads the resolved `runtime.node.profile` value.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the value is redacted, is not text, or is not one of
    /// the three shipped profiles.
    pub fn with_profile(mut self, profile: &Resolved) -> Result<Self, DeploymentError> {
        self.coordination = Some(coordination_for_profile(profile_name(profile)?)?);
        Ok(self)
    }

    /// Reads the resolved replay policy (`off`, `auto`, `always`), keeping any approval
    /// policy already set.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the value is redacted, is not text, or is not a policy.
    pub fn with_replay(mut self, resolved: &Resolved) -> Result<Self, DeploymentError> {
        let replay = replay_policy(resolved)?;
        let approval = self
            .policies
            .as_ref()
            .map_or_else(ApprovalPolicy::default, |policies| policies.approval);
        self.policies = Some(SitePolicies::new(replay, approval));
        Ok(self)
    }

    /// Sets the approval policy, keeping any replay policy already set.
    #[must_use]
    pub fn with_approval(mut self, approval: ApprovalPolicy) -> Self {
        let replay = self
            .policies
            .as_ref()
            .map_or(ReplayPolicy::Off, |policies| policies.replay);
        self.policies = Some(SitePolicies::new(replay, approval));
        self
    }

    /// Sets the site policies as the settings resolve them.
    #[must_use]
    pub fn with_policies(mut self, policies: SitePolicies) -> Self {
        self.policies = Some(policies);
        self
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl crate::facts::FactSource for SettingsFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Settings
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(Facts {
            coordination: self.coordination.clone(),
            policies: self.policies.clone(),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}

/// The profile name a resolved setting holds.
///
/// # Errors
///
/// [`DeploymentError::Invalid`] when the setting is redacted or is not text.
pub fn profile_name(profile: &Resolved) -> Result<&str, DeploymentError> {
    match profile.value.as_value() {
        Some(ParamValue::Text(name)) => Ok(name.as_str()),
        Some(_) => Err(DeploymentError::invalid(
            "settings profile",
            "the node profile is text",
        )),
        None => Err(DeploymentError::invalid(
            "settings profile",
            "the node profile is not a secret",
        )),
    }
}

/// The cache and coordination shape a named profile selects.
///
/// # Errors
///
/// [`DeploymentError::Invalid`] for any name outside `standalone`, `cluster-basic` and
/// `cluster-redis`.
pub fn coordination_for_profile(name: &str) -> Result<Coordination, DeploymentError> {
    match name {
        "standalone" | "cluster-basic" => Ok(Coordination::new(
            CacheBackend::Database,
            Coordinator::InProcess,
        )),
        "cluster-redis" => Ok(Coordination::new(CacheBackend::Redis, Coordinator::Redis)),
        other => Err(DeploymentError::invalid(
            "settings profile",
            format!("`{other}` is not a runtime profile"),
        )),
    }
}

/// The replay policy a resolved setting holds (`off`, `auto`, `always`).
///
/// # Errors
///
/// [`DeploymentError::Invalid`] when the value is redacted, is not text, or is not a policy.
pub fn replay_policy(resolved: &Resolved) -> Result<ReplayPolicy, DeploymentError> {
    match resolved.value.as_value() {
        Some(ParamValue::Text(name)) => crate::event::parse_replay_policy(name),
        Some(_) => Err(DeploymentError::invalid(
            "settings replay",
            "the replay policy is text",
        )),
        None => Err(DeploymentError::invalid(
            "settings replay",
            "the replay policy is not a secret",
        )),
    }
}
