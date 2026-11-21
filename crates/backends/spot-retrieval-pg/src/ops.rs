//! The statements: replace, delete, search and read-back, each one transaction.
//!
//! Every statement is a `const` with `$n` placeholders. The only text that reaches SQL besides a
//! bound value is the table name written in [`crate::schema`]. Every statement filters on the
//! tenant, so a store bound to one tenant can neither read nor write another's rows.

use std::collections::BTreeMap;

use deadpool_postgres::Client;
use spot_retrieval::{Chunk, RetrievalError, ScoredChunk};
use spot_storage_contract::{RecordId, Version};

use crate::error::{refused, store};
use crate::schema::{IndexKind, TABLE};
use crate::wire::{big, big_u32, is_zero, parse_vector, record_id, render_vector};

/// The most rows one `search` returns, whatever the caller asks for.
pub const MAX_SEARCH_LIMIT: usize = 10_000;

/// The most documents one `delete_documents` call removes.
pub const MAX_DELETE_BATCH: usize = 10_000;

/// The most documents one `documents_page` call returns.
pub const MAX_PAGE: usize = 1_000;

/// Serialises writers of one document: a transaction-scoped advisory lock on a hash of the
/// document's key, so two writers of the same document cannot interleave their delete and insert.
const LOCK_DOCUMENT: &str = "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))";

/// The newest stored version of one document (a null when it has no chunks).
const NEWEST_VERSION: &str =
    "SELECT max(version) FROM spot_retrieval_chunk WHERE tenant = $1 AND doctype = $2 AND id = $3";

/// Removes a document's chunks at or below a version.
const DELETE_UP_TO: &str = "DELETE FROM spot_retrieval_chunk \
     WHERE tenant = $1 AND doctype = $2 AND id = $3 AND version <= $4";

/// Writes one chunk; a repeated `(tenant, doctype, id, chunk_no)` within one call replaces the
/// earlier one, as the in-memory store's map does.
const INSERT_CHUNK: &str = "INSERT INTO spot_retrieval_chunk \
     (tenant, doctype, id, chunk_no, version, fields, body, embedding) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::vector) \
     ON CONFLICT (tenant, doctype, id, chunk_no) DO UPDATE SET \
     version = EXCLUDED.version, fields = EXCLUDED.fields, body = EXCLUDED.body, \
     embedding = EXCLUDED.embedding";

/// Removes every chunk of a document.
const DELETE_DOCUMENT: &str =
    "DELETE FROM spot_retrieval_chunk WHERE tenant = $1 AND doctype = $2 AND id = $3";

/// The columns a chunk is read back with.
const COLUMNS: &str = "doctype, id, version, chunk_no, fields, body, embedding::text";

/// The nearest chunks, best first, ties broken by DocType (byte order), record id, chunk number.
const SEARCH: &str = "SELECT doctype, id, version, chunk_no, fields, body, embedding::text, \
     (1 - (embedding <=> $1::text::vector))::float4 \
     FROM spot_retrieval_chunk WHERE tenant = $2 \
     ORDER BY embedding <=> $1::text::vector, doctype COLLATE \"C\", id, chunk_no LIMIT $3";

/// The first chunks by key, for a query vector with no direction: every score is zero.
const SEARCH_UNRANKED: &str = "SELECT doctype, id, version, chunk_no, fields, body, \
     embedding::text, 0::float4 \
     FROM spot_retrieval_chunk WHERE tenant = $1 \
     ORDER BY doctype COLLATE \"C\", id, chunk_no LIMIT $2";

/// Applies the search-time settings for this transaction only.
const TUNE: &str =
    "SELECT set_config('hnsw.ef_search', $1, true), set_config('ivfflat.probes', $2, true)";

/// One chunk, validated and rendered for the wire.
struct Prepared<'a> {
    chunk: &'a Chunk,
    version: i64,
    chunk_no: i64,
    vector: String,
}

/// Replaces each document's older chunks with the incoming ones.
///
/// Per document: the newest incoming version is compared with the newest stored one. When the
/// stored one is newer nothing is written (a replayed or out-of-order change is a no-op).
/// Otherwise every stored chunk at or below the incoming version is removed and the incoming
/// chunks **of that newest version** are written; chunks of an older version in the same call are
/// dropped, exactly as the in-memory store does. The whole call is one transaction, so a reader
/// sees a document's old chunks or its new ones, never a mixture, and a failure leaves the
/// previous chunks in place.
pub(crate) async fn upsert(
    client: &mut Client,
    tenant: &str,
    dim: usize,
    chunks: &[Chunk],
) -> Result<(), RetrievalError> {
    if chunks.is_empty() {
        return Ok(());
    }
    // Validate and render everything before the first write.
    let mut prepared = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        prepared.push(Prepared {
            chunk,
            version: big(chunk.version.0)?,
            chunk_no: big_u32(chunk.chunk_no),
            vector: render_vector(&chunk.vector, dim)?,
        });
    }
    let mut newest: BTreeMap<(&str, RecordId), i64> = BTreeMap::new();
    for item in &prepared {
        newest
            .entry((item.chunk.doctype.as_str(), item.chunk.id))
            .and_modify(|v| *v = (*v).max(item.version))
            .or_insert(item.version);
    }

    let tx = client.transaction().await.map_err(|e| store(&e))?;
    let lock = tx.prepare(LOCK_DOCUMENT).await.map_err(|e| store(&e))?;
    let newest_stmt = tx.prepare(NEWEST_VERSION).await.map_err(|e| store(&e))?;
    let delete = tx.prepare(DELETE_UP_TO).await.map_err(|e| store(&e))?;
    let insert = tx.prepare(INSERT_CHUNK).await.map_err(|e| store(&e))?;
    for ((doctype, id), incoming) in &newest {
        let key = document_key(tenant, doctype, *id);
        tx.execute(&lock, &[&key]).await.map_err(|e| store(&e))?;
        let id_bytes: &[u8] = &id.as_bytes()[..];
        let stored: Option<i64> = tx
            .query_one(&newest_stmt, &[&tenant, doctype, &id_bytes])
            .await
            .map_err(|e| store(&e))?
            .try_get(0)
            .map_err(|e| store(&e))?;
        if stored.is_some_and(|stored| stored > *incoming) {
            continue;
        }
        tx.execute(&delete, &[&tenant, doctype, &id_bytes, incoming])
            .await
            .map_err(|e| store(&e))?;
        for item in prepared.iter().filter(|item| {
            item.chunk.doctype == *doctype && item.chunk.id == *id && item.version == *incoming
        }) {
            tx.execute(
                &insert,
                &[
                    &tenant,
                    &item.chunk.doctype,
                    &id_bytes,
                    &item.chunk_no,
                    &item.version,
                    &item.chunk.fields,
                    &item.chunk.text,
                    &item.vector,
                ],
            )
            .await
            .map_err(|e| store(&e))?;
        }
    }
    tx.commit().await.map_err(|e| store(&e))
}

/// The key a document's advisory lock hashes: tenant, DocType and record id, separated by a
/// control character none of them can contain.
fn document_key(tenant: &str, doctype: &str, id: RecordId) -> String {
    format!("{tenant}\u{1f}{doctype}\u{1f}{id}")
}

/// Removes every chunk of each document, in one transaction.
pub(crate) async fn delete_documents(
    client: &mut Client,
    tenant: &str,
    documents: &[(&str, RecordId)],
) -> Result<(), RetrievalError> {
    if documents.is_empty() {
        return Ok(());
    }
    if documents.len() > MAX_DELETE_BATCH {
        return Err(refused("too many documents in one delete"));
    }
    let tx = client.transaction().await.map_err(|e| store(&e))?;
    let lock = tx.prepare(LOCK_DOCUMENT).await.map_err(|e| store(&e))?;
    let delete = tx.prepare(DELETE_DOCUMENT).await.map_err(|e| store(&e))?;
    for (doctype, id) in documents {
        let key = document_key(tenant, doctype, *id);
        tx.execute(&lock, &[&key]).await.map_err(|e| store(&e))?;
        let id_bytes: &[u8] = &id.as_bytes()[..];
        tx.execute(&delete, &[&tenant, doctype, &id_bytes])
            .await
            .map_err(|e| store(&e))?;
    }
    tx.commit().await.map_err(|e| store(&e))
}

/// The `limit` nearest chunks of `tenant` to `vector`, best first.
///
/// The query runs in a read transaction that raises `hnsw.ef_search` (to `limit`, within
/// pgvector's `40..=1000`) and sets `ivfflat.probes` for this transaction only, so a filtered
/// scan still reaches enough candidates. A zero query vector has no direction (pgvector's cosine
/// distance is undefined), so it ranks by key with every score zero, as the in-memory store does.
pub(crate) async fn search(
    client: &mut Client,
    tenant: &str,
    dim: usize,
    index: IndexKind,
    vector: &[f32],
    limit: usize,
) -> Result<Vec<ScoredChunk>, RetrievalError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let limit = limit.min(MAX_SEARCH_LIMIT);
    let literal = render_vector(vector, dim)?;
    let limit_param = i64::try_from(limit).unwrap_or(i64::MAX);
    let tx = client.transaction().await.map_err(|e| store(&e))?;
    let ef_search = limit.clamp(40, 1_000).to_string();
    let probes = index.probes().to_string();
    tx.execute(TUNE, &[&ef_search, &probes])
        .await
        .map_err(|e| store(&e))?;
    let fetched = if is_zero(vector) {
        tx.query(SEARCH_UNRANKED, &[&tenant, &limit_param]).await
    } else {
        tx.query(SEARCH, &[&literal, &tenant, &limit_param]).await
    };
    let rows = fetched.map_err(|e| store(&e))?;
    tx.commit().await.map_err(|e| store(&e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let score: f32 = row.try_get(7).map_err(|e| store(&e))?;
        out.push(ScoredChunk {
            chunk: decode(row)?,
            // A stored zero vector has an undefined distance; its score is zero, as in memory.
            score: if score.is_nan() { 0.0 } else { score },
        });
    }
    Ok(out)
}

/// Decodes the first seven columns of a row into a chunk, through the same constructors a
/// caller would use: the record id is re-validated, the version and number are range-checked.
fn decode(row: &tokio_postgres::Row) -> Result<Chunk, RetrievalError> {
    let id_bytes: Vec<u8> = row.try_get(1).map_err(|e| store(&e))?;
    let version: i64 = row.try_get(2).map_err(|e| store(&e))?;
    let chunk_no: i64 = row.try_get(3).map_err(|e| store(&e))?;
    let vector_text: String = row.try_get(6).map_err(|e| store(&e))?;
    Ok(Chunk {
        doctype: row.try_get(0).map_err(|e| store(&e))?,
        id: record_id(&id_bytes)?,
        version: Version(
            u64::try_from(version).map_err(|_| refused("a stored version is out of range"))?,
        ),
        chunk_no: u32::try_from(chunk_no)
            .map_err(|_| refused("a stored chunk number is out of range"))?,
        fields: row.try_get(4).map_err(|e| store(&e))?,
        text: row.try_get(5).map_err(|e| store(&e))?,
        vector: parse_vector(&vector_text)?,
    })
}

/// Every chunk of one document of this tenant, in chunk order.
pub(crate) async fn chunks_of(
    client: &Client,
    tenant: &str,
    doctype: &str,
    id: RecordId,
) -> Result<Vec<Chunk>, RetrievalError> {
    let statement = format!(
        "SELECT {COLUMNS} FROM {TABLE} WHERE tenant = $1 AND doctype = $2 AND id = $3 \
         ORDER BY chunk_no"
    );
    let id_bytes: &[u8] = &id.as_bytes()[..];
    let rows = client
        .query(statement.as_str(), &[&tenant, &doctype, &id_bytes])
        .await
        .map_err(|e| store(&e))?;
    rows.iter().map(decode).collect()
}

/// How many chunks this tenant holds.
pub(crate) async fn count(client: &Client, tenant: &str) -> Result<u64, RetrievalError> {
    let statement = format!("SELECT count(*) FROM {TABLE} WHERE tenant = $1");
    let total: i64 = client
        .query_one(statement.as_str(), &[&tenant])
        .await
        .map_err(|e| store(&e))?
        .try_get(0)
        .map_err(|e| store(&e))?;
    u64::try_from(total).map_err(|_| refused("a row count was negative"))
}

/// One page of the documents this tenant has chunks for, ordered by DocType (byte order) then
/// record id, after `after` (`None` for the first page).
pub(crate) async fn documents_page(
    client: &Client,
    tenant: &str,
    after: Option<(&str, RecordId)>,
    limit: usize,
) -> Result<Vec<(String, RecordId)>, RetrievalError> {
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    // The empty DocType and the empty id sort before every real document.
    let (after_doctype, after_id): (&str, Vec<u8>) = match after {
        Some((doctype, id)) => (doctype, id.as_bytes().to_vec()),
        None => ("", Vec::new()),
    };
    let statement = format!(
        "SELECT doctype, id FROM {TABLE} \
         WHERE tenant = $1 AND (doctype COLLATE \"C\", id) > ($2::text COLLATE \"C\", $3::bytea) \
         GROUP BY doctype, id ORDER BY doctype COLLATE \"C\", id LIMIT $4"
    );
    let rows = client
        .query(
            statement.as_str(),
            &[&tenant, &after_doctype, &after_id, &limit],
        )
        .await
        .map_err(|e| store(&e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let doctype: String = row.try_get(0).map_err(|e| store(&e))?;
        let id_bytes: Vec<u8> = row.try_get(1).map_err(|e| store(&e))?;
        out.push((doctype, record_id(&id_bytes)?));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_filters_on_the_tenant_and_binds_its_values() {
        for statement in [
            NEWEST_VERSION,
            DELETE_UP_TO,
            DELETE_DOCUMENT,
            SEARCH,
            SEARCH_UNRANKED,
        ] {
            assert!(statement.contains("tenant = $"), "{statement}");
            assert!(statement.contains('$'), "{statement}");
        }
        assert!(INSERT_CHUNK.contains("$1, $2, $3, $4, $5, $6, $7, $8::text::vector"));
    }

    #[test]
    fn the_search_orders_ties_by_byte_order_like_the_memory_store() {
        assert!(SEARCH.contains("doctype COLLATE \"C\", id, chunk_no"));
        assert!(SEARCH_UNRANKED.contains("doctype COLLATE \"C\", id, chunk_no"));
    }

    #[test]
    fn a_document_key_names_tenant_doctype_and_id_with_separators() {
        let id = RecordId::new_v7();
        let key = document_key("acme", "task", id);
        assert_eq!(key.matches('\u{1f}').count(), 2);
        assert!(key.starts_with("acme\u{1f}task\u{1f}"));
    }
}
