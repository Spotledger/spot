//! The DDL of the retrieval table and its vector index, and the settings that shape them.
//!
//! One table holds every tenant's chunks, and **the tenant is part of every key**:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the site the chunk belongs to (part of the primary key) |
//! | `doctype`, `id` | the document: DocType name and the 16-byte record id |
//! | `chunk_no` | the window's number within the document's version |
//! | `version` | the committed version the text came from |
//! | `fields` | the names of the fields the text came from (the retriever's field check) |
//! | `body` | the window's text, exactly as the indexer gave it |
//! | `embedding` | `vector(dim)`, the embedding of `body` |
//!
//! There is **no permission column**: no role, no owner, no share, no allowed-user list. The
//! index is a copy of data and decides no access; `spot-retrieval` re-checks every hit against
//! the live permission core before it leaves that crate. The only rights-shaped data stored is
//! the list of field names, which the retriever compares with the caller's live field view.
//!
//! # The vector column
//!
//! `embedding vector(dim)`: the width is fixed per index policy (the embedding model's output
//! width), checked on every write and every query, and checked again by [`prepare`] against an
//! existing table, so a changed model is a re-index into a new table and never a silent mix of
//! widths. pgvector indexes at most [`MAX_DIM`] dimensions.
//!
//! # The index
//!
//! [`IndexKind::Hnsw`] (the default, `m = 16`, `ef_construction = 64`, cosine) or
//! [`IndexKind::Ivfflat`] with an explicit `lists`. The measured choice is a deployment setting
//! because the right method depends on the corpus size and on whether the table is built empty
//! (HNSW is fine empty; IVFFlat is built from existing rows). Preparing with the other kind drops
//! the first method's index, so the planner never chooses between two.
//!
//! Filtering by tenant happens after the approximate scan (pgvector does not partition an index
//! by a column), so a table shared by many tenants returns fewer than `limit` rows when most of
//! the nearest rows belong to others. The search raises `hnsw.ef_search` for that, and the
//! recommended deployment is one retrieval database per site, where the filter keeps every row.

use deadpool_postgres::Pool;

use crate::error::{PrepareError, prepare, prepare_pool};

/// The table every chunk lives in.
pub const TABLE: &str = "spot_retrieval_chunk";

/// The name of the HNSW index on [`TABLE`].
pub const HNSW_INDEX: &str = "spot_retrieval_chunk_embedding_hnsw";

/// The name of the IVFFlat index on [`TABLE`].
pub const IVFFLAT_INDEX: &str = "spot_retrieval_chunk_embedding_ivfflat";

/// The widest vector pgvector will index.
pub const MAX_DIM: usize = 2_000;

/// The most `lists` an IVFFlat index may be built with.
pub const MAX_LISTS: u32 = 32_768;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5245_5452_0001;

/// Which approximate nearest-neighbour index the table carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    /// Hierarchical navigable small world: the default.
    Hnsw,
    /// Inverted file with flat lists, built with `lists` lists.
    Ivfflat {
        /// The list count; `1..=`[`MAX_LISTS`].
        lists: u32,
    },
}

impl IndexKind {
    /// The method's name as pgvector spells it in an index definition.
    #[must_use]
    pub fn method(self) -> &'static str {
        match self {
            Self::Hnsw => "hnsw",
            Self::Ivfflat { .. } => "ivfflat",
        }
    }

    /// The `ivfflat.probes` a query uses: a tenth of the lists, at least one. HNSW has none.
    #[must_use]
    pub(crate) fn probes(self) -> i64 {
        match self {
            Self::Hnsw => 1,
            Self::Ivfflat { lists } => i64::from((lists / 10).max(1)),
        }
    }

    /// Whether the settings are in range.
    #[must_use]
    pub(crate) fn valid(self) -> bool {
        match self {
            Self::Hnsw => true,
            Self::Ivfflat { lists } => (1..=MAX_LISTS).contains(&lists),
        }
    }
}

/// The table and its non-vector constraints. Every statement is `IF NOT EXISTS`, so `prepare`
/// can run on an existing database and from several nodes at once.
pub(crate) fn table_ddl(dim: usize) -> String {
    format!(
        r"
CREATE TABLE IF NOT EXISTS {TABLE} (
    tenant    text NOT NULL,
    doctype   text NOT NULL,
    id        bytea NOT NULL,
    chunk_no  bigint NOT NULL,
    version   bigint NOT NULL,
    fields    text[] NOT NULL,
    body      text NOT NULL,
    embedding vector({dim}) NOT NULL,
    PRIMARY KEY (tenant, doctype, id, chunk_no),
    CONSTRAINT {TABLE}_id_len CHECK (octet_length(id) = 16),
    CONSTRAINT {TABLE}_version CHECK (version >= 0),
    CONSTRAINT {TABLE}_chunk_no CHECK (chunk_no >= 0)
)"
    )
}

/// The statements that give the table its vector index, dropping the other method's first.
pub(crate) fn index_ddl(kind: IndexKind) -> String {
    match kind {
        IndexKind::Hnsw => format!(
            "DROP INDEX IF EXISTS {IVFFLAT_INDEX};\n\
             CREATE INDEX IF NOT EXISTS {HNSW_INDEX} ON {TABLE} \
             USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)"
        ),
        IndexKind::Ivfflat { lists } => format!(
            "DROP INDEX IF EXISTS {HNSW_INDEX};\n\
             CREATE INDEX IF NOT EXISTS {IVFFLAT_INDEX} ON {TABLE} \
             USING ivfflat (embedding vector_cosine_ops) WITH (lists = {lists})"
        ),
    }
}

/// Creates the `vector` extension in the database the transaction runs on, when it is missing.
async fn ensure_extension<C>(tx: &C) -> Result<(), PrepareError>
where
    C: deadpool_postgres::GenericClient,
{
    let present: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')",
            &[],
        )
        .await
        .map_err(|e| prepare(&e))?
        .try_get(0)
        .map_err(|e| prepare(&e))?;
    if present {
        return Ok(());
    }
    let available: bool = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector')",
            &[],
        )
        .await
        .map_err(|e| prepare(&e))?
        .try_get(0)
        .map_err(|e| prepare(&e))?;
    if !available {
        return Err(PrepareError::ExtensionMissing);
    }
    tx.batch_execute("CREATE EXTENSION vector")
        .await
        .map_err(|e| prepare(&e))
}

/// Creates the extension, the table and the index if they are missing, and checks the width of
/// an existing table.
///
/// Idempotent, and safe to run from several nodes at once: the whole call is one transaction
/// behind a transaction-scoped advisory lock.
///
/// # Errors
///
/// [`PrepareError::ExtensionMissing`] without pgvector, [`PrepareError::DimensionMismatch`]
/// when the table has another width, and [`PrepareError::Failed`] when the database refuses.
pub(crate) async fn prepare_schema(
    pool: &Pool,
    dim: usize,
    kind: IndexKind,
) -> Result<(), PrepareError> {
    let mut conn = pool.get().await.map_err(|e| prepare_pool(&e))?;
    let tx = conn.transaction().await.map_err(|e| prepare(&e))?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await
        .map_err(|e| prepare(&e))?;
    ensure_extension(&tx).await?;
    tx.batch_execute(&table_ddl(dim))
        .await
        .map_err(|e| prepare(&e))?;
    // pgvector keeps the declared width in `atttypmod`.
    let stored: i32 = tx
        .query_one(
            "SELECT atttypmod FROM pg_attribute \
             WHERE attrelid = to_regclass($1) AND attname = 'embedding'",
            &[&TABLE],
        )
        .await
        .map_err(|e| prepare(&e))?
        .try_get(0)
        .map_err(|e| prepare(&e))?;
    if usize::try_from(stored).ok() != Some(dim) {
        // Dropping `tx` rolls the transaction back.
        return Err(PrepareError::DimensionMismatch {
            stored,
            wanted: dim,
        });
    }
    tx.batch_execute(&index_ddl(kind))
        .await
        .map_err(|e| prepare(&e))?;
    tx.commit().await.map_err(|e| prepare(&e))
}

/// The vector index method the table carries, from `pg_indexes`, or `None` when it has none.
///
/// # Errors
///
/// [`PrepareError::Failed`] when the catalogue cannot be read.
pub(crate) async fn index_method(pool: &Pool) -> Result<Option<&'static str>, PrepareError> {
    let conn = pool.get().await.map_err(|e| prepare_pool(&e))?;
    let rows = conn
        .query(
            "SELECT indexname FROM pg_indexes WHERE tablename = $1 AND indexdef LIKE '%embedding%'",
            &[&TABLE],
        )
        .await
        .map_err(|e| prepare(&e))?;
    for row in rows {
        let name: String = row.try_get(0).map_err(|e| prepare(&e))?;
        if name == HNSW_INDEX {
            return Ok(Some("hnsw"));
        }
        if name == IVFFLAT_INDEX {
            return Ok(Some("ivfflat"));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_table_carries_the_requested_width_and_the_tenant_in_the_key() {
        let ddl = table_ddl(384);
        assert!(ddl.contains("vector(384)"));
        assert!(ddl.contains("PRIMARY KEY (tenant, doctype, id, chunk_no)"));
    }

    #[test]
    fn the_table_holds_no_permission_data() {
        let ddl = table_ddl(8).to_lowercase();
        for word in [
            "role",
            "owner",
            "share",
            "acl",
            "permission",
            "allowed",
            "user",
        ] {
            assert!(!ddl.contains(word), "the DDL mentions {word}");
        }
    }

    #[test]
    fn each_index_statement_drops_the_other_method() {
        let hnsw = index_ddl(IndexKind::Hnsw);
        assert!(hnsw.contains("USING hnsw"));
        assert!(hnsw.contains(&format!("DROP INDEX IF EXISTS {IVFFLAT_INDEX}")));
        let ivf = index_ddl(IndexKind::Ivfflat { lists: 250 });
        assert!(ivf.contains("USING ivfflat"));
        assert!(ivf.contains("lists = 250"));
        assert!(ivf.contains(&format!("DROP INDEX IF EXISTS {HNSW_INDEX}")));
    }

    #[test]
    fn index_settings_are_range_checked() {
        assert!(IndexKind::Hnsw.valid());
        assert!(IndexKind::Ivfflat { lists: 1 }.valid());
        assert!(IndexKind::Ivfflat { lists: MAX_LISTS }.valid());
        assert!(!IndexKind::Ivfflat { lists: 0 }.valid());
        assert!(
            !IndexKind::Ivfflat {
                lists: MAX_LISTS + 1
            }
            .valid()
        );
        assert_eq!(IndexKind::Ivfflat { lists: 5 }.probes(), 1);
        assert_eq!(IndexKind::Ivfflat { lists: 200 }.probes(), 20);
        assert_eq!(IndexKind::Hnsw.method(), "hnsw");
    }
}
