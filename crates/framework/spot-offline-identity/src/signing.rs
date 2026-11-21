//! TUF-style signing roles (ADR-070 rule 12.13 b).
//!
//! - The **root key** stays offline. It only signs a [`Delegation`]: which online keys may sign
//!   snapshots and timestamps for a tenant, at a version that only ever increases.
//! - The **snapshot key** (online) signs the full [`Snapshot`].
//! - The **timestamp key** (online) signs a small [`Timestamp`] that restates the held
//!   snapshot's epoch and hash and a fresh set of deadlines. ADR-065's heartbeat can carry it,
//!   so the lease is extended often without re-sending the snapshot.
//!
//! Verification uses the Ed25519 primitives of `spot-bundle-signing` ([`PublicKey`]). Signing
//! sits behind the [`Signer`] trait because the root key lives in a key-management system or
//! an offline device, and this crate reads no randomness and holds no secret.

use serde::{Deserialize, Serialize};
use spot_bundle_signing::PublicKey;

use crate::canonical::Enc;
use crate::error::Rejection;
use crate::snapshot::{Snapshot, SnapshotHash, TierDeadlines};
use crate::types::{HubMs, NodeId, TenantId, Tier};

const DELEGATION_DOMAIN: &str = "SPOT-OFFLINE-DELEGATION-V1";
const TIMESTAMP_DOMAIN: &str = "SPOT-OFFLINE-TIMESTAMP-V2";
const HUB_TIME_DOMAIN: &str = "SPOT-OFFLINE-HUBTIME-V2";
const NODE_REQUEST_DOMAIN: &str = "SPOT-OFFLINE-NODEREQ-V1";

/// Something that can sign: a key-management client, an offline root device, or a test key.
pub trait Signer {
    /// The public half.
    fn public_key(&self) -> PublicKey;
    /// A detached Ed25519 signature (64 bytes) over `message`.
    fn sign(&self, message: &[u8]) -> Vec<u8>;
}

/// The root's statement of which keys may sign for a tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// The tenant the keys are valid for.
    pub tenant: TenantId,
    /// Increases whenever the online keys change; a node never accepts a lower version.
    pub version: u64,
    /// The snapshot key (32 bytes).
    pub snapshot_key: [u8; 32],
    /// The timestamp key (32 bytes).
    pub timestamp_key: [u8; 32],
}

impl Delegation {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new(DELEGATION_DOMAIN);
        e.str(self.tenant.as_str())
            .u64(self.version)
            .bytes(&self.snapshot_key)
            .bytes(&self.timestamp_key);
        e.finish()
    }
}

/// A delegation and the root's signature over it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDelegation {
    /// The delegation.
    pub body: Delegation,
    /// The root's signature (64 bytes).
    pub signature: Vec<u8>,
}

/// The small, frequently re-signed message that extends the lease without changing the
/// snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    /// The tenant.
    pub tenant: TenantId,
    /// The node.
    pub node: NodeId,
    /// The epoch of the snapshot it restates.
    pub epoch: u64,
    /// The hash of that snapshot.
    pub snapshot_hash: SnapshotHash,
    /// Hub time of signing; strictly increases from one timestamp to the next.
    pub signed_at: HubMs,
    /// The deadlines per tier.
    pub valid_until: TierDeadlines,
    /// A fresh hub nonce: strictly higher than the one in the previous reply the hub signed
    /// for this node. The node remembers the newest one (see [`crate::lease::LeaseRecord`]).
    pub nonce: u64,
}

impl Timestamp {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new(TIMESTAMP_DOMAIN);
        e.str(self.tenant.as_str())
            .str(self.node.as_str())
            .u64(self.epoch)
            .bytes(&self.snapshot_hash)
            .u64(self.signed_at);
        for t in Tier::ALL {
            e.u64(self.valid_until.get(t));
        }
        e.u64(self.nonce);
        e.finish()
    }
}

/// The hub's time, signed. This is the only way hub time reaches a node other than the
/// `issued_at` of a signed snapshot and the `signed_at` of a signed [`Timestamp`]: a heartbeat
/// may prove the hub is alive, but it never carries time the node believes.
///
/// It restates no deadlines, so it can restore a lease that a restart charged (see
/// [`crate::lease::MAX_CHECKPOINT_GAP`]) but never extend one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubTime {
    /// The tenant.
    pub tenant: TenantId,
    /// The node it is addressed to.
    pub node: NodeId,
    /// The highest snapshot epoch the hub has issued to the node.
    pub epoch: u64,
    /// Hub time of signing; strictly increases from one signed time to the next.
    pub hub_ms: HubMs,
    /// A fresh hub nonce: strictly higher than the one in the previous reply the hub signed
    /// for this node.
    pub nonce: u64,
}

impl HubTime {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new(HUB_TIME_DOMAIN);
        e.str(self.tenant.as_str())
            .str(self.node.as_str())
            .u64(self.epoch)
            .u64(self.hub_ms)
            .u64(self.nonce);
        e.finish()
    }
}

/// A snapshot with the snapshot key's signature and the delegation that authorises the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedSnapshot {
    /// The snapshot.
    pub snapshot: Snapshot,
    /// The snapshot key's signature (64 bytes).
    pub signature: Vec<u8>,
    /// The root's delegation of the snapshot key.
    pub delegation: SignedDelegation,
}

/// A timestamp with the timestamp key's signature and the delegation that authorises the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedTimestamp {
    /// The timestamp.
    pub timestamp: Timestamp,
    /// The timestamp key's signature (64 bytes).
    pub signature: Vec<u8>,
    /// The root's delegation of the timestamp key.
    pub delegation: SignedDelegation,
}

/// A hub time with the timestamp key's signature and the delegation that authorises the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedHubTime {
    /// The signed time.
    pub time: HubTime,
    /// The timestamp key's signature (64 bytes).
    pub signature: Vec<u8>,
    /// The root's delegation of the timestamp key.
    pub delegation: SignedDelegation,
}

/// What the hub sends a node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    /// A full snapshot.
    Snapshot(SignedSnapshot),
    /// A timestamp for the snapshot the node holds.
    Timestamp(SignedTimestamp),
    /// The hub's time and nothing else.
    HubTime(SignedHubTime),
}

/// What the node holds, as it tells the hub (the node half of replay detection).
///
/// A disk-write attacker can put back an older state **and** its older fence while the node is
/// off; nothing on the node can see that. The hub can, if the node says what it holds: a
/// node that reports an epoch, snapshot hash or nonce older than the hub knows it was given has
/// run on a replayed image. The report is part of every [`NodeRequest`] and covered by its
/// signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    /// The highest epoch the node holds.
    pub highest_epoch: u64,
    /// The hash of the snapshot at that epoch, if the node holds one.
    pub snapshot_hash: Option<SnapshotHash>,
    /// The newest signed hub time the node accepted (its hub clock).
    pub last_attested: HubMs,
    /// The nonce of the newest hub reply the node accepted (zero before the first).
    pub hub_nonce: u64,
}

impl NodeReport {
    fn encode(&self, e: &mut Enc) {
        e.u64(self.highest_epoch);
        match &self.snapshot_hash {
            Some(hash) => e.bool(true).bytes(hash),
            None => e.bool(false),
        };
        e.u64(self.last_attested).u64(self.hub_nonce);
    }
}

/// What a node asks the hub for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestKind {
    /// A signed hub time.
    HubTime,
    /// A snapshot (or a timestamp for the one the node holds).
    Snapshot,
}

/// The node's request to the hub for signed hub time or a snapshot. It is not valid without
/// its [`NodeReport`] ([`verify_node_request`] refuses it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRequest {
    /// The tenant.
    pub tenant: TenantId,
    /// The node asking.
    pub node: NodeId,
    /// What it asks for.
    pub kind: RequestKind,
    /// What the node holds. `None` is a malformed request: the verifier refuses it.
    #[serde(default)]
    pub report: Option<NodeReport>,
}

impl NodeRequest {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new(NODE_REQUEST_DOMAIN);
        e.str(self.tenant.as_str())
            .str(self.node.as_str())
            .u8(match self.kind {
                RequestKind::HubTime => 1,
                RequestKind::Snapshot => 2,
            });
        match &self.report {
            Some(report) => {
                e.bool(true);
                report.encode(&mut e);
            }
            None => {
                e.bool(false);
            }
        }
        e.finish()
    }
}

/// A request and the node's signature over it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedNodeRequest {
    /// The request.
    pub request: NodeRequest,
    /// The node key's signature (64 bytes).
    pub signature: Vec<u8>,
}

/// Why the hub (or a test) refuses a [`SignedNodeRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestRefusal {
    /// The request carries no [`NodeReport`].
    #[error("the request carries no node report ({code})", code = self.code())]
    ReportMissing,
    /// The signature does not verify under the node key.
    #[error("the request signature does not verify ({code})", code = self.code())]
    BadSignature,
}

impl RequestRefusal {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ReportMissing => "offline_identity.request_report_missing",
            Self::BadSignature => "offline_identity.request_bad_signature",
        }
    }
}

/// Signs a request with the node key.
#[must_use]
pub fn sign_node_request(request: NodeRequest, key: &dyn Signer) -> SignedNodeRequest {
    let signature = key.sign(&request.canonical_bytes());
    SignedNodeRequest { request, signature }
}

/// Verifies a request under the node's public key and returns its report. The report is
/// required, and the signature covers it.
///
/// # Errors
/// [`RequestRefusal::BadSignature`] when the signature does not verify (including a changed
/// report), [`RequestRefusal::ReportMissing`] when the request carries no report.
pub fn verify_node_request<'a>(
    node_key: &PublicKey,
    signed: &'a SignedNodeRequest,
) -> Result<&'a NodeReport, RequestRefusal> {
    if !check(
        node_key,
        &signed.request.canonical_bytes(),
        &signed.signature,
    ) {
        return Err(RequestRefusal::BadSignature);
    }
    signed
        .request
        .report
        .as_ref()
        .ok_or(RequestRefusal::ReportMissing)
}

/// Signs a delegation with the root key.
#[must_use]
pub fn sign_delegation(
    root: &dyn Signer,
    tenant: &TenantId,
    version: u64,
    snapshot_key: &PublicKey,
    timestamp_key: &PublicKey,
) -> SignedDelegation {
    let body = Delegation {
        tenant: tenant.clone(),
        version,
        snapshot_key: *snapshot_key.as_bytes(),
        timestamp_key: *timestamp_key.as_bytes(),
    };
    let signature = root.sign(&body.canonical_bytes());
    SignedDelegation { body, signature }
}

/// Signs a snapshot with the snapshot key.
#[must_use]
pub fn sign_snapshot(
    snapshot: Snapshot,
    key: &dyn Signer,
    delegation: SignedDelegation,
) -> SignedSnapshot {
    let signature = key.sign(&snapshot.canonical_bytes());
    SignedSnapshot {
        snapshot,
        signature,
        delegation,
    }
}

/// Signs a timestamp with the timestamp key.
#[must_use]
pub fn sign_timestamp(
    timestamp: Timestamp,
    key: &dyn Signer,
    delegation: SignedDelegation,
) -> SignedTimestamp {
    let signature = key.sign(&timestamp.canonical_bytes());
    SignedTimestamp {
        timestamp,
        signature,
        delegation,
    }
}

/// Signs a hub time with the timestamp key.
#[must_use]
pub fn sign_hub_time(
    time: HubTime,
    key: &dyn Signer,
    delegation: SignedDelegation,
) -> SignedHubTime {
    let signature = key.sign(&time.canonical_bytes());
    SignedHubTime {
        time,
        signature,
        delegation,
    }
}

fn check(key: &PublicKey, message: &[u8], signature: &[u8]) -> bool {
    <[u8; 64]>::try_from(signature).is_ok_and(|sig| key.verify(message, &sig).is_ok())
}

/// Verifies a delegation against the trusted root and the expected tenant.
pub(crate) fn verify_delegation(
    root: &PublicKey,
    tenant: &TenantId,
    delegation: &SignedDelegation,
) -> Result<(), Rejection> {
    if &delegation.body.tenant != tenant {
        return Err(Rejection::WrongTenant);
    }
    if check(
        root,
        &delegation.body.canonical_bytes(),
        &delegation.signature,
    ) {
        Ok(())
    } else {
        Err(Rejection::BadDelegation)
    }
}

/// Verifies a snapshot's signature under the delegated snapshot key.
pub(crate) fn verify_snapshot(signed: &SignedSnapshot) -> Result<(), Rejection> {
    let key = PublicKey::from_bytes(&signed.delegation.body.snapshot_key)
        .map_err(|_| Rejection::BadDelegation)?;
    if check(&key, &signed.snapshot.canonical_bytes(), &signed.signature) {
        Ok(())
    } else {
        Err(Rejection::BadSignature)
    }
}

/// Verifies a timestamp's signature under the delegated timestamp key.
pub(crate) fn verify_timestamp(signed: &SignedTimestamp) -> Result<(), Rejection> {
    let key = PublicKey::from_bytes(&signed.delegation.body.timestamp_key)
        .map_err(|_| Rejection::BadDelegation)?;
    if check(&key, &signed.timestamp.canonical_bytes(), &signed.signature) {
        Ok(())
    } else {
        Err(Rejection::BadSignature)
    }
}

/// Verifies a hub time's signature under the delegated timestamp key.
pub(crate) fn verify_hub_time(signed: &SignedHubTime) -> Result<(), Rejection> {
    let key = PublicKey::from_bytes(&signed.delegation.body.timestamp_key)
        .map_err(|_| Rejection::BadDelegation)?;
    if check(&key, &signed.time.canonical_bytes(), &signed.signature) {
        Ok(())
    } else {
        Err(Rejection::BadSignature)
    }
}
