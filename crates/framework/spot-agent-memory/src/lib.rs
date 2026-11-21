#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! `spot-agent-memory`: the agent's memory model and logic (WP-AI-09.T1, blueprint WS13, ADR-26).
//!
//! An agent that repairs a system has to remember three different things, and this crate models
//! exactly those:
//!
//! - **[`episode`]**: what happened — the incident, its symptom tags, the actions taken with their
//!   audit ids, and how it ended. Episodic memory never crosses a tenant.
//! - **[`procedure`]**: how to fix it — an ordered list of `spot-remedies` steps with their
//!   parameters, and the incidents that confirmed it. A procedure is readable only by an agent at
//!   the tier that learned it or above.
//! - **[`fact`]**: what the installation is — a deployment fact keyed by the
//!   `spot-deployment-model` epoch it was read at, so knowledge that lags the deployment is
//!   visible as stale.
//!
//! Each entry carries metadata (tier, tenant, creation instant, TTL, confidence, source) and an
//! embedding from the host's [`embed::Embedder`], which is `spot-retrieval`'s embedding port.
//!
//! # The two promises
//!
//! **No business data in the memory namespace.** Memory references a record by type and id and
//! nothing else: [`id::RecordRef`] is the only way it names one, [`sanitiser::Summary`] is the
//! only free text an entry can hold and only [`sanitiser::Sanitiser`] produces it, and no entry
//! type has a field a document payload could sit in. `Sanitiser::check` scans text against the
//! pii, financial and secret field classes (`spot-redact`) and this crate's amount patterns; a
//! secret or financial value **rejects** the text, and personal data is redacted.
//!
//! **A learned remedy is blocked until CI passes.** [`learning::propose_remedy`] turns a repeated
//! successful procedure into a [`learning::CandidateRemedy`] in the remedy spec format, with a
//! self-test scenario skeleton, in the `candidates/` queue. [`learning::can_use`] is false unless
//! the candidate is `Promoted`, only a [`learning::PromotionPort`] promotion promotes it, and that
//! promotion records the CI run id. [`learning::UsableRemedy`] — what an unattended agent runs —
//! has one constructor and it takes the promoted candidate, so an unproven remedy does not
//! compile.
//!
//! # Shape
//!
//! - [`store`]: the [`store::MemoryStore`] trait (`put`, `get`, `list`, `remove`, `tenants`) and a
//!   deterministic in-memory implementation. The PostgreSQL/pgvector backend is WP-AI-09.T2 and
//!   plugs in behind this trait; no SQL, pool or vector-extension type is in this crate.
//! - [`recall`]: vector similarity plus structured filters (symptom, remedy, time, confidence,
//!   kind), with provenance, the tier gate on procedures and the tenant gate on episodes.
//! - [`retention`]: per-kind TTLs and size caps, deterministic compaction of old episodes into
//!   aggregate statistics, legal holds through `spot-data-policy`, and tenant export and deletion.
//! - [`learning`]: the candidate queue, the CI status ladder, the promotion port and the type that
//!   only a promotion produces.
//!
//! # Purity
//!
//! No storage backend, no network, no model call, no clock (the time is always an argument), no
//! random source, no `unsafe`, no `unwrap`/`expect` outside tests. Money, quantities, instants,
//! confidences and scores are integers: the only float in the crate is an embedding vector and the
//! cosine over two of them, confined to [`embed`] the way `spot-retrieval` confines its own.
//!
//! # Example
//!
//! ```
//! use spot_agent_memory::*;
//!
//! # fn main() -> Result<(), MemoryError> {
//! let sanitiser = Sanitiser::standard()?;
//! let embedder = FakeEmbedder::new();
//! let tenant = TenantId::parse("tenant_a")?;
//! let meta = EntryMeta::new(Tier::T0, tenant.clone(), 1_000, SourceKind::Incident)?;
//! let episode = Episode::record(
//!     &sanitiser,
//!     &embedder,
//!     EpisodeInput {
//!         id: EpisodeId::parse("episode.job.one")?,
//!         incident: RecordRef::new("job", "018f-0001")?,
//!         symptoms: vec![Symptom::parse("job.stuck_running")?],
//!         actions: vec![ActionRef::diagnosis(AuditRef::parse("audit-1")?, Tier::T0)],
//!         outcome: Outcome::Resolved,
//!         summary: "the running job was unstuck and finished",
//!         audits: vec![AuditRef::parse("audit-1")?],
//!         meta,
//!     },
//! )?;
//!
//! let store = InMemoryStore::new();
//! store.put(Entry::Episode(episode))?;
//!
//! let hits = recall(
//!     &store,
//!     &embedder,
//!     &RecallRequest {
//!         tenant: &tenant,
//!         agent_tier: Tier::T0,
//!         query: "a running job that is stuck",
//!         filters: Filters::any(),
//!         k: 5,
//!         now_ms: 2_000,
//!     },
//! )?;
//! assert_eq!(hits.len(), 1);
//! assert_eq!(hits[0].provenance.kind, MemoryKind::Episode);
//!
//! // A record's value never enters memory.
//! assert!(sanitiser.check("the invoice total was Rs 12,500").is_err());
//! # Ok(())
//! # }
//! ```

pub mod candidate;
pub mod embed;
pub mod episode;
pub mod error;
pub mod fact;
pub mod id;
pub mod learning;
pub mod meta;
pub mod procedure;
pub mod recall;
pub mod retention;
pub mod sanitiser;
pub mod store;
pub mod testing;

pub use candidate::{
    CandidateBundle, CandidateQueue, InMemoryCandidateQueue, ScenarioSkeleton, SkeletonStep,
    UndoExpectation,
};
pub use embed::{EMBEDDING_DIM, Embedder, cosine, similarity_bp, vector_is_usable};
pub use episode::{ActionRef, Episode, EpisodeInput, Outcome};
pub use error::{CODES, MemoryError, codes};
pub use fact::{EpisodeAggregate, Epoch, Fact, FactInput, FactValue};
pub use id::{
    AuditRef, CandidateId, CiRunId, EpisodeId, FactId, FactKey, ProcedureId, RecordRef, Symptom,
    TenantId,
};
pub use learning::{
    CandidateRemedy, CandidateStatus, CiOutcome, CiPromotion, LearningPolicy, Promoted,
    PromotionPort, UsableRemedy, can_use, propose_remedy,
};
pub use meta::{Confidence, EntryMeta, MemoryKind, SourceKind};
pub use procedure::{Confirmation, ParamValue, Procedure, ProcedureInput, ProcedureStep};
pub use recall::{
    Filters, MAX_QUERY_BYTES, Provenance, RecallHit, RecallRequest, provenance_of, read_procedure,
    recall,
};
pub use retention::{
    DAY_MS, DeletionReport, EntryRef, KindPolicy, RetentionAction, RetentionPlan, RetentionPolicy,
    RetentionReport, TenantExport, apply_retention, delete_tenant, export_tenant, plan_retention,
};
pub use sanitiser::{FieldClass, MAX_CONTENT_BYTES, Sanitiser, SanitiserError, Summary};
pub use spot_agent_tiers::Tier;
pub use spot_remedies::RemedyId;
pub use store::{Entry, InMemoryStore, MemoryStore};
pub use testing::FakeEmbedder;

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-agent-memory";
