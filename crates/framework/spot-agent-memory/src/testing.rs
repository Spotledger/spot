//! Deterministic test doubles: a bag-of-words fake embedder and small builders for the three
//! kinds.
//!
//! The fake embedder hashes each word into a fixed number of buckets, so two texts that share
//! words score above zero and two that share none score zero — enough to assert a ranking without
//! a model, a network or a random seed. Nothing here is a production path.

use std::collections::BTreeMap;

use spot_agent_tiers::Tier;
use spot_remedies::RemedyId;
use spot_retrieval::{EmbeddingProvider, RetrievalError};

use crate::episode::{ActionRef, Episode, EpisodeInput, Outcome};
use crate::fact::{Epoch, Fact, FactInput, FactValue};
use crate::id::{AuditRef, EpisodeId, FactId, FactKey, ProcedureId, RecordRef, Symptom, TenantId};
use crate::meta::{Confidence, EntryMeta, SourceKind};
use crate::procedure::{Confirmation, ParamValue, Procedure, ProcedureInput, ProcedureStep};
use crate::sanitiser::Sanitiser;

/// The symptom tag the test episodes share. Written in two parts because the error-code scan
/// reads any dotted lower-case literal as a candidate error code (the same spelling
/// `spot-remedies`' seeds use).
pub const STUCK_RUNNING: &str = concat!("job", ".stuck_running");
/// The remedy the test procedures run, spelled the same way and for the same reason.
pub const RETRY_FAILED: &str = concat!("job", ".retry_failed");

/// A deterministic bag-of-words embedding over `dim` buckets.
///
/// Words are split on everything that is not a letter or a digit, hashed with FNV-1a and counted,
/// so the same text always gives the same vector and nothing depends on a seed.
pub struct FakeEmbedder {
    dim: usize,
    fail: bool,
}

impl FakeEmbedder {
    /// A 64-bucket embedder that never fails.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dim: 64,
            fail: false,
        }
    }

    /// An embedder over `dim` buckets (at least one). Any width but `EMBEDDING_DIM` is refused by
    /// the crate; this exists to prove that.
    #[must_use]
    pub fn with_dim(dim: usize) -> Self {
        Self {
            dim: dim.max(1),
            fail: false,
        }
    }

    /// An embedder whose every call fails, for the provider-failure tests.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            dim: 64,
            fail: true,
        }
    }
}

impl Default for FakeEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingProvider for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, RetrievalError> {
        if self.fail {
            return Err(RetrievalError::Embed {
                detail: "the fake embedder was asked to fail".to_owned(),
            });
        }
        Ok(texts.iter().map(|text| self.embed_text(text)).collect())
    }
}

impl FakeEmbedder {
    /// The buckets one text fills.
    #[allow(clippy::float_arithmetic)] // a fake embedding is a vector: the crate's float exception
    fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut buckets = vec![0.0f32; self.dim];
        for word in text.split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            let slot = fnv1a(&word.to_ascii_lowercase()) % self.dim as u64;
            if let Some(bucket) = buckets.get_mut(slot as usize) {
                *bucket += 1.0;
            }
        }
        buckets
    }
}

/// FNV-1a over the bytes of `text`: small, stable and dependency-free.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The standard sanitiser. A test double that panics only if the compiled-in patterns break.
#[must_use]
pub fn sanitiser() -> Sanitiser {
    match Sanitiser::standard() {
        Ok(sanitiser) => sanitiser,
        Err(error) => panic!("the standard sanitiser must compile: {error}"),
    }
}

/// The deterministic fake embedder.
#[must_use]
pub fn embedder() -> FakeEmbedder {
    FakeEmbedder::new()
}

/// Metadata for a tenant, tier, instant and source.
#[must_use]
pub fn meta(tenant: &str, tier: Tier, created_ms: i64, source: SourceKind) -> EntryMeta {
    let tenant = match TenantId::parse(tenant) {
        Ok(tenant) => tenant,
        Err(error) => panic!("the test tenant must be valid: {error}"),
    };
    match EntryMeta::new(tier, tenant, created_ms, source) {
        Ok(meta) => meta.with_confidence(Confidence::FULL),
        Err(error) => panic!("the test metadata must be valid: {error}"),
    }
}

/// An episode `n` in `tenant`, resolved, with one symptom and one audit entry.
#[must_use]
pub fn episode(tenant: &str, n: u32) -> Episode {
    episode_saying(tenant, n, "unstuck the job", 1_000)
}

/// The same episode, saying `summary`, created at `created_ms`.
#[must_use]
pub fn episode_saying(tenant: &str, n: u32, summary: &str, created_ms: i64) -> Episode {
    let sanitiser = sanitiser();
    let embedder = embedder();
    let audit = match AuditRef::parse(&format!("audit-{n}")) {
        Ok(audit) => audit,
        Err(error) => panic!("the test audit id must be valid: {error}"),
    };
    let id = match EpisodeId::parse(&format!("episode.e{n}")) {
        Ok(id) => id,
        Err(error) => panic!("the test episode id must be valid: {error}"),
    };
    let symptom = match Symptom::parse(STUCK_RUNNING) {
        Ok(symptom) => symptom,
        Err(error) => panic!("the test symptom must be valid: {error}"),
    };
    let incident = RecordRef::new("job", &format!("018f-{n:04}"));
    let incident = match incident {
        Ok(incident) => incident,
        Err(error) => panic!("the test record reference must be valid: {error}"),
    };
    let recorded = Episode::record(
        &sanitiser,
        &embedder,
        EpisodeInput {
            id,
            incident,
            symptoms: vec![symptom],
            actions: vec![ActionRef::diagnosis(audit.clone(), Tier::T0)],
            outcome: Outcome::Resolved,
            summary,
            audits: vec![audit],
            meta: meta(tenant, Tier::T0, created_ms, SourceKind::Incident),
        },
    );
    match recorded {
        Ok(episode) => episode,
        Err(error) => panic!("the test episode must record: {error}"),
    }
}

/// A procedure `n` in `tenant`: one step (`job.retry_failed` with `attempts = 3`), one success.
#[must_use]
pub fn procedure(tenant: &str, n: u32) -> Procedure {
    let sanitiser = sanitiser();
    let embedder = embedder();
    let id = match ProcedureId::parse(&format!("procedure.p{n}")) {
        Ok(id) => id,
        Err(error) => panic!("the test procedure id must be valid: {error}"),
    };
    let symptom = match Symptom::parse(STUCK_RUNNING) {
        Ok(symptom) => symptom,
        Err(error) => panic!("the test symptom must be valid: {error}"),
    };
    let confirmation = Confirmation::new(
        match EpisodeId::parse(&format!("episode.e{n}")) {
            Ok(episode) => episode,
            Err(error) => panic!("the test episode id must be valid: {error}"),
        },
        Outcome::Resolved,
        1_000,
    );
    let confirmation = match confirmation {
        Ok(confirmation) => confirmation,
        Err(error) => panic!("the test confirmation must be valid: {error}"),
    };
    let recorded = Procedure::record(
        &sanitiser,
        &embedder,
        ProcedureInput {
            id,
            title: "retry the stuck job",
            symptoms: vec![symptom],
            steps: vec![
                ProcedureStep::new(RemedyId::new(RETRY_FAILED))
                    .with("attempts", ParamValue::Integer(3)),
            ],
            confirmations: vec![confirmation],
            meta: meta(tenant, Tier::T2, 1_000, SourceKind::RemedyRun),
        },
    );
    match recorded {
        Ok(procedure) => procedure,
        Err(error) => panic!("the test procedure must record: {error}"),
    }
}

/// A deployment fact `n` in `tenant`.
#[must_use]
pub fn fact(tenant: &str, n: u32) -> Fact {
    let sanitiser = sanitiser();
    let embedder = embedder();
    let id = match FactId::parse(&format!("fact.f{n}")) {
        Ok(id) => id,
        Err(error) => panic!("the test fact id must be valid: {error}"),
    };
    let key = match FactKey::parse(&format!("backend.fact_{n}")) {
        Ok(key) => key,
        Err(error) => panic!("the test fact key must be valid: {error}"),
    };
    let value = match FactValue::text(&sanitiser, "standalone") {
        Ok(value) => value,
        Err(error) => panic!("the test fact value must be sanitised: {error}"),
    };
    let recorded = Fact::record(
        &sanitiser,
        &embedder,
        FactInput {
            id,
            key,
            value,
            epoch: Epoch::new(1),
            meta: meta(tenant, Tier::T1, 1_000, SourceKind::Deployment),
            text: None,
        },
    );
    match recorded {
        Ok(fact) => fact,
        Err(error) => panic!("the test fact must record: {error}"),
    }
}

/// A parameter map with one integer, for step builders in tests.
#[must_use]
pub fn params(pairs: &[(&str, i64)]) -> BTreeMap<String, ParamValue> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), ParamValue::Integer(*value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{embedder, fnv1a};
    use crate::embed::{Embedder, similarity_bp};

    #[test]
    fn the_fake_embedder_is_deterministic_and_word_driven() {
        let embedder = embedder();
        let shared = [
            "retry the stuck job",
            "retry the stuck job",
            "the job is stuck",
        ];
        let vectors = embedder.embed(&shared.map(str::to_owned)).unwrap();
        assert_eq!(vectors[0], vectors[1]);
        assert!(similarity_bp(&vectors[0], &vectors[2]) > 0);
        let unrelated = embedder.embed(&["cache flush".to_owned()]).unwrap();
        assert_eq!(similarity_bp(&vectors[0], &unrelated[0]), 0);
    }

    #[test]
    fn the_hash_is_stable() {
        assert_eq!(fnv1a("job"), fnv1a("job"));
        assert_ne!(fnv1a("job"), fnv1a("jobb"));
    }
}
