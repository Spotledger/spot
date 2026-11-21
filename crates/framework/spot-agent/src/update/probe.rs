//! `spot-agent probe`: the short run of a new version that the running agent starts during a
//! self-update (see [`service::ProcessProbe`](super::service::ProcessProbe)). It reaches the
//! gateway over the same channel, sends one heartbeat that carries its own version and exits
//! 0; any failure exits non-zero and the engine rolls the update back.
//!
//! The probe takes no command, opens no audit log and touches no state: it only reads the
//! enrolment record, the configuration and the identity key, like `status` does.

use std::sync::Arc;

use spot_agent_enroll::identity::AgentIdentity;
use spot_agent_enroll::keys::KeyStore;

use crate::channel::client::Client;
use crate::channel::serve::build_transport;
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::keystore::{KeyStoreKind, open_key_store};
use crate::state::StateDir;

/// Runs the probe: one signed heartbeat with `agent_version` and the state word `probe`.
///
/// # Errors
/// [`AgentError::NotEnrolled`], the state and configuration errors, or
/// [`AgentError::Transport`] when the gateway cannot be reached or refuses the heartbeat
/// (the text names no detail of the answer).
pub fn probe(state: &StateDir, now: i64) -> Result<(), AgentError> {
    if !state.root().exists() || !state.is_enrolled() {
        return Err(AgentError::NotEnrolled);
    }
    state.check_permissions()?;
    let text = std::fs::read_to_string(state.config_path()).map_err(|_| AgentError::NotEnrolled)?;
    let config = AgentConfig::parse(&text)?;
    let keys: Arc<dyn KeyStore> = Arc::from(open_key_store(KeyStoreKind::File, &state.key_path())?);
    let identity = AgentIdentity::load(state.key_path())
        .map_err(|_| AgentError::ConfigInvalid("the enrolment record is malformed".to_owned()))?;
    let transport = build_transport(
        &config.gateway,
        config.ca.as_deref().map(std::path::Path::new),
    )?;
    let client = Client::new(transport, keys, identity.agent_id);
    client
        .heartbeat(
            &[("agent_version".to_owned(), crate::VERSION.to_owned())],
            "probe",
            now,
        )
        .map_err(|_| AgentError::Transport("the probe heartbeat failed".to_owned()))
}
