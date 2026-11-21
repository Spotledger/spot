//! Hybrid recall in one statement: vector similarity, the structured filters and a recency
//! boost, over the three tables at once.
//!
//! # Why the database does the ranking
//!
//! `spot_agent_memory::recall` ranks whatever a [`MemoryStore`](spot_agent_memory::MemoryStore)
//! hands back, which for a durable store means reading every entry of the tenant into this
//! process. This module answers the same question with one parameterised statement: the
//! `pgvector` index finds the nearest embeddings, the metadata filters run in the same query
//! (symptom and remedy on `text[]` columns with GIN indexes, creation instant and confidence as
//! range filters), and only `k` rows cross the wire.
//!
//! # The score
//!
//! `score_bp` is integer basis points, exactly as
//! [`RecallHit::score_bp`](spot_agent_memory::RecallHit) is: the statement rounds
//! `(1 - cosine_distance) * 10_000` once, inside the database where the vector type lives, and
//! adds the recency boost — an integer that is [`RecencyBoost::cap_bp`] for an entry created at
//! `now_ms` and falls linearly to `0` at [`RecencyBoost::window_ms`] of age. Nothing in this
//! crate computes with a float; the one rounding happens in SQL beside the extension.
//!
//! # The gates, in SQL as in the model
//!
//! - **Tenant**: every branch filters `tenant = $2`, and the request's tenant must be the
//!   store's (a mismatch is refused, not ignored).
//! - **Tier**: a procedure branch carries `tier <= $3`, so a procedure learned above the
//!   reader's tier is never returned — it is not counted, skipped or hinted at.
//! - **Life**: `ttl_ms IS NULL OR ttl_ms > now_ms - created_ms` (a form with no sum that can overflow), so an expired entry is absent.
//! - **Kind**: the request's kinds are one `text[]` parameter every branch tests, so a recall
//!   that wants only facts runs the episode and procedure branches as no-ops.
//!
//! # The plan
//!
//! [`plan`] returns `EXPLAIN` for the same statement and the same parameters, so
//! an operator (and this crate's gated test at 100 000 rows) can assert that the recall uses the
//! vector index rather than scanning the table. It is the same SQL: a plan for a different
//! statement would prove nothing.

use spot_agent_memory::{
    AuditRef, Filters, MemoryError, MemoryKind, Provenance, RecallHit, RemedyId, TenantId, Tier,
};
use tokio_postgres::Client;

use crate::codec::{confidence_from_i16, corrupt, epoch_from_i64, remedy_from_text, tier_from_i16};
use crate::error::store;

/// The most hits one recall returns. A larger `k` is clamped to this, so one call cannot read
/// three times a tenant's embeddings.
pub const MAX_RECALL_K: usize = 100;

/// The most basis points a recency boost may add.
pub const MAX_BOOST_BP: i32 = 10_000;

/// The widest recency window, in milliseconds (about 100 years).
pub const MAX_WINDOW_MS: i64 = crate::row::MAX_TTL_MS;

/// How much a recent entry is boosted, and over what window.
///
/// The boost is an integer added to the similarity in basis points: an entry created at the
/// request's `now_ms` gains [`cap_bp`](Self::cap_bp), and the gain falls to zero at
/// [`window_ms`](Self::window_ms) of age. With the defaults (500 basis points over 30 days) a
/// fresher entry outranks a marginally closer stale one, which is the trade WS13 asks for: the
/// agent should act on what is still true.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecencyBoost {
    /// The most an entry can gain, in basis points.
    pub cap_bp: i32,
    /// The age at which the gain reaches zero, in milliseconds.
    pub window_ms: i64,
}

impl RecencyBoost {
    /// No boost at all: ranking is pure similarity, exactly as `spot_agent_memory::recall`
    /// behaves.
    pub const NONE: Self = Self {
        cap_bp: 0,
        window_ms: 1,
    };

    /// The default boost: up to 500 basis points over a 30-day window.
    pub const DEFAULT: Self = Self {
        cap_bp: 500,
        window_ms: 30 * 24 * 60 * 60 * 1_000,
    };

    /// A boost of `cap_bp` over `window_ms`, refusing a negative cap and a window below one
    /// millisecond (the divisor).
    ///
    /// # Errors
    ///
    /// [`MemoryError::Store`] when the cap is negative or the window is not positive.
    pub fn new(cap_bp: i32, window_ms: i64) -> Result<Self, MemoryError> {
        if !(0..=MAX_BOOST_BP).contains(&cap_bp) || !(1..=MAX_WINDOW_MS).contains(&window_ms) {
            return Err(MemoryError::Store {
                detail:
                    "a recency boost is 0 to 10000 basis points over a window of 1 ms to 100 years"
                        .to_owned(),
            });
        }
        Ok(Self { cap_bp, window_ms })
    }
}

impl Default for RecencyBoost {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One recall request, as the store answers it.
///
/// The query text is embedded by the caller ([`recall`](spot_agent_memory::recall) does the same
/// with the host's provider) and arrives here as a vector: this crate never calls a model, and a
/// store that embedded its own query would be a second, silent provider.
pub struct RecallQuery<'a> {
    /// The asking tenant; must be the store's own.
    pub tenant: &'a TenantId,
    /// The asking agent's tier: procedures above it are not returned.
    pub agent_tier: Tier,
    /// The embedded query, [`EMBEDDING_DIM`](spot_agent_memory::EMBEDDING_DIM) wide.
    pub vector: &'a [f32],
    /// The structured filters.
    pub filters: &'a Filters,
    /// How many hits at most.
    pub k: usize,
    /// The instant the read happens at, in Unix milliseconds: TTL expiry and the recency boost.
    pub now_ms: i64,
    /// The recency boost the caller wants.
    pub recency: RecencyBoost,
}

/// The statement. Every value is a parameter; the only identifiers are this crate's.
///
/// Each kind is one parenthesised branch that asks the extension for its `k` nearest rows
/// (`ORDER BY embedding <=> $1 LIMIT $5`), which is the only shape an HNSW or IVFFlat index can
/// serve: the index scan has to sit directly under a `LIMIT` on the distance. The branches are
/// then merged and re-ranked by similarity plus the recency boost, over at most `3k` rows.
///
/// What that costs, and why it is the right trade: an entry whose similarity is more than
/// [`RecencyBoost::cap_bp`] below its branch's k-th neighbour cannot enter the answer even with a
/// full boost, so the only rows the merge can miss are ones the caller asked for by `k` anyway.
/// The alternative — ranking the whole union by a computed score — cannot use the vector index at
/// all, and would read every embedding of the tenant for every recall.
const RECALL_SQL: &str = r#"
WITH candidates AS (
    (
        SELECT 'episode'::text AS kind,
               e.id,
               e.summary AS display,
               e.tier,
               e.source,
               e.created_ms,
               e.confidence_bp,
               e.audits,
               e.remedies,
               NULL::bigint AS epoch,
               e.embedding <=> $1::text::vector AS distance
        FROM spot_memory_episode e
        WHERE e.tenant = $2
          AND 'episode' = ANY($11)
          AND ($6::text IS NULL OR $6::text = ANY(e.symptoms))
          AND ($7::text IS NULL OR $7::text = ANY(e.remedies))
          AND ($8::bigint IS NULL OR e.created_ms >= $8)
          AND ($9::bigint IS NULL OR e.created_ms <= $9)
          AND ($10::smallint IS NULL OR e.confidence_bp >= $10)
          AND (e.ttl_ms IS NULL OR e.ttl_ms > $4::bigint - e.created_ms)
        ORDER BY distance
        LIMIT $5
    )
    UNION ALL
    (
        SELECT 'procedure'::text,
               p.id,
               p.title,
               p.tier,
               p.source,
               p.created_ms,
               p.confidence_bp,
               ARRAY[]::text[],
               p.remedies,
               NULL::bigint,
               p.embedding <=> $1::text::vector
        FROM spot_memory_procedure p
        WHERE p.tenant = $2
          AND 'procedure' = ANY($11)
          AND p.tier <= $3
          AND ($6::text IS NULL OR $6::text = ANY(p.symptoms))
          AND ($7::text IS NULL OR $7::text = ANY(p.remedies))
          AND ($8::bigint IS NULL OR p.created_ms >= $8)
          AND ($9::bigint IS NULL OR p.created_ms <= $9)
          AND ($10::smallint IS NULL OR p.confidence_bp >= $10)
          AND (p.ttl_ms IS NULL OR p.ttl_ms > $4::bigint - p.created_ms)
        ORDER BY 11
        LIMIT $5
    )
    UNION ALL
    (
        SELECT 'fact'::text,
               f.id,
               f.display_text,
               f.tier,
               f.source,
               f.created_ms,
               f.confidence_bp,
               ARRAY[]::text[],
               ARRAY[]::text[],
               f.epoch,
               f.embedding <=> $1::text::vector
        FROM spot_memory_fact f
        WHERE f.tenant = $2
          AND 'fact' = ANY($11)
          -- A fact carries no symptom and no remedy, so a filter on either excludes every fact,
          -- exactly as `Filters::matches` does in the model.
          AND $6::text IS NULL
          AND $7::text IS NULL
          AND ($8::bigint IS NULL OR f.created_ms >= $8)
          AND ($9::bigint IS NULL OR f.created_ms <= $9)
          AND ($10::smallint IS NULL OR f.confidence_bp >= $10)
          AND (f.ttl_ms IS NULL OR f.ttl_ms > $4::bigint - f.created_ms)
        ORDER BY 11
        LIMIT $5
    )
),
scored AS (
    SELECT kind, id, display, tier, source, created_ms, confidence_bp, audits, remedies, epoch,
           (round((1.0 - distance) * 10000))::int AS similarity_bp,
           LEAST($12::int,
                 GREATEST(0,
                          $12::int - (LEAST(GREATEST($4::bigint - created_ms, 0), $13::bigint)
                                      * $12::int) / $13::bigint))::int AS boost_bp
    FROM candidates
)
SELECT kind, id, display, tier, source, created_ms, confidence_bp, audits, remedies, epoch,
       similarity_bp + boost_bp AS score_bp
FROM scored
ORDER BY score_bp DESC,
         CASE kind WHEN 'episode' THEN 0 WHEN 'procedure' THEN 1 ELSE 2 END,
         id
LIMIT $5
"#;

/// The embedded query as the extension's text input form.
///
/// The width and the finiteness are checked here, so a vector the column would refuse fails with
/// this crate's error rather than the driver's.
fn vector_param(dim: usize, vector: &[f32]) -> Result<String, MemoryError> {
    if vector.len() != dim || !vector.iter().all(|value| value.is_finite()) {
        return Err(MemoryError::EntryInvalid {
            kind: "embedding".to_owned(),
            id: "vector".to_owned(),
            reason: format!("a recall query is {dim} finite values"),
        });
    }
    let mut text = String::with_capacity(vector.len() * 8 + 2);
    text.push('[');
    for (slot, value) in vector.iter().enumerate() {
        if slot > 0 {
            text.push(',');
        }
        text.push_str(&format!("{value}"));
    }
    text.push(']');
    Ok(text)
}

/// The parameters one recall statement takes, in `$n` order.
struct Bound {
    vector: String,
    tenant: String,
    agent_tier: i16,
    now_ms: i64,
    limit: i64,
    symptom: Option<String>,
    remedy: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    min_confidence: Option<i16>,
    kinds: Vec<String>,
    cap_bp: i32,
    window_ms: i64,
}

impl Bound {
    fn of(query: &RecallQuery<'_>, dim: usize) -> Result<Self, MemoryError> {
        if query.k == 0 {
            return Err(MemoryError::Store {
                detail: "a recall asks for at least one hit".to_owned(),
            });
        }
        // A larger `k` is clamped, not refused: the answer is the best `MAX_RECALL_K`.
        let limit = i64::try_from(query.k.min(MAX_RECALL_K)).unwrap_or(1);
        Ok(Self {
            vector: vector_param(dim, query.vector)?,
            tenant: query.tenant.as_str().to_owned(),
            agent_tier: crate::codec::tier_to_i16(query.agent_tier),
            now_ms: query.now_ms.clamp(0, crate::row::MAX_TIME_MS),
            limit,
            symptom: query
                .filters
                .symptom
                .as_ref()
                .map(|symptom| symptom.as_str().to_owned()),
            remedy: query.filters.remedy.as_ref().map(ToString::to_string),
            since: query.filters.since_ms,
            until: query.filters.until_ms,
            min_confidence: query
                .filters
                .min_confidence
                .map(crate::codec::confidence_to_i16),
            kinds: kinds_param(query.filters),
            cap_bp: query.recency.cap_bp.clamp(0, MAX_BOOST_BP),
            window_ms: query.recency.window_ms.clamp(1, MAX_WINDOW_MS),
        })
    }

    /// The statement's parameters, in `$n` order.
    fn params(&self) -> [&(dyn tokio_postgres::types::ToSql + Sync); 13] {
        [
            &self.vector,
            &self.tenant,
            &self.agent_tier,
            &self.now_ms,
            &self.limit,
            &self.symptom,
            &self.remedy,
            &self.since,
            &self.until,
            &self.min_confidence,
            &self.kinds,
            &self.cap_bp,
            &self.window_ms,
        ]
    }
}

/// The kinds a recall considers, as the `text[]` the statement tests.
fn kinds_param(filters: &Filters) -> Vec<String> {
    MemoryKind::all()
        .iter()
        .filter(|kind| filters.wants(**kind))
        .map(|kind| kind.as_str().to_owned())
        .collect()
}

/// Recalls the `k` best entries for `query`, best first.
///
/// # Errors
///
/// [`MemoryError::EntryInvalid`] when the query vector is not
/// [`EMBEDDING_DIM`](spot_agent_memory::EMBEDDING_DIM) finite values, [`MemoryError::Store`]
/// when the request names another tenant or the memory database refuses.
pub(crate) async fn recall(
    client: &Client,
    store_tenant: &TenantId,
    query: &RecallQuery<'_>,
    dim: usize,
) -> Result<Vec<RecallHit>, MemoryError> {
    if query.tenant != store_tenant {
        return Err(MemoryError::Store {
            detail: "this memory store is bound to another tenant".to_owned(),
        });
    }
    if query.k == 0 {
        return Ok(Vec::new());
    }
    let bound = Bound::of(query, dim)?;
    let rows = client
        .query(RECALL_SQL, &bound.params())
        .await
        .map_err(|error| store(&error))?;
    rows.iter()
        .map(|row| hit_from_row(row, store_tenant))
        .collect()
}

/// `EXPLAIN (FORMAT TEXT)` for the recall statement with `query`'s parameters, one line per plan
/// row, as the server rendered it.
///
/// The plan is returned verbatim so a caller (an operator, or the gated test at 100 000 rows) can
/// assert on node types and index names without this crate inventing a plan vocabulary.
///
/// # Errors
///
/// As [`recall`], plus a store failure when the plan cannot be read.
pub(crate) async fn plan(
    client: &Client,
    query: &RecallQuery<'_>,
    dim: usize,
) -> Result<String, MemoryError> {
    let bound = Bound::of(query, dim)?;
    let statement = format!("EXPLAIN (FORMAT TEXT) {RECALL_SQL}");
    let rows = client
        .query(&statement, &bound.params())
        .await
        .map_err(|error| store(&error))?;
    let mut plan = String::new();
    for row in rows {
        let line: String = row.try_get(0).map_err(|_| corrupt("plan"))?;
        plan.push_str(&line);
        plan.push('\n');
    }
    Ok(plan)
}

/// One recalled row.
fn hit_from_row(row: &tokio_postgres::Row, tenant: &TenantId) -> Result<RecallHit, MemoryError> {
    let kind = match text(row, 0, "kind")?.as_str() {
        "episode" => MemoryKind::Episode,
        "procedure" => MemoryKind::Procedure,
        "fact" => MemoryKind::Fact,
        _ => return Err(corrupt("kind")),
    };
    let score_bp: i32 = row.try_get(10).map_err(|_| corrupt("score_bp"))?;
    let provenance = Provenance {
        tenant: tenant.clone(),
        kind,
        id: text(row, 1, "id")?,
        source: crate::codec::source_from_text("source", &text(row, 4, "source")?)?,
        created_ms: row.try_get(5).map_err(|_| corrupt("created_ms"))?,
        tier: tier_from_i16("tier", smallint(row, 3, "tier")?)?,
        audits: audits(row, 7)?,
        remedies: remedies(row, 8)?,
        epoch: match row
            .try_get::<_, Option<i64>>(9)
            .map_err(|_| corrupt("epoch"))?
        {
            Some(value) => Some(epoch_from_i64("epoch", value)?),
            None => None,
        },
    };
    Ok(RecallHit {
        provenance,
        score_bp,
        text: text(row, 2, "display")?,
        confidence: confidence_from_i16("confidence_bp", smallint(row, 6, "confidence_bp")?)?,
    })
}

fn text(row: &tokio_postgres::Row, index: usize, column: &str) -> Result<String, MemoryError> {
    row.try_get(index).map_err(|_| corrupt(column))
}

fn smallint(row: &tokio_postgres::Row, index: usize, column: &str) -> Result<i16, MemoryError> {
    row.try_get(index).map_err(|_| corrupt(column))
}

fn audits(row: &tokio_postgres::Row, index: usize) -> Result<Vec<AuditRef>, MemoryError> {
    let ids: Vec<String> = row.try_get(index).map_err(|_| corrupt("audits"))?;
    ids.iter()
        .map(|id| AuditRef::parse(id).map_err(|_| corrupt("audits")))
        .collect()
}

fn remedies(row: &tokio_postgres::Row, index: usize) -> Result<Vec<RemedyId>, MemoryError> {
    let ids: Vec<String> = row.try_get(index).map_err(|_| corrupt("remedies"))?;
    ids.iter()
        .map(|id| remedy_from_text("remedies", id))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use spot_agent_memory::EMBEDDING_DIM;

    fn filters() -> Filters {
        Filters::any()
    }

    fn query<'a>(tenant: &'a TenantId, vector: &'a [f32], filters: &'a Filters) -> RecallQuery<'a> {
        RecallQuery {
            tenant,
            agent_tier: Tier::T2,
            vector,
            filters,
            k: 5,
            now_ms: 10_000,
            recency: RecencyBoost::DEFAULT,
        }
    }

    #[test]
    fn the_query_vector_is_checked_before_the_database_sees_it() {
        assert!(vector_param(EMBEDDING_DIM, &vec![0.5; EMBEDDING_DIM]).is_ok());
        assert!(vector_param(EMBEDDING_DIM, &[0.5, 1.0]).is_err());
        assert!(vector_param(EMBEDDING_DIM, &vec![f32::NAN; EMBEDDING_DIM]).is_err());
    }

    #[test]
    fn the_kinds_parameter_carries_the_requested_kinds_in_canonical_order() {
        assert_eq!(kinds_param(&filters()), ["episode", "procedure", "fact"]);
        assert_eq!(
            kinds_param(&Filters::kinds(&[MemoryKind::Fact, MemoryKind::Episode])),
            ["episode", "fact"]
        );
        assert_eq!(
            kinds_param(&Filters::kinds(&[MemoryKind::Procedure])),
            ["procedure"]
        );
    }

    #[test]
    fn a_recall_asks_for_at_least_one_hit() {
        let tenant = TenantId::parse("tenant_a").unwrap();
        let vector = vec![0.25; EMBEDDING_DIM];
        let filters = filters();
        let mut request = query(&tenant, &vector, &filters);
        request.k = 0;
        assert!(Bound::of(&request, EMBEDDING_DIM).is_err());
        request.k = 5;
        let bound = Bound::of(&request, EMBEDDING_DIM).unwrap();
        assert_eq!(bound.limit, 5);
        assert_eq!(bound.kinds.len(), 3);
        assert!(bound.vector.starts_with('[') && bound.vector.ends_with(']'));
    }

    #[test]
    fn k_is_clamped_to_the_recall_ceiling() {
        let tenant = TenantId::parse("tenant_a").unwrap();
        let vector = vec![0.25; EMBEDDING_DIM];
        let filters = filters();
        let mut request = query(&tenant, &vector, &filters);
        request.k = usize::MAX;
        let bound = Bound::of(&request, EMBEDDING_DIM).unwrap();
        assert_eq!(bound.limit, i64::try_from(MAX_RECALL_K).unwrap());
        request.k = MAX_RECALL_K;
        assert_eq!(Bound::of(&request, EMBEDDING_DIM).unwrap().limit, 100);
        request.k = 7;
        assert_eq!(Bound::of(&request, EMBEDDING_DIM).unwrap().limit, 7);
    }

    #[test]
    fn extreme_times_and_boosts_are_clamped_before_the_database_sees_them() {
        let tenant = TenantId::parse("tenant_a").unwrap();
        let vector = vec![0.25; EMBEDDING_DIM];
        let filters = filters();
        let mut request = query(&tenant, &vector, &filters);
        request.now_ms = i64::MAX;
        request.recency = RecencyBoost {
            cap_bp: i32::MAX,
            window_ms: i64::MAX,
        };
        let bound = Bound::of(&request, EMBEDDING_DIM).unwrap();
        assert_eq!(bound.now_ms, crate::row::MAX_TIME_MS);
        assert_eq!(bound.cap_bp, MAX_BOOST_BP);
        assert_eq!(bound.window_ms, MAX_WINDOW_MS);
        // The largest product the boost forms stays inside bigint.
        assert!(i128::from(bound.window_ms) * i128::from(bound.cap_bp) < i128::from(i64::MAX));
        request.now_ms = i64::MIN;
        assert_eq!(Bound::of(&request, EMBEDDING_DIM).unwrap().now_ms, 0);
        assert!(RecencyBoost::new(MAX_BOOST_BP + 1, 1).is_err());
        assert!(RecencyBoost::new(1, MAX_WINDOW_MS + 1).is_err());
    }

    #[test]
    fn the_ttl_filter_has_no_sum_that_can_overflow() {
        assert!(!RECALL_SQL.contains("created_ms + "));
        assert_eq!(RECALL_SQL.matches("ttl_ms > $4::bigint -").count(), 3);
    }

    #[test]
    fn a_recency_boost_refuses_a_negative_cap_or_an_empty_window() {
        assert_eq!(RecencyBoost::new(0, 1).unwrap().cap_bp, 0);
        assert!(RecencyBoost::new(-1, 1_000).is_err());
        assert!(RecencyBoost::new(100, 0).is_err());
        assert_eq!(RecencyBoost::NONE.cap_bp, 0);
        assert_eq!(RecencyBoost::DEFAULT.window_ms, 2_592_000_000);
    }
}
