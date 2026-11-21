//! How one installed connector is configured: the credential it authenticates with, the
//! credentials it may read, its sandbox limits and its key-value budget.

use spot_wasm::Limits;

use crate::model::ConnectorId;
use crate::vault::{AuthSpec, CredentialRef};

/// The host-side configuration of one installed connector. The bundle carries the component;
/// this carries the policy the host applies to it.
#[derive(Clone, Debug)]
pub struct ConnectorConfig {
    /// The connector's identity.
    pub id: ConnectorId,
    /// How the host authenticates outbound requests, if at all.
    pub auth: Option<AuthSpec>,
    /// The references the connector may read with `secrets-get`. A reference used by `auth` is
    /// never in this list: the host injects it instead.
    pub secrets: Vec<CredentialRef>,
    /// The hosts the connector declares it talks to. The egress policy is authoritative; this
    /// is what the connector *intends* and is reported for review.
    pub endpoints: Vec<String>,
    /// The sandbox limits: fuel, wall clock, memory, table elements, host calls, output bytes.
    pub limits: Limits,
    /// The key-value store's byte budget for this connector.
    pub kv_bytes: usize,
}

impl ConnectorConfig {
    /// A configuration for `id` with the default sandbox limits and no credentials.
    pub fn new(id: ConnectorId) -> Self {
        Self {
            id,
            auth: None,
            secrets: Vec::new(),
            endpoints: Vec::new(),
            limits: Limits::default(),
            kv_bytes: 64 * 1024,
        }
    }

    /// Refuses a configuration that injects authentication without declaring the hosts the
    /// credential may go to. Without a list the credential would be attached to a request for
    /// any host the egress policy allows.
    pub fn validate(&self) -> crate::error::Result<()> {
        let injects = self.auth.as_ref().is_some_and(|a| a.kind.is_injected());
        if injects && self.endpoints.iter().all(|e| e.trim().is_empty()) {
            return Err(crate::error::ConnectorError::EndpointsRequired {
                connector: self.id.as_str().to_owned(),
            });
        }
        Ok(())
    }

    /// Whether the connector may read `reference` with `secrets-get`.
    pub fn may_read_secret(&self, reference: &CredentialRef) -> bool {
        if let Some(auth) = &self.auth
            && auth.credential == *reference
        {
            // Injected by the host; the guest must not read it.
            return false;
        }
        self.secrets.iter().any(|r| r == reference)
    }
}
