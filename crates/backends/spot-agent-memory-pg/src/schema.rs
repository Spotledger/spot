//! The DDL of the memory tables and the recall index this store chooses.
//!
//! The tables live in a **memory database of their own**, never in the tenant's business
//! database, and this module is the only place their shape is written down.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_memory_episode` | what happened: the incident it is about (DocType and id only), its symptoms, its actions as `jsonb`, the outcome, the sanitiser's summary, the audit ids, the remedies it ran, its metadata and its embedding |
//! | `spot_memory_procedure` | how to fix it: the title, symptoms, ordered steps and confirmations as `jsonb`, the remedies it runs, its metadata and its embedding |
//! | `spot_memory_fact` | what the installation is: the fact key, its value as `jsonb`, the deployment epoch, the rendered text, its metadata and its embedding |
//! | `spot_memory_candidate` | the candidate-remedy queue: the spec fields, the steps, the self-test scenario skeleton and the CI status |
//!
//! Every table keys on `(tenant, id)`. The database is already per tenant — that is the whole
//! isolation story — and the tenant column is kept anyway so a row still says whose memory it
//! is, and so a store handed the wrong [`TenantId`](spot_agent_memory::TenantId) finds nothing
//! rather than somebody else's episode.
//!
//! # The vector column
//!
//! `embedding vector([`EMBEDDING_DIM`])` — the width is the domain model's, not a parameter: a
//! pgvector column is fixed-width and `spot-agent-memory` refuses, records and scores only
//! [`EMBEDDING_DIM`]-wide vectors, so a configurable width would be a width nothing could write
//! at. `CREATE EXTENSION vector` runs against the memory database and nowhere else.
//!
//! # The index is chosen by size
//!
//! An approximate nearest-neighbour index is built at [`prepare`] time from the row count
//! ([`recall_index_for`]): **HNSW** (m = 16, ef_construction = 64) while the table is below
//! [`IVFFLAT_FROM_ROWS`], and **IVFFlat** (`lists` scaled from the row count) from there up,
//! because an IVFFlat build on a large table is far cheaper and gives up little at that size —
//! while an IVFFlat build on an empty table gives up everything. Re-running [`prepare`] after
//! the data grows switches the index: the other method's index is dropped first, so the table
//! never carries two vector indexes the planner has to choose between.
//!
//! `prepare` is idempotent and serialised across nodes by a transaction-scoped advisory lock.

use deadpool_postgres::Pool;
use spot_agent_memory::EMBEDDING_DIM;
use spot_app_api::AppResult;

use crate::error::{extension_failed, extension_missing, migration, migration_pool};

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4D45_4D00_0001;

/// The width of every `vector(n)` column: the domain model's, which is what every stored entry
/// is checked against before it is written.
pub const VECTOR_DIM: usize = EMBEDDING_DIM;

/// From this many rows on, [`prepare`] builds an IVFFlat index instead of HNSW.
pub const IVFFLAT_FROM_ROWS: i64 = 100_000;

/// The name of the check that keeps a candidate from being promoted without a CI run.
pub const PROMOTED_NEEDS_CI_CONSTRAINT: &str = "spot_memory_candidate_promoted_needs_ci";

/// Which approximate nearest-neighbour index a table carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallIndex {
    /// Hierarchical navigable small world: the default below [`IVFFLAT_FROM_ROWS`] rows.
    Hnsw,
    /// Inverted file with flat lists: the default from [`IVFFLAT_FROM_ROWS`] rows up.
    Ivfflat,
}

impl RecallIndex {
    /// The method's name as pgvector spells it in an index definition.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hnsw => "hnsw",
            Self::Ivfflat => "ivfflat",
        }
    }
}

/// The index a table with `rows` rows gets.
#[must_use]
pub fn recall_index_for(rows: i64) -> RecallIndex {
    if rows < IVFFLAT_FROM_ROWS {
        RecallIndex::Hnsw
    } else {
        RecallIndex::Ivfflat
    }
}

/// The `lists` an IVFFlat index over `rows` rows is built with: one list per thousand rows,
/// clamped to the range pgvector recommends.
#[must_use]
pub fn ivfflat_lists(rows: i64) -> i64 {
    (rows / 1_000).clamp(1, 4_096)
}

/// The three entry tables, in recall order.
pub(crate) const ENTRY_TABLES: [&str; 3] = [
    "spot_memory_episode",
    "spot_memory_procedure",
    "spot_memory_fact",
];

/// The tables, their constraints and the indexes that are not the vector index.
///
/// Every statement is `IF NOT EXISTS` or a guarded drop-and-add, so `prepare` can run on an
/// existing memory database and from several nodes at once.
fn tables_ddl() -> String {
    let dim = VECTOR_DIM;
    format!(
        r#"
CREATE TABLE IF NOT EXISTS spot_memory_episode (
    tenant           text NOT NULL,
    id               text NOT NULL,
    incident_doctype text NOT NULL,
    incident_id      text NOT NULL,
    symptoms         text[] NOT NULL,
    actions          jsonb NOT NULL,
    outcome          text NOT NULL,
    summary          text NOT NULL,
    audits           text[] NOT NULL,
    remedies         text[] NOT NULL,
    tier             smallint NOT NULL,
    created_ms       bigint NOT NULL,
    ttl_ms           bigint,
    confidence_bp    smallint NOT NULL,
    source           text NOT NULL,
    embedding        vector({dim}) NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_memory_episode_tier CHECK (tier BETWEEN 0 AND 3),
    CONSTRAINT spot_memory_episode_created CHECK (created_ms BETWEEN 0 AND 253402300799999),
    CONSTRAINT spot_memory_episode_ttl CHECK (ttl_ms IS NULL OR ttl_ms BETWEEN 0 AND 3153600000000),
    CONSTRAINT spot_memory_episode_confidence CHECK (confidence_bp BETWEEN 0 AND 10000),
    CONSTRAINT spot_memory_episode_symptoms CHECK (cardinality(symptoms) >= 1),
    CONSTRAINT spot_memory_episode_audits CHECK (cardinality(audits) >= 1)
);

CREATE TABLE IF NOT EXISTS spot_memory_procedure (
    tenant        text NOT NULL,
    id            text NOT NULL,
    title         text NOT NULL,
    symptoms      text[] NOT NULL,
    steps         jsonb NOT NULL,
    confirmations jsonb NOT NULL,
    remedies      text[] NOT NULL,
    tier          smallint NOT NULL,
    created_ms    bigint NOT NULL,
    ttl_ms        bigint,
    confidence_bp smallint NOT NULL,
    source        text NOT NULL,
    embedding     vector({dim}) NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_memory_procedure_tier CHECK (tier BETWEEN 0 AND 3),
    CONSTRAINT spot_memory_procedure_created CHECK (created_ms BETWEEN 0 AND 253402300799999),
    CONSTRAINT spot_memory_procedure_ttl CHECK (ttl_ms IS NULL OR ttl_ms BETWEEN 0 AND 3153600000000),
    CONSTRAINT spot_memory_procedure_confidence CHECK (confidence_bp BETWEEN 0 AND 10000),
    CONSTRAINT spot_memory_procedure_symptoms CHECK (cardinality(symptoms) >= 1)
);

CREATE TABLE IF NOT EXISTS spot_memory_fact (
    tenant        text NOT NULL,
    id            text NOT NULL,
    fact_key      text NOT NULL,
    value         jsonb NOT NULL,
    epoch         bigint NOT NULL,
    display_text  text NOT NULL,
    tier          smallint NOT NULL,
    created_ms    bigint NOT NULL,
    ttl_ms        bigint,
    confidence_bp smallint NOT NULL,
    source        text NOT NULL,
    embedding     vector({dim}) NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_memory_fact_tier CHECK (tier BETWEEN 0 AND 3),
    CONSTRAINT spot_memory_fact_created CHECK (created_ms BETWEEN 0 AND 253402300799999),
    CONSTRAINT spot_memory_fact_ttl CHECK (ttl_ms IS NULL OR ttl_ms BETWEEN 0 AND 3153600000000),
    CONSTRAINT spot_memory_fact_confidence CHECK (confidence_bp BETWEEN 0 AND 10000)
);

CREATE INDEX IF NOT EXISTS spot_memory_episode_symptoms_idx
    ON spot_memory_episode USING gin (symptoms);
CREATE INDEX IF NOT EXISTS spot_memory_episode_remedies_idx
    ON spot_memory_episode USING gin (remedies);
CREATE INDEX IF NOT EXISTS spot_memory_episode_created_idx
    ON spot_memory_episode (tenant, created_ms);

CREATE INDEX IF NOT EXISTS spot_memory_procedure_symptoms_idx
    ON spot_memory_procedure USING gin (symptoms);
CREATE INDEX IF NOT EXISTS spot_memory_procedure_remedies_idx
    ON spot_memory_procedure USING gin (remedies);

CREATE INDEX IF NOT EXISTS spot_memory_fact_key_idx
    ON spot_memory_fact (tenant, fact_key);

CREATE TABLE IF NOT EXISTS spot_memory_candidate (
    tenant        text NOT NULL,
    id            text NOT NULL,
    remedy        text NOT NULL,
    procedure_id  text NOT NULL,
    status        text NOT NULL,
    ci_run        text,
    rejection     text,
    confirmations integer NOT NULL,
    symptoms      text[] NOT NULL,
    steps         jsonb NOT NULL,
    title         text NOT NULL,
    tier          smallint NOT NULL,
    risk          text NOT NULL,
    affects       text[] NOT NULL,
    preconditions text NOT NULL,
    undo_kind     text NOT NULL,
    undo_detail   text NOT NULL,
    params        jsonb NOT NULL,
    scenario      jsonb NOT NULL,
    updated_ms    bigint NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT spot_memory_candidate_status
        CHECK (status IN ('proposed', 'in_ci', 'passed', 'rejected', 'promoted')),
    CONSTRAINT spot_memory_candidate_promoted_needs_ci
        CHECK (status <> 'promoted' OR ci_run IS NOT NULL),
    CONSTRAINT spot_memory_candidate_ci_run
        CHECK (ci_run IS NULL OR status <> 'proposed'),
    CONSTRAINT spot_memory_candidate_tier CHECK (tier BETWEEN 0 AND 3),
    CONSTRAINT spot_memory_candidate_updated CHECK (updated_ms >= 0)
);
"#
    )
}

/// The statement that gives `table` its recall index for `index`, dropping the other method's
/// index first so the table never carries two.
fn recall_index_ddl(table: &str, index: RecallIndex, rows: i64) -> String {
    let (hnsw_name, ivfflat_name) = (
        format!("{table}_embedding_hnsw"),
        format!("{table}_embedding_ivfflat"),
    );
    let create = match index {
        RecallIndex::Hnsw => format!(
            "CREATE INDEX IF NOT EXISTS {hnsw_name} ON {table} \
             USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)"
        ),
        RecallIndex::Ivfflat => format!(
            "CREATE INDEX IF NOT EXISTS {ivfflat_name} ON {table} \
             USING ivfflat (embedding vector_cosine_ops) WITH (lists = {})",
            ivfflat_lists(rows)
        ),
    };
    let (drop, keep) = match index {
        RecallIndex::Hnsw => (ivfflat_name, hnsw_name),
        RecallIndex::Ivfflat => (hnsw_name, ivfflat_name),
    };
    format!(
        "DROP INDEX IF EXISTS {drop};\n{create};\nCOMMENT ON INDEX {keep} IS 'spot recall index'"
    )
}

/// The recall index a table carries, from `pg_indexes`, or `None` when it has no vector index.
///
/// # Errors
/// `agent_memory_pg.migration_failed` when the catalogue cannot be read.
pub async fn recall_index_of(
    client: &tokio_postgres::Client,
    table: &str,
) -> AppResult<Option<RecallIndex>> {
    let rows = client
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = $1 AND indexdef LIKE '%embedding%'",
            &[&table],
        )
        .await
        .map_err(migration)?;
    for row in rows {
        let name: String = row.try_get(0).map_err(migration)?;
        if name.ends_with("_embedding_hnsw") {
            return Ok(Some(RecallIndex::Hnsw));
        }
        if name.ends_with("_embedding_ivfflat") {
            return Ok(Some(RecallIndex::Ivfflat));
        }
    }
    Ok(None)
}

/// Creates the `vector` extension in the memory database this pool points at.
///
/// `CREATE EXTENSION` needs a role the server trusts to install an extension, so provisioning
/// (which runs as the control-plane role) is where it happens; [`prepare`] calls this too and
/// treats a server without the extension as the skippable state
/// `agent_memory_pg.extension_missing`.
///
/// # Errors
/// `agent_memory_pg.extension_missing` when the server has no `vector` extension, and
/// `agent_memory_pg.migration_failed` when it exists but cannot be created here.
pub async fn ensure_extension<C>(client: &C) -> AppResult<()>
where
    C: deadpool_postgres::GenericClient,
{
    let present: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')",
            &[],
        )
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    if present {
        return Ok(());
    }
    let available: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector')",
            &[],
        )
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    if !available {
        return Err(extension_missing());
    }
    client
        .batch_execute("CREATE EXTENSION vector")
        .await
        .map_err(extension_failed)
}

/// Creates the extension, the tables and the recall index if they are missing.
///
/// Idempotent, and safe to run from several nodes at once: the whole call is one transaction
/// behind a transaction-scoped advisory lock.
///
/// # Errors
/// `agent_memory_pg.extension_missing` when the server has no `vector` extension, and
/// `agent_memory_pg.migration_failed` when the memory database cannot apply the DDL.
pub async fn prepare(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(migration)?;
    ensure_extension(&tx).await?;
    tx.batch_execute(&tables_ddl()).await.map_err(migration)?;
    for table in ENTRY_TABLES {
        let rows: i64 = tx
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .map_err(migration)?
            .try_get(0)
            .map_err(migration)?;
        tx.batch_execute(&recall_index_ddl(table, recall_index_for(rows), rows))
            .await
            .map_err(migration)?;
    }
    tx.commit().await.map_err(migration)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_hnsw_below_the_threshold_and_ivfflat_from_it() {
        assert_eq!(recall_index_for(0), RecallIndex::Hnsw);
        assert_eq!(recall_index_for(99_999), RecallIndex::Hnsw);
        assert_eq!(recall_index_for(IVFFLAT_FROM_ROWS), RecallIndex::Ivfflat);
        assert_eq!(RecallIndex::Hnsw.as_str(), "hnsw");
        assert_eq!(RecallIndex::Ivfflat.as_str(), "ivfflat");
    }

    #[test]
    fn the_list_count_scales_with_the_table_and_stays_in_range() {
        assert_eq!(ivfflat_lists(0), 1);
        assert_eq!(ivfflat_lists(100_000), 100);
        assert_eq!(ivfflat_lists(10_000_000), 4_096);
    }

    #[test]
    fn every_table_carries_the_domains_vector_width() {
        let ddl = tables_ddl();
        assert_eq!(ddl.matches(&format!("vector({VECTOR_DIM})")).count(), 3);
        for table in ENTRY_TABLES {
            assert!(ddl.contains(table), "{table} is created");
        }
        assert_eq!(VECTOR_DIM, EMBEDDING_DIM);
    }

    #[test]
    fn the_time_checks_match_the_constants_the_queries_clamp_to() {
        let ddl = tables_ddl();
        let created = format!("BETWEEN 0 AND {}", crate::row::MAX_TIME_MS);
        let ttl = format!("BETWEEN 0 AND {}", crate::row::MAX_TTL_MS);
        assert_eq!(ddl.matches(&created).count(), 3);
        assert_eq!(ddl.matches(&ttl).count(), 3);
    }

    #[test]
    fn a_candidate_cannot_be_promoted_without_a_ci_run() {
        let ddl = tables_ddl();
        assert!(ddl.contains(PROMOTED_NEEDS_CI_CONSTRAINT));
        assert!(ddl.contains("CHECK (status <> 'promoted' OR ci_run IS NOT NULL)"));
    }

    #[test]
    fn the_index_statement_drops_the_other_method() {
        let hnsw = recall_index_ddl("spot_memory_fact", RecallIndex::Hnsw, 0);
        assert!(hnsw.contains("USING hnsw"));
        assert!(hnsw.contains("DROP INDEX IF EXISTS spot_memory_fact_embedding_ivfflat"));
        let ivfflat = recall_index_ddl("spot_memory_fact", RecallIndex::Ivfflat, 250_000);
        assert!(ivfflat.contains("USING ivfflat"));
        assert!(ivfflat.contains("lists = 250"));
        assert!(ivfflat.contains("DROP INDEX IF EXISTS spot_memory_fact_embedding_hnsw"));
    }
}
