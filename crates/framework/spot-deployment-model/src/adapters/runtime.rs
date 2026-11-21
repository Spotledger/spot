//! The host-agent adapter: the runtime profile, node roles, PostgreSQL version and replicas.
//!
//! `spot-agent-runtime` (ADM-06) owns what this node runs. The profile and role set are its
//! settings; the replica status comes from its `db.replica.status` module. This adapter maps
//! them onto the model's topology and backend, using the same wire names the runtime writes
//! (`web`, `jobs`, `realtime`, `database`, `branch-edge`) so the two never drift apart.

use std::collections::BTreeSet;

use spot_agent_runtime::{
    NodeRole as RuntimeNodeRole, Profile, ReplicaRole as RuntimeReplicaRole, ReplicaStatus,
};
use spot_self_model::EvidenceRef;

use crate::DeploymentError;
use crate::facts::Facts;
use crate::model::{
    Backend, BackendEngine, DeploymentNode, FactSourceKind, NodeRole, Placement, Replica,
    ReplicaRole, Topology, TopologyMode,
};

/// The runtime's ground truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeFacts {
    epoch: u64,
    node: String,
    topology: Topology,
    backend: Option<Backend>,
    runtime_version: Option<String>,
    evidence: Option<EvidenceRef>,
}

impl RuntimeFacts {
    /// Reads this node's profile and roles.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the role set is empty or the node name is unusable.
    pub fn new(
        epoch: u64,
        node: impl Into<String>,
        profile: Profile,
        roles: impl IntoIterator<Item = RuntimeNodeRole>,
    ) -> Result<Self, DeploymentError> {
        let node = node.into();
        let roles: BTreeSet<NodeRole> = roles
            .into_iter()
            .map(|role| match role {
                RuntimeNodeRole::Web => NodeRole::Web,
                RuntimeNodeRole::Jobs => NodeRole::Jobs,
                RuntimeNodeRole::Realtime => NodeRole::Realtime,
                RuntimeNodeRole::Database => NodeRole::Database,
                RuntimeNodeRole::BranchEdge => NodeRole::BranchEdge,
            })
            .collect();
        let placement = if roles.contains(&NodeRole::BranchEdge) {
            Placement::BranchEdge
        } else {
            Placement::HeadOffice
        };
        let mode = match profile {
            Profile::Standalone => TopologyMode::Standalone,
            Profile::ClusterBasic | Profile::ClusterRedis => TopologyMode::Cluster,
        };
        let topology = Topology::new(
            mode,
            placement,
            std::collections::BTreeMap::from([(node.clone(), DeploymentNode::new(roles)?)]),
        )?;
        Ok(Self {
            epoch,
            node,
            topology,
            backend: None,
            runtime_version: None,
            evidence: None,
        })
    }

    /// Records the PostgreSQL version and the replicas this node sees.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Invalid`] when the version is empty or two replicas name one node.
    pub fn with_backend(
        mut self,
        version: impl Into<String>,
        replicas: impl IntoIterator<Item = (String, ReplicaStatus)>,
    ) -> Result<Self, DeploymentError> {
        let replicas: Vec<Replica> = replicas
            .into_iter()
            .map(|(node, status)| {
                let role = match status.role {
                    RuntimeReplicaRole::Primary => ReplicaRole::Primary,
                    RuntimeReplicaRole::Standby => ReplicaRole::Standby,
                };
                Replica::new(node, role, status.streaming)
            })
            .collect::<Result<_, _>>()?;
        self.backend = Some(Backend::new(BackendEngine::Postgres, version, replicas)?);
        Ok(self)
    }

    /// Records the Spotledger runtime version.
    #[must_use]
    pub fn with_runtime_version(mut self, version: impl Into<String>) -> Self {
        self.runtime_version = Some(version.into());
        self
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// The node this adapter read.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }
}

impl crate::facts::FactSource for RuntimeFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Runtime
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(Facts {
            topology: Some(self.topology.clone()),
            backend: self.backend.clone(),
            runtime_version: self.runtime_version.clone(),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
