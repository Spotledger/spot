//! Recall: vector similarity plus structured filters, with provenance and the tier and tenant
//! gates WS13 requires.
//!
//! `recall` answers "what do I know that is like this, and where did it come from?" over the
//! three kinds. It is not a search engine over records — memory holds no record values — and it
//! never widens a reader's rights:
//!
//! - **Tenant.** Only the asking tenant's entries are considered, and an entry from another
//!   tenant is dropped even when a store hands it back.
//! - **Tier.** A procedure is readable only by an agent at the tier that learned it or above.
//!   `recall` omits what the reader may not read (no totals, no "hidden" count, so a refused
//!   read is indistinguishable from an absent entry); [`read_procedure`] is the loud path for a
//!   direct read and refuses with [`MemoryError::TierRefused`].
//! - **Life.** An entry past its TTL is not returned; the caller passes `now_ms`.
//!
//! Scores are integer basis points, so the order is exact and the same run gives the same order.

use std::collections::BTreeSet;

use spot_agent_tiers::Tier;
use spot_remedies::RemedyId;

use crate::embed::{Embedder, embed_one, similarity_bp};
use crate::error::MemoryError;
use crate::fact::Epoch;
use crate::id::{AuditRef, Symptom, TenantId};
use crate::meta::{Confidence, MemoryKind};
use crate::store::{Entry, MemoryStore};

/// The longest query this crate will embed, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;

/// The structured side of a recall: what the answer must look like, independent of similarity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    /// The kinds to consider; empty means every kind.
    pub kinds: BTreeSet<MemoryKind>,
    /// Only entries whose symptom signature names this tag.
    pub symptom: Option<Symptom>,
    /// Only entries that name this remedy: a procedure step or an episode action.
    pub remedy: Option<RemedyId>,
    /// Only entries created at or after this instant, in Unix milliseconds.
    pub since_ms: Option<i64>,
    /// Only entries created at or before this instant, in Unix milliseconds.
    pub until_ms: Option<i64>,
    /// Only entries at least this confident.
    pub min_confidence: Option<Confidence>,
}

impl Filters {
    /// No filter: every kind, every entry.
    #[must_use]
    pub fn any() -> Self {
        Self::default()
    }

    /// Only these kinds.
    #[must_use]
    pub fn kinds(kinds: &[MemoryKind]) -> Self {
        Self {
            kinds: kinds.iter().copied().collect(),
            ..Self::default()
        }
    }

    /// Whether `kind` is considered.
    #[must_use]
    pub fn wants(&self, kind: MemoryKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }

    /// Whether `entry` satisfies every structured filter.
    #[must_use]
    pub fn matches(&self, entry: &Entry) -> bool {
        if let Some(symptom) = &self.symptom
            && !entry
                .symptoms()
                .iter()
                .any(|candidate| candidate == symptom)
        {
            return false;
        }
        if let Some(remedy) = &self.remedy
            && !entry.remedies().contains(&remedy)
        {
            return false;
        }
        let created = entry.meta().created_ms;
        if let Some(since) = self.since_ms
            && created < since
        {
            return false;
        }
        if let Some(until) = self.until_ms
            && created > until
        {
            return false;
        }
        if let Some(minimum) = self.min_confidence
            && !entry.meta().confidence.is_at_least(minimum)
        {
            return false;
        }
        true
    }
}

/// Where a hit came from: the entry's own provenance, so an agent can cite it and an operator can
/// follow it to the audit chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The tenant whose namespace holds the entry.
    pub tenant: TenantId,
    /// The entry's kind.
    pub kind: MemoryKind,
    /// The entry's id.
    pub id: String,
    /// What produced the entry.
    pub source: crate::meta::SourceKind,
    /// When the entry was recorded, in Unix milliseconds.
    pub created_ms: i64,
    /// The tier the entry was learned at.
    pub tier: Tier,
    /// The audit entries behind an episode (empty for the other kinds).
    pub audits: Vec<AuditRef>,
    /// The remedies the entry names.
    pub remedies: Vec<RemedyId>,
    /// The deployment epoch a fact was read at (no other kind has one).
    pub epoch: Option<Epoch>,
}

/// One recalled entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecallHit {
    /// Where it came from.
    pub provenance: Provenance,
    /// The cosine similarity to the query, in integer basis points.
    pub score_bp: i32,
    /// The text to show: an episode's summary, a procedure's title, a fact's key and value.
    pub text: String,
    /// How sure the writer was.
    pub confidence: Confidence,
}

/// One recall request. Time is an argument, never read from a clock.
pub struct RecallRequest<'a> {
    /// The asking tenant: only its entries are considered.
    pub tenant: &'a TenantId,
    /// The asking agent's tier: procedures above it are omitted.
    pub agent_tier: Tier,
    /// The query text.
    pub query: &'a str,
    /// The structured filters.
    pub filters: Filters,
    /// How many hits at most.
    pub k: usize,
    /// The instant the read happens at, in Unix milliseconds, for TTL expiry.
    pub now_ms: i64,
}

/// Recalls the `k` best entries for `request`, best first.
///
/// An empty query or `k == 0` returns nothing without calling the embedder.
///
/// # Errors
///
/// [`MemoryError::QueryTooLong`] when the query is larger than [`MAX_QUERY_BYTES`], and
/// [`MemoryError::Embed`] when the provider fails.
pub fn recall(
    store: &dyn MemoryStore,
    embedder: &dyn Embedder,
    request: &RecallRequest<'_>,
) -> Result<Vec<RecallHit>, MemoryError> {
    if request.query.len() > MAX_QUERY_BYTES {
        return Err(MemoryError::QueryTooLong {
            bytes: request.query.len(),
            max: MAX_QUERY_BYTES,
        });
    }
    if request.k == 0 || request.query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let query = embed_one(embedder, request.query)?;
    let mut hits = Vec::new();
    for kind in MemoryKind::all() {
        if !request.filters.wants(kind) {
            continue;
        }
        for entry in store.list(request.tenant, kind) {
            // Defence in depth: a store that leaked another tenant's entry still cannot leak it
            // through recall.
            if entry.meta().tenant != *request.tenant {
                continue;
            }
            if entry.meta().is_expired(request.now_ms) {
                continue;
            }
            if kind == MemoryKind::Procedure && entry.meta().tier > request.agent_tier {
                continue;
            }
            if !request.filters.matches(&entry) {
                continue;
            }
            hits.push(hit_for(&entry, similarity_bp(&query, entry.vector())));
        }
    }
    hits.sort_by(|left, right| {
        right
            .score_bp
            .cmp(&left.score_bp)
            .then_with(|| left.provenance.kind.cmp(&right.provenance.kind))
            .then_with(|| left.provenance.id.cmp(&right.provenance.id))
    });
    hits.truncate(request.k);
    Ok(hits)
}

/// One entry's provenance, without the score.
#[must_use]
pub fn provenance_of(entry: &Entry) -> Provenance {
    let meta = entry.meta();
    let audits = match entry {
        Entry::Episode(episode) => episode.audits.clone(),
        Entry::Procedure(_) | Entry::Fact(_) => Vec::new(),
    };
    let epoch = match entry {
        Entry::Fact(fact) => Some(fact.epoch),
        Entry::Episode(_) | Entry::Procedure(_) => None,
    };
    Provenance {
        tenant: meta.tenant.clone(),
        kind: entry.kind(),
        id: entry.id_str().to_owned(),
        source: meta.source,
        created_ms: meta.created_ms,
        tier: meta.tier,
        audits,
        remedies: entry.remedies().into_iter().cloned().collect(),
        epoch,
    }
}

fn hit_for(entry: &Entry, score_bp: i32) -> RecallHit {
    RecallHit {
        provenance: provenance_of(entry),
        score_bp,
        text: entry.text(),
        confidence: entry.meta().confidence,
    }
}

/// Reads one procedure for an agent at `agent_tier`, at the instant `now_ms`.
///
/// This is the **only** direct read of a stored entry: there is no ungated read by kind, so a
/// procedure cannot be fetched past its tier or its TTL.
///
/// # Errors
///
/// [`MemoryError::TierRefused`] when the agent's tier is below the procedure's, and
/// [`MemoryError::EntryInvalid`] when no live procedure of that id is in the tenant's namespace
/// (an expired one reads as absent).
pub fn read_procedure(
    store: &dyn MemoryStore,
    tenant: &TenantId,
    agent_tier: Tier,
    id: &str,
    now_ms: i64,
) -> Result<crate::procedure::Procedure, MemoryError> {
    match store.get(tenant, MemoryKind::Procedure, id) {
        Some(Entry::Procedure(procedure))
            if procedure.meta.tenant == *tenant && !procedure.meta.is_expired(now_ms) =>
        {
            if procedure.readable_by(agent_tier) {
                Ok(procedure)
            } else {
                Err(MemoryError::TierRefused {
                    agent: agent_tier.as_str().to_owned(),
                    required: procedure.meta.tier.as_str().to_owned(),
                    procedure: procedure.id.to_string(),
                })
            }
        }
        _ => Err(MemoryError::EntryInvalid {
            kind: MemoryKind::Procedure.as_str().to_owned(),
            id: id.to_owned(),
            reason: "no procedure of that id is in this tenant's memory".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{Filters, RecallRequest, read_procedure, recall};
    use crate::meta::MemoryKind;
    use crate::store::{Entry, InMemoryStore, MemoryStore};
    use crate::testing::{embedder, episode_saying, fact, procedure};
    use spot_agent_tiers::Tier;

    fn tenant(name: &str) -> crate::id::TenantId {
        crate::id::TenantId::parse(name).unwrap()
    }

    fn store_with_episodes() -> InMemoryStore {
        let store = InMemoryStore::new();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                1,
                "the job stayed stuck in running",
                1_000,
            )))
            .unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                2,
                "the cache flush cleared the stale lease",
                2_000,
            )))
            .unwrap();
        store
            .put(Entry::Episode(episode_saying(
                "tenant_b",
                3,
                "the job stayed stuck in running",
                3_000,
            )))
            .unwrap();
        store
    }

    #[test]
    fn a_recall_ranks_by_similarity_and_reports_provenance() {
        let store = store_with_episodes();
        let request = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T2,
            query: "stuck in running job",
            filters: Filters::any(),
            k: 5,
            now_ms: 10_000,
        };
        let hits = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "the other tenant's episode is never returned"
        );
        assert_eq!(hits[0].provenance.id, "episode.e1");
        assert!(hits[0].score_bp > hits[1].score_bp);
        assert_eq!(hits[0].provenance.kind, MemoryKind::Episode);
        assert_eq!(hits[0].provenance.source, crate::meta::SourceKind::Incident);
        assert_eq!(hits[0].provenance.audits.len(), 1);
        assert_eq!(hits[0].provenance.epoch, None);
    }

    #[test]
    fn the_same_request_gives_the_same_order_every_time() {
        let store = store_with_episodes();
        let request = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T2,
            query: "stuck job lease",
            filters: Filters::any(),
            k: 5,
            now_ms: 10_000,
        };
        let first = recall(&store, &embedder(), &request).unwrap();
        let second = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn structured_filters_narrow_the_answer() {
        let store = store_with_episodes();
        let symptom = crate::id::Symptom::parse("job.stuck_running").unwrap();
        let request = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T2,
            query: "cache",
            filters: Filters {
                symptom: Some(symptom),
                ..Filters::any()
            },
            k: 5,
            now_ms: 10_000,
        };
        let hits = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(hits.len(), 2, "both episodes carry the symptom");

        let request = RecallRequest {
            filters: Filters {
                since_ms: Some(1_500),
                ..Filters::any()
            },
            ..request
        };
        let hits = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provenance.id, "episode.e2");

        let request = RecallRequest {
            filters: Filters::kinds(&[MemoryKind::Fact]),
            ..request
        };
        assert!(recall(&store, &embedder(), &request).unwrap().is_empty());
    }

    #[test]
    fn a_procedure_above_the_readers_tier_is_omitted_but_a_direct_read_refuses() {
        let store = InMemoryStore::new();
        store
            .put(Entry::Procedure(procedure("tenant_a", 1)))
            .unwrap();
        let low = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T0,
            query: "stuck job retry",
            filters: Filters::any(),
            k: 5,
            now_ms: 10_000,
        };
        assert!(recall(&store, &embedder(), &low).unwrap().is_empty());
        assert_eq!(
            read_procedure(&store, &tenant("tenant_a"), Tier::T0, "procedure.p1", 1_000)
                .unwrap_err()
                .code(),
            "agent_memory.tier_refused"
        );
        let high = RecallRequest {
            agent_tier: Tier::T2,
            ..low
        };
        assert_eq!(recall(&store, &embedder(), &high).unwrap().len(), 1);
        assert!(
            read_procedure(&store, &tenant("tenant_a"), Tier::T2, "procedure.p1", 1_000).is_ok()
        );
    }

    #[test]
    fn an_expired_procedure_cannot_be_read_directly() {
        let store = InMemoryStore::new();
        let mut short = procedure("tenant_a", 1);
        short.meta.ttl_ms = Some(500);
        let created = short.meta.created_ms;
        store.put(Entry::Procedure(short)).unwrap();
        let tenant = tenant("tenant_a");
        assert!(read_procedure(&store, &tenant, Tier::T3, "procedure.p1", created).is_ok());
        assert_eq!(
            read_procedure(&store, &tenant, Tier::T3, "procedure.p1", created + 500)
                .unwrap_err()
                .code(),
            "agent_memory.entry_invalid"
        );
    }

    #[test]
    fn an_expired_entry_is_not_recalled() {
        let store = InMemoryStore::new();
        let mut short = episode_saying("tenant_a", 1, "the job stayed stuck", 1_000);
        short.meta.ttl_ms = Some(500);
        store.put(Entry::Episode(short)).unwrap();
        let mut long = episode_saying("tenant_a", 2, "the job stayed stuck", 1_000);
        long.meta.ttl_ms = Some(100_000);
        store.put(Entry::Episode(long)).unwrap();
        let request = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T0,
            query: "stuck job",
            filters: Filters::any(),
            k: 5,
            now_ms: 2_000,
        };
        let hits = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(hits.len(), 1, "the entry whose TTL elapsed is gone");
        assert_eq!(hits[0].provenance.id, "episode.e2");
    }

    #[test]
    fn a_fact_reports_the_epoch_it_was_read_at() {
        let store = InMemoryStore::new();
        store.put(Entry::Fact(fact("tenant_a", 1))).unwrap();
        let request = RecallRequest {
            tenant: &tenant("tenant_a"),
            agent_tier: Tier::T0,
            query: "backend fact 1",
            filters: Filters::any(),
            k: 5,
            now_ms: 10_000,
        };
        let hits = recall(&store, &embedder(), &request).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].provenance.epoch.map(crate::fact::Epoch::get),
            Some(1)
        );
    }
}
