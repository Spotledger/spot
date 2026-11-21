//! The memory of one tenant, as the agent path uses it: recall (tier-checked), remember (through
//! the write filter), the candidate-remedy queue and the retention pass.
//!
//! Everything here runs as the **calling agent** or, for retention, as the platform on its own
//! schedule (retention touches only the tenant's memory, never a business record). The agent's tier
//! is taken from the authenticated principal (`McpPrincipal::tier`, which the agent directory filled
//! from the stored registration), never from an argument.
//!
//! The store is synchronous and blocks on its own runtime, so the async methods run it on a
//! blocking-pool thread.

use std::path::PathBuf;
use std::sync::Arc;

use spot_agent_memory::{
    CandidateId, CandidateRemedy, Embedder, Entry, EntryRef, Epoch, Filters, MAX_QUERY_BYTES,
    MemoryError, MemoryKind, MemoryStore, RecallHit, RetentionPolicy, RetentionReport, TenantId,
    Tier, apply_retention, plan_retention, vector_is_usable,
};
use spot_agent_memory_pg::{PgMemoryStore, RecallQuery, RecencyBoost, StoredCandidate};
use spot_core::Clock;
use spot_data_policy::Holds;
use spot_mcp::principal::McpPrincipal;

use super::filter::WriteFilter;

/// Why a call was refused or failed. The words are plain; none carries a value or a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryFault {
    /// The caller is not an agent (or is the platform): memory serves the agent path.
    AgentsOnly,
    /// The entry is above the caller's tier.
    TierRefused,
    /// The entry belongs to another tenant.
    OtherTenant,
    /// The arguments are out of range (the sentence names the limit).
    Invalid(&'static str),
    /// The write filter refused: the class of business data found.
    BusinessData(&'static str),
    /// The embedding provider could not answer.
    Embedder,
    /// The memory database could not answer.
    Store,
    /// The legal-hold file could not be read, so retention did nothing.
    Holds,
}

impl MemoryFault {
    /// A short word for logs and counters (no dots: it is not an error code).
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::AgentsOnly => "agents_only",
            Self::TierRefused => "tier_refused",
            Self::OtherTenant => "other_tenant",
            Self::Invalid(_) => "invalid",
            Self::BusinessData(_) => "business_data",
            Self::Embedder => "embedder_unavailable",
            Self::Store => "store_unavailable",
            Self::Holds => "holds_unreadable",
        }
    }
}

/// Limits of the recall path.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most hits one recall returns.
    pub max_k: usize,
}

/// What an [`AgentMemory`] is built from.
pub struct Parts {
    /// The tenant.
    pub tenant: TenantId,
    /// The store on the tenant's memory database.
    pub store: Arc<PgMemoryStore>,
    /// The embedding provider (width `EMBEDDING_DIM`).
    pub embedder: Arc<dyn Embedder>,
    /// The write filter.
    pub filter: WriteFilter,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// The retention policy.
    pub retention: RetentionPolicy,
    /// The legal-hold file, when the site has one.
    pub holds_file: Option<PathBuf>,
    /// The recall limits.
    pub limits: Limits,
}

/// One tenant's memory.
pub struct AgentMemory {
    tenant: TenantId,
    store: Arc<PgMemoryStore>,
    embedder: Arc<dyn Embedder>,
    filter: WriteFilter,
    clock: Arc<dyn Clock>,
    retention: RetentionPolicy,
    holds_file: Option<PathBuf>,
    limits: Limits,
}

impl AgentMemory {
    /// A memory over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self {
            tenant: parts.tenant,
            store: parts.store,
            embedder: parts.embedder,
            filter: parts.filter,
            clock: parts.clock,
            retention: parts.retention,
            holds_file: parts.holds_file,
            limits: parts.limits,
        }
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The store (for the operator tools and tests that need the typed accessors).
    #[must_use]
    pub fn store(&self) -> &Arc<PgMemoryStore> {
        &self.store
    }

    /// The write filter, for its counters.
    #[must_use]
    pub fn filter(&self) -> &WriteFilter {
        &self.filter
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_utc().unix_timestamp().saturating_mul(1_000)
    }

    /// The agent tier of `caller`: an agent, never the platform.
    fn agent_tier(caller: &McpPrincipal) -> Result<Tier, MemoryFault> {
        caller
            .assert_not_system()
            .map_err(|_| MemoryFault::AgentsOnly)?;
        caller.tier.ok_or(MemoryFault::AgentsOnly)
    }

    /// Recalls the `k` best entries for `query`, as the calling agent's tier allows: a procedure
    /// learned above the caller's tier is not returned, counted or hinted at (the gate is in the
    /// SQL), and only this tenant's memory is read.
    ///
    /// # Errors
    ///
    /// [`MemoryFault::AgentsOnly`], [`MemoryFault::Invalid`], [`MemoryFault::Embedder`] or
    /// [`MemoryFault::Store`].
    pub async fn recall(
        &self,
        caller: &McpPrincipal,
        query: &str,
        k: usize,
        kinds: &[MemoryKind],
    ) -> Result<Vec<RecallHit>, MemoryFault> {
        let tier = Self::agent_tier(caller)?;
        if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES {
            return Err(MemoryFault::Invalid("the query is empty or too long"));
        }
        if k == 0 || k > self.limits.max_k {
            return Err(MemoryFault::Invalid("k is out of range"));
        }
        let (store, embedder, tenant) = (
            Arc::clone(&self.store),
            Arc::clone(&self.embedder),
            self.tenant.clone(),
        );
        let (query, kinds, now_ms) = (query.to_owned(), kinds.to_vec(), self.now_ms());
        tokio::task::spawn_blocking(move || {
            recall_blocking(&store, &*embedder, &tenant, tier, &query, k, &kinds, now_ms)
        })
        .await
        .map_err(|_| MemoryFault::Store)?
    }

    /// Writes `entry` for the calling agent, through the write filter.
    ///
    /// # Errors
    ///
    /// [`MemoryFault::AgentsOnly`], [`MemoryFault::OtherTenant`], [`MemoryFault::TierRefused`] (an
    /// agent never writes above its own tier), [`MemoryFault::BusinessData`] with the class the
    /// filter found, or [`MemoryFault::Store`].
    pub async fn remember(&self, caller: &McpPrincipal, entry: Entry) -> Result<(), MemoryFault> {
        let tier = Self::agent_tier(caller)?;
        if entry.meta().tenant != self.tenant {
            return Err(MemoryFault::OtherTenant);
        }
        if entry.meta().tier > tier {
            return Err(MemoryFault::TierRefused);
        }
        let agent = caller.agent_id.clone().unwrap_or_default();
        self.filter
            .admit(&caller.audit_actor(), &agent, &entry)
            .await
            .map_err(|refusal| MemoryFault::BusinessData(refusal.class))?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.put(entry))
            .await
            .map_err(|_| MemoryFault::Store)?
            .map_err(|_: MemoryError| MemoryFault::Store)
    }

    /// Queues a learned-remedy candidate (blocking). The store refuses a promotion with no CI run.
    ///
    /// # Errors
    ///
    /// [`MemoryFault::Store`].
    pub fn record_candidate(&self, candidate: &CandidateRemedy) -> Result<(), MemoryFault> {
        self.store
            .record_candidate(candidate, self.now_ms())
            .map_err(|_| MemoryFault::Store)
    }

    /// The queued candidate `id`, as stored (blocking).
    ///
    /// # Errors
    ///
    /// [`MemoryFault::Store`].
    pub fn candidate(&self, id: &CandidateId) -> Result<Option<StoredCandidate>, MemoryFault> {
        self.store.candidate(id).map_err(|_| MemoryFault::Store)
    }

    /// Whether the learned remedy `id` may run: **only a candidate CI promoted** may. An unknown
    /// candidate is not usable (blocking).
    ///
    /// # Errors
    ///
    /// [`MemoryFault::Store`].
    pub fn remedy_usable(&self, id: &CandidateId) -> Result<bool, MemoryFault> {
        Ok(self.candidate(id)?.is_some_and(|stored| stored.is_usable()))
    }

    /// The legal holds now: read from the file every pass, so a new hold applies at the next pass.
    fn holds(&self) -> Result<Holds, MemoryFault> {
        match &self.holds_file {
            None => Ok(Holds::empty()),
            Some(path) => {
                let text = std::fs::read_to_string(path).map_err(|_| MemoryFault::Holds)?;
                if text.trim().is_empty() {
                    return Ok(Holds::empty());
                }
                Holds::from_toml(&text).map_err(|_| MemoryFault::Holds)
            }
        }
    }

    /// One retention pass at `now_ms` (blocking): plan, then apply exactly the plan. A legal hold
    /// saves what it covers; **a hold file that cannot be read stops the pass** (retention that
    /// cannot see the holds deletes nothing).
    ///
    /// # Errors
    ///
    /// [`MemoryFault::Holds`], [`MemoryFault::Store`] or [`MemoryFault::Embedder`].
    pub fn run_retention(&self, now_ms: i64) -> Result<RetentionReport, MemoryFault> {
        let holds = self.holds()?;
        let store: &dyn MemoryStore = &*self.store;
        let plan = plan_retention(store, &self.tenant, &self.retention, now_ms, &holds)
            .map_err(|_| MemoryFault::Store)?;
        apply_retention(store, &*self.embedder, &plan, Epoch::new(0)).map_err(|error| match error {
            MemoryError::Embed { .. } => MemoryFault::Embedder,
            _ => MemoryFault::Store,
        })
    }

    /// One retention pass at the clock's now (blocking).
    ///
    /// # Errors
    ///
    /// As [`run_retention`](Self::run_retention).
    pub fn run_retention_now(&self) -> Result<RetentionReport, MemoryFault> {
        self.run_retention(self.now_ms())
    }
}

/// How many entries a report removed in total.
#[must_use]
pub fn removed(report: &RetentionReport) -> usize {
    report.expired.len() + report.compacted.len()
}

/// The ids a report removed, for an operator's log line (ids only, never text).
#[must_use]
pub fn removed_ids(report: &RetentionReport) -> Vec<&EntryRef> {
    report
        .expired
        .iter()
        .chain(report.compacted.iter())
        .collect()
}

/// The blocking half of recall: embed the query, then ask the store (tier and tenant gates are in
/// its SQL).
#[allow(clippy::too_many_arguments)]
fn recall_blocking(
    store: &PgMemoryStore,
    embedder: &dyn Embedder,
    tenant: &TenantId,
    tier: Tier,
    query: &str,
    k: usize,
    kinds: &[MemoryKind],
    now_ms: i64,
) -> Result<Vec<RecallHit>, MemoryFault> {
    let mut vectors = embedder
        .embed(&[query.to_owned()])
        .map_err(|_| MemoryFault::Embedder)?;
    if vectors.len() != 1 || !vector_is_usable(&vectors[0]) {
        return Err(MemoryFault::Embedder);
    }
    let vector = vectors.remove(0);
    let filters = if kinds.is_empty() {
        Filters::any()
    } else {
        Filters::kinds(kinds)
    };
    store
        .recall(&RecallQuery {
            tenant,
            agent_tier: tier,
            vector: &vector,
            filters: &filters,
            k,
            now_ms,
            recency: RecencyBoost::DEFAULT,
        })
        .map_err(|_| MemoryFault::Store)
}
