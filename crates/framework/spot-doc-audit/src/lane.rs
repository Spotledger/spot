//! Lanes, the head digest and the signed site checkpoint.
//!
//! One hash chain has one head, so appending to a single chain inside every business transaction
//! would serialise every writer on that head. The document audit therefore shards documents over
//! `N` lanes, `lane = hash(doctype + id) mod N`: each lane is an independent chain, and a
//! document is always in exactly one lane, so its events keep their order. A small **head
//! digest** hashes every lane head into one value, and a **site checkpoint** signs that digest,
//! so removing a whole lane or rolling several lanes back together is also detectable.

use sha2::{Digest, Sha256};

use spot_audit_chain::{Checkpoint, CheckpointSigner, Hash, KeyRing, Verifier};

use crate::error::DocAuditError;

/// The domain tag of the head digest, so a digest is never a valid entry or checkpoint body.
const DIGEST_TAG: &[u8] = b"spot.docaudit.head.v1\0";
/// The domain tag of a lane hash, so a lane is stable across sites and versions.
const LANE_TAG: &[u8] = b"spot.docaudit.lane.v1\0";

/// The site string of one lane's chain in an [`AuditStore`](spot_audit_chain::AuditStore).
#[must_use]
pub fn lane_site(site: &str, lane: u32) -> String {
    format!("{site}:lane:{lane}")
}

/// The lane a document belongs to: `hash(doctype + id) mod n`, stable for the site's lifetime.
///
/// `n` is at least 1; a document's lane never changes while `n` and the epoch do not.
#[must_use]
pub fn lane_of(doctype: &str, id: &str, n: u32) -> u32 {
    let n = n.max(1);
    let mut hasher = Sha256::new();
    hasher.update(LANE_TAG);
    put_field(&mut hasher, doctype.as_bytes());
    put_field(&mut hasher, id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    let value = u64::from_be_bytes(first);
    // `n` is a small site setting, so the modulo bias is negligible and never a correctness issue
    // (it only shifts which lane a document lands in, not whether it is audited).
    u32::try_from(value % u64::from(n)).unwrap_or(0)
}

/// The number of lanes and the epoch they belong to. Changing the lane count is a new epoch; a
/// site checkpoint records the epoch so a roll-back of the configuration is detectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneConfig {
    /// The number of lanes; at least 1.
    pub n: u32,
    /// The epoch: bumped whenever `n` changes.
    pub epoch: u32,
}

impl LaneConfig {
    /// A configuration of `n` lanes at `epoch`; `n` is clamped to at least 1.
    #[must_use]
    pub fn new(n: u32, epoch: u32) -> Self {
        Self { n: n.max(1), epoch }
    }

    /// Every lane index, in order.
    pub fn lanes(&self) -> impl Iterator<Item = u32> + '_ {
        0..self.n
    }

    /// Checks a site checkpoint's epoch and lane count against this configuration.
    ///
    /// # Errors
    /// [`DocAuditError::LaneEpoch`] when either differs.
    pub fn check_epoch(&self, checkpoint: &SiteCheckpoint) -> Result<(), DocAuditError> {
        if checkpoint.epoch != self.epoch || checkpoint.n != self.n {
            return Err(DocAuditError::LaneEpoch {
                epoch: self.epoch,
                n: self.n,
                checkpoint: checkpoint.epoch,
                checkpoint_n: checkpoint.n,
            });
        }
        Ok(())
    }
}

/// One lane's head at the moment a site checkpoint was signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LaneHead {
    /// The lane.
    pub lane: u32,
    /// The head's sequence number; `0` for an empty lane.
    pub seq: u64,
    /// The head's hash, or the lane's genesis hash when `seq` is 0.
    pub head_hash: Hash,
}

/// Hashes every lane head into one digest that the site checkpoint signs.
pub struct HeadDigest;

impl HeadDigest {
    /// The digest of `heads` at `epoch`: `SHA-256(tag || u32(epoch) || u32(count) || heads)`,
    /// where each head is `u32(lane) || u64(seq) || head_hash`, in the order given.
    #[must_use]
    pub fn compute(heads: &[LaneHead], epoch: u32) -> Hash {
        let mut hasher = Sha256::new();
        hasher.update(DIGEST_TAG);
        hasher.update(epoch.to_be_bytes());
        hasher.update(u32::try_from(heads.len()).unwrap_or(u32::MAX).to_be_bytes());
        for head in heads {
            hasher.update(head.lane.to_be_bytes());
            hasher.update(head.seq.to_be_bytes());
            hasher.update(head.head_hash);
        }
        hasher.finalize().into()
    }
}

/// A signed statement that every lane was at least at the named head when it was taken.
///
/// The lane heads are bound by [`HeadDigest`]; the digest is signed as the `head_hash` of an
/// ordinary [`Checkpoint`], so the same key ring and verifier that check a lane's chain check a
/// site checkpoint too. Removing a lane's tail makes a head fall short of the signed `seq`, and
/// removing a lane altogether makes its head hash differ.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SiteCheckpoint {
    /// The site.
    pub site: String,
    /// The epoch the lane count belongs to.
    pub epoch: u32,
    /// The number of lanes.
    pub n: u32,
    /// Every lane's head, one entry per lane.
    pub lanes: Vec<LaneHead>,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
    /// The signature over the head digest (an ordinary chain checkpoint).
    pub signed: Checkpoint,
}

impl SiteCheckpoint {
    /// Signs a site checkpoint of `heads` for `site` at `at`.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the signer fails or the heads are malformed.
    pub fn sign(
        signer: &dyn CheckpointSigner,
        site: &str,
        config: &LaneConfig,
        heads: &[LaneHead],
        at: i64,
    ) -> Result<Self, DocAuditError> {
        let mut lanes = heads.to_vec();
        lanes.sort_by_key(|h| h.lane);
        if lanes.len() != config.n as usize
            || lanes
                .iter()
                .enumerate()
                .any(|(i, h)| h.lane != u32::try_from(i).unwrap_or(u32::MAX))
        {
            return Err(DocAuditError::Chain(
                "the lane heads do not name every lane exactly once".to_owned(),
            ));
        }
        let digest = HeadDigest::compute(&lanes, config.epoch);
        let seq: u64 = lanes.iter().map(|h| h.seq).fold(0, u64::saturating_add);
        let signed = Checkpoint::sign(signer, site, seq, digest, at)?;
        Ok(Self {
            site: site.to_owned(),
            epoch: config.epoch,
            n: config.n,
            lanes,
            at,
            signed,
        })
    }

    /// The head digest this checkpoint binds.
    #[must_use]
    pub fn digest(&self) -> Hash {
        HeadDigest::compute(&self.lanes, self.epoch)
    }

    /// Checks the checkpoint for `site` under `config`: the epoch and lane count match, the
    /// signature verifies under `keys`, the lanes name every lane once, and the digest is the one
    /// that was signed.
    ///
    /// # Errors
    /// [`DocAuditError::LaneEpoch`] or [`DocAuditError::Chain`].
    pub fn verify(
        &self,
        keys: &KeyRing,
        site: &str,
        config: &LaneConfig,
    ) -> Result<(), DocAuditError> {
        config.check_epoch(self)?;
        if self.site != site {
            return Err(DocAuditError::Chain(format!(
                "the site checkpoint is for `{}`, not `{site}`",
                self.site
            )));
        }
        let mut lanes = self.lanes.clone();
        lanes.sort_by_key(|h| h.lane);
        if lanes.len() != config.n as usize
            || lanes
                .iter()
                .enumerate()
                .any(|(i, h)| h.lane != u32::try_from(i).unwrap_or(u32::MAX))
        {
            return Err(DocAuditError::Chain(
                "the site checkpoint does not name every lane exactly once".to_owned(),
            ));
        }
        if let Some(kind) = Verifier::new(keys).check_checkpoint(site, &self.signed) {
            return Err(DocAuditError::Chain(format!(
                "the site checkpoint signature does not verify ({})",
                kind.code()
            )));
        }
        if self.signed.head_hash != self.digest() {
            return Err(DocAuditError::Chain(
                "the site checkpoint's signed digest is not the digest of its lane heads"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    hasher.update(bytes);
}
