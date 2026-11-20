//! `spot-agent enrol`: generate the identity key, present the one-time token to the
//! gateway (`POST /agent/v1/enrol`, contract `agent-channel` 0.1.0), store the trust
//! anchor and the enrolment record, all before the first poll.
//!
//! The gateway call is a trait ([`EnrolGateway`]): the TLS client is WP-ADM-02.T3
//! (`channel::enrol_gateway`); tests use an in-process fake that implements the contract
//! (token single use).
//!
//! The token is never logged or stored: only a digest prefix is, in the log and in the
//! local audit log.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use spot_agent_core::ResultLog as _;
use spot_agent_enroll::identity::AgentIdentity;
use spot_agent_enroll::keys::{KeyStore, Protection};
use spot_agent_enroll::message::PublicKeyB64;

use crate::audit::{LocalAuditLog, event_record};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::keystore::{KeyStoreKind, open_key_store};
use crate::logging::Logger;
use crate::state::{StateDir, atomic_write};
use crate::trust::{TrustFile, parse_trust_anchor};

/// The platform block of the enrol request.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlatformWire {
    /// `linux` or `windows`.
    pub os: String,
    /// CPU architecture.
    pub arch: String,
    /// The agent version.
    pub agent_version: String,
}

/// `AgentEnrolRequest`.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrolRequestWire {
    /// The one-time token.
    pub token: String,
    /// Base64 Ed25519 public key.
    pub public_key: String,
    /// The platform.
    pub platform: PlatformWire,
}

impl std::fmt::Debug for EnrolRequestWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolRequestWire")
            .field("token", &"<redacted>")
            .field("platform", &self.platform)
            .finish()
    }
}

/// The `data` of `AgentEnrolResponse`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnrolResponseWire {
    /// The assigned agent id.
    pub agent_id: String,
    /// The administrator key(s) the agent trusts for commands.
    pub trust_anchor: String,
    /// Suggested delay before the first poll.
    pub poll_after_ms: u64,
}

/// The enrolment call to the gateway.
pub trait EnrolGateway {
    /// Sends the request; a used or unknown token is [`AgentError::TokenInvalid`].
    ///
    /// # Errors
    /// [`AgentError::TokenInvalid`], [`AgentError::Transport`], [`AgentError::GatewayProtocol`].
    fn enrol(&self, request: &EnrolRequestWire) -> Result<EnrolResponseWire, AgentError>;
}

/// What a successful enrolment produced (nothing secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrolSummary {
    /// The assigned agent id.
    pub agent_id: String,
    /// The public key fingerprint.
    pub fingerprint: String,
    /// How many administrator keys were trusted.
    pub trusted_keys: usize,
    /// Digest prefix of the token, as logged.
    pub token_digest: String,
}

/// The first 8 hex characters of SHA-256 of the token.
#[must_use]
pub fn token_digest_prefix(token: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::new();
    for b in digest.iter().take(4) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Whether `id` is an acceptable agent id.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The inputs of an enrolment.
pub struct EnrolInput<'a> {
    /// The state directory.
    pub state: &'a StateDir,
    /// The gateway base URL.
    pub gateway_url: &'a str,
    /// The CA file path to record in the config.
    pub ca: Option<&'a str>,
    /// The one-time token.
    pub token: &'a str,
    /// Which key store holds the identity key.
    pub key_store: KeyStoreKind,
    /// The platform block.
    pub platform: PlatformWire,
    /// The current instant, Unix seconds.
    pub now: i64,
    /// The operator accepts software administrator keys on a host without a hardware-backed
    /// key store (`--allow-software-keys`). Read by [`enrol`] only: a rotation never changes
    /// the stored policy.
    pub allow_software_keys: bool,
}

/// What the enrolment decided about key protection, from the store that was really created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyDecision {
    /// The identity key store is hardware-backed.
    hardware: bool,
    /// Software administrator keys are accepted on this host.
    software_allowed: bool,
}

/// Decides the key protection policy of a new enrolment: hardware-backed stores keep the
/// strict policy; a software store needs the operator's explicit flag.
fn decide(protection: Protection, allow_software_keys: bool) -> Result<KeyDecision, AgentError> {
    match protection {
        Protection::Hardware => Ok(KeyDecision {
            hardware: true,
            software_allowed: false,
        }),
        Protection::Software if allow_software_keys => Ok(KeyDecision {
            hardware: false,
            software_allowed: true,
        }),
        Protection::Software => Err(AgentError::SoftwareKeysNotAllowed),
    }
}

/// Runs the enrolment.
///
/// # Errors
/// [`AgentError::AlreadyEnrolled`] when an identity exists; the gateway errors; the state
/// errors.
pub fn enrol(
    input: &EnrolInput<'_>,
    gateway: &dyn EnrolGateway,
    logger: &Logger,
) -> Result<EnrolSummary, AgentError> {
    if input.state.is_enrolled() {
        return Err(AgentError::AlreadyEnrolled);
    }
    // A file store is a software key by construction: refuse before the key is created and
    // before the one-time token is spent.
    if input.key_store == KeyStoreKind::File && !input.allow_software_keys {
        return Err(AgentError::SoftwareKeysNotAllowed);
    }
    // The key file lives in the state directory: it must exist before the store creates the key.
    input.state.create()?;
    let store = open_key_store(input.key_store, &input.state.key_path())?;
    enrol_with_store(input, store, gateway, logger)
}

/// [`enrol`] with the identity key store already opened (the seam tests use to present a
/// hardware-backed store; the TPM wiring is a later task).
///
/// # Errors
/// As [`enrol`]; [`AgentError::SoftwareKeysNotAllowed`] when `store` is a software store
/// and `input.allow_software_keys` is not set.
pub fn enrol_with_store(
    input: &EnrolInput<'_>,
    store: Box<dyn KeyStore>,
    gateway: &dyn EnrolGateway,
    logger: &Logger,
) -> Result<EnrolSummary, AgentError> {
    if input.state.is_enrolled() {
        return Err(AgentError::AlreadyEnrolled);
    }
    let decision = decide(store.protection(), input.allow_software_keys)?;
    let token = input.token;
    if !(16..=256).contains(&token.len()) || !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(AgentError::Usage(
            "the enrolment token has an invalid shape".to_owned(),
        ));
    }
    // Validate the config before touching the gateway: the token is one-time.
    let config = AgentConfig::new(input.gateway_url, input.ca.map(str::to_owned));
    config.validate()?;
    input.state.create()?;
    let public = store.public_key();
    let digest = token_digest_prefix(token);
    logger.info(
        "enrol_start",
        &format!("enrolling with token digest {digest}"),
    );

    let request = EnrolRequestWire {
        token: token.to_owned(),
        public_key: STANDARD.encode(public.as_bytes()),
        platform: input.platform.clone(),
    };
    let response = gateway.enrol(&request)?;

    if !valid_agent_id(&response.agent_id) || response.poll_after_ms > 60_000 {
        return Err(AgentError::GatewayProtocol(
            "the enrolment answer is invalid".to_owned(),
        ));
    }
    let keys = parse_trust_anchor(&response.trust_anchor)?;

    // Order: trust anchor, config, then the identity (the marker of "enrolled").
    TrustFile::from_keys_with(&keys, decision.hardware, decision.software_allowed)
        .save(&input.state.trust_path())?;
    atomic_write(&input.state.config_path(), config.to_toml()?.as_bytes())?;
    let identity = AgentIdentity {
        agent_id: response.agent_id.clone(),
        server_public_key: PublicKeyB64(STANDARD.encode(keys[0])),
        enrolled_at: input.now,
    };
    identity
        .save(input.state.key_path())
        .map_err(|_| AgentError::StateIo("cannot write the enrolment record".to_owned()))?;

    let audit = LocalAuditLog::open(input.state.audit_path(), response.agent_id.clone())?;
    // The audit record carries the truth about key protection: a host that accepted
    // software keys says so in its own log.
    let outcome = if decision.software_allowed {
        "enrolled_software_keys"
    } else {
        "enrolled"
    };
    let mut record = event_record(&response.agent_id, "agent.enrol", outcome, None, input.now);
    record.command_id = format!("token:{digest}");
    audit
        .append(&record)
        .map_err(|_| AgentError::AuditFailed("cannot record the enrolment".to_owned()))?;
    logger.info(
        "enrolled",
        &format!("enrolled as {} (token digest {digest})", response.agent_id),
    );
    Ok(EnrolSummary {
        agent_id: response.agent_id,
        fingerprint: public.fingerprint(),
        trusted_keys: keys.len(),
        token_digest: digest,
    })
}

/// Rotates the identity key: a new key is generated beside the old one, enrolled with a
/// fresh one-time token, and replaces the old key only after the gateway accepted it.
/// (The gateway revokes the old key once the new one polls successfully.)
///
/// # Errors
/// [`AgentError::NotEnrolled`] when not enrolled; the gateway and state errors.
pub fn rotate_key(
    input: &EnrolInput<'_>,
    gateway: &dyn EnrolGateway,
    logger: &Logger,
) -> Result<EnrolSummary, AgentError> {
    if !input.state.is_enrolled() {
        return Err(AgentError::NotEnrolled);
    }
    let token = input.token;
    if !(16..=256).contains(&token.len()) {
        return Err(AgentError::Usage(
            "the enrolment token has an invalid shape".to_owned(),
        ));
    }
    let mut next_name = input.state.key_path().into_os_string();
    next_name.push(".next");
    let next = std::path::PathBuf::from(next_name);
    let store = open_key_store(input.key_store, &next)?;
    let public = store.public_key();
    // The policy is the one stored at enrolment: a rotation never loosens it. A software
    // key on a host that did not accept software keys is refused (the new key file is
    // removed so no half-created key stays behind).
    let policy_file = TrustFile::load(&input.state.trust_path())?;
    let hardware = store.protection() == Protection::Hardware;
    if !hardware && !policy_file.software_keys_allowed {
        drop(store);
        let _ = std::fs::remove_file(&next);
        return Err(AgentError::SoftwareKeysNotAllowed);
    }
    let digest = token_digest_prefix(token);
    let response = gateway.enrol(&EnrolRequestWire {
        token: token.to_owned(),
        public_key: STANDARD.encode(public.as_bytes()),
        platform: input.platform.clone(),
    })?;
    if !valid_agent_id(&response.agent_id) || response.poll_after_ms > 60_000 {
        return Err(AgentError::GatewayProtocol(
            "the enrolment answer is invalid".to_owned(),
        ));
    }
    let keys = parse_trust_anchor(&response.trust_anchor)?;
    TrustFile::from_keys_with(&keys, hardware, policy_file.software_keys_allowed)
        .save(&input.state.trust_path())?;
    drop(store);
    std::fs::rename(&next, input.state.key_path())
        .map_err(|_| AgentError::StateIo("cannot install the new key".to_owned()))?;
    AgentIdentity {
        agent_id: response.agent_id.clone(),
        server_public_key: PublicKeyB64(STANDARD.encode(keys[0])),
        enrolled_at: input.now,
    }
    .save(input.state.key_path())
    .map_err(|_| AgentError::StateIo("cannot write the enrolment record".to_owned()))?;
    let audit = LocalAuditLog::open(input.state.audit_path(), response.agent_id.clone())?;
    let mut record = event_record(
        &response.agent_id,
        "agent.rotate_key",
        "key_rotated",
        None,
        input.now,
    );
    record.command_id = format!("token:{digest}");
    audit
        .append(&record)
        .map_err(|_| AgentError::AuditFailed("cannot record the rotation".to_owned()))?;
    logger.info(
        "key_rotated",
        &format!("key rotated (token digest {digest})"),
    );
    Ok(EnrolSummary {
        agent_id: response.agent_id,
        fingerprint: public.fingerprint(),
        trusted_keys: keys.len(),
        token_digest: digest,
    })
}
