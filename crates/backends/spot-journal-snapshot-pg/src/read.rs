//! The consistent image reader.
//!
//! [`ImageReader::open`] begins **one** `REPEATABLE READ` **read-only** transaction on the
//! site's backend, reads the outbox head inside it and answers every later page from the same
//! snapshot: a write that commits while the snapshot is being taken is invisible to it. The
//! transaction takes no table lock (only `ACCESS SHARE`), has a statement timeout and a lock
//! timeout, and is read only by its own mode.
//!
//! Records come out sorted by `(entity, id)`, the order `SnapshotWriter` requires, in bounded
//! pages: one keyset page of documents (`WHERE id > $1 ORDER BY id LIMIT $2`) and, per declared
//! child table, one query for the child rows of exactly those documents. Memory is bounded by the
//! page size, never by the table.
//!
//! # The image
//!
//! The same ADR-055 value encoding the capture path writes in `doc_state` (and the audit's
//! `PgLiveSide` digests): `id`, `version`, every declared field (`null` when unset), every child
//! table as an array of rows with their `id`, 1-based `idx` and fields. The select list writes
//! each column in its ADR-055 text form in SQL; [`scalar`] types it. The audit crate's encoder
//! is crate-private, so this is a copy of its rules, pinned by this crate's tests: see the task
//! report.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value as Json};
use spot_journal_snapshot::ImageRecord;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, EntityDef, EntityKind, FieldDef, FieldType, Isolation, Name, StorageBackend,
    Transaction, TxOptions,
};
use tokio_postgres::Row;

use crate::error::SnapshotPgError;

/// Records per page when the caller does not choose.
pub const DEFAULT_PAGE_SIZE: i64 = 500;

/// The outbox head the snapshot is as of: the position the journal must reach before the snapshot
/// may be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// The outbox generation.
    pub generation: u64,
    /// The last published sequence number.
    pub seq: u64,
}

const HEAD_SQL: &str = "SELECT (SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'), \
                        last_seq FROM spot_feed_state WHERE log = 'outbox'";

fn quote(name: &str) -> String {
    // Names are validated storage names (lower snake case): nothing to escape.
    format!("\"{name}\"")
}

fn field_expr(field: &FieldDef) -> String {
    let column = quote(field.name.as_str());
    match &field.ty {
        // RFC 3339 in UTC ending in `Z`, microseconds only when non-zero.
        FieldType::DateTime => format!(
            "CASE WHEN {column} IS NULL THEN NULL ELSE concat(\
             to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS'), \
             rtrim(rtrim(to_char({column} AT TIME ZONE 'UTC', '.US'), '0'), '.'), 'Z') END"
        ),
        // PostgreSQL's base64 wraps at 76 characters; ADR-055's wire form does not.
        FieldType::Bytes => format!("translate(encode({column}, 'base64'), chr(10), '')"),
        // `::text` of a geometry is hex EWKB, not a point.
        FieldType::Geo { .. } => format!("public.ST_AsText({column})"),
        _ => format!("{column}::text"),
    }
}

fn document_page_sql(entity: &EntityDef) -> String {
    let mut parts = vec!["\"id\"::text".to_owned(), "\"version\"::text".to_owned()];
    parts.extend(entity.fields.iter().map(field_expr));
    format!(
        "SELECT {} FROM {} WHERE \"id\" > $1::text::uuid ORDER BY \"id\" LIMIT $2",
        parts.join(", "),
        quote(entity.name.as_str())
    )
}

fn child_page_sql(child: &EntityDef) -> String {
    let mut parts = vec![
        "\"parent\"::text".to_owned(),
        "\"id\"::text".to_owned(),
        "\"idx\"::text".to_owned(),
    ];
    parts.extend(child.fields.iter().map(field_expr));
    format!(
        "SELECT {} FROM {} WHERE \"parent\" = ANY($1::text[]::uuid[]) ORDER BY \"parent\", \"idx\"",
        parts.join(", "),
        quote(child.name.as_str())
    )
}

/// One stored column as the image JSON of its declared type (`text` is the select list's text).
pub fn scalar(ty: &FieldType, text: Option<&str>) -> Result<Json, SnapshotPgError> {
    let Some(text) = text else {
        return Ok(Json::Null);
    };
    Ok(match ty {
        FieldType::Bool => match text {
            "t" | "true" => Json::Bool(true),
            "f" | "false" => Json::Bool(false),
            _ => {
                return Err(SnapshotPgError::ImageInvalid(
                    "a boolean column is neither true nor false",
                ));
            }
        },
        FieldType::Int => {
            Json::from(text.parse::<i64>().map_err(|_| {
                SnapshotPgError::ImageInvalid("an integer column is not an integer")
            })?)
        }
        FieldType::Json => serde_json::from_str(text)
            .map_err(|_| SnapshotPgError::ImageInvalid("a json column is not JSON"))?,
        FieldType::Decimal { .. }
        | FieldType::Text { .. }
        | FieldType::Date
        | FieldType::Time
        | FieldType::DateTime
        | FieldType::Link { .. }
        | FieldType::Bytes
        | FieldType::Geo { .. }
        | FieldType::Range { .. } => Json::String(text.to_owned()),
    })
}

fn text_at(row: &Row, at: usize) -> Result<Option<String>, SnapshotPgError> {
    row.try_get::<usize, Option<String>>(at)
        .map_err(|_| SnapshotPgError::ReadFailed("a stored value is not text"))
}

fn required(row: &Row, at: usize) -> Result<String, SnapshotPgError> {
    text_at(row, at)?.ok_or(SnapshotPgError::ReadFailed("a key column is null"))
}

fn read_failed<E>(_: E) -> SnapshotPgError {
    SnapshotPgError::ReadFailed("a statement failed")
}

/// The reader of one snapshot: a held transaction and a cursor over the document entities.
pub struct ImageReader {
    tx: PgTransaction,
    catalog: Arc<Catalog>,
    entities: Vec<EntityDef>,
    at: usize,
    after: Option<String>,
    page_size: i64,
    head: Head,
    pages: usize,
    records: u64,
}

impl ImageReader {
    /// Opens the snapshot: the transaction, its timeouts and the outbox head read inside it.
    ///
    /// `excluded` are the entities outside the recoverable record (the capture exclusions); child
    /// entities are never read on their own.
    ///
    /// # Errors
    /// [`SnapshotPgError::OptionsInvalid`] for a catalogue with no document entity left or a zero
    /// page size, [`SnapshotPgError::ReadFailed`] when the transaction cannot start or the outbox
    /// is not installed.
    pub async fn open(
        backend: &PgBackend,
        catalog: &Arc<Catalog>,
        excluded: &BTreeSet<String>,
        page_size: i64,
        statement_timeout: Duration,
    ) -> Result<Self, SnapshotPgError> {
        if page_size < 1 {
            return Err(SnapshotPgError::OptionsInvalid("the page size is zero"));
        }
        let entities: Vec<EntityDef> = catalog
            .entities()
            .filter(|e| {
                matches!(e.kind, EntityKind::Document) && !excluded.contains(e.name.as_str())
            })
            .cloned()
            .collect();
        if entities.is_empty() {
            return Err(SnapshotPgError::OptionsInvalid(
                "the catalogue declares no document entity to snapshot",
            ));
        }
        let label = Name::new("journal_snapshot_read")
            .map_err(|_| SnapshotPgError::OptionsInvalid("the transaction label is unusable"))?;
        let mut options = TxOptions::new(label, Isolation::RepeatableRead).read_only();
        options.statement_timeout = statement_timeout;
        options.lock_timeout = Duration::from_secs(5);
        let mut tx = backend
            .begin(Arc::clone(catalog), options)
            .await
            .map_err(read_failed)?;
        // The first read fixes the snapshot; the head is that first read.
        let rows = tx
            .raw_query(HEAD_SQL, &[], None)
            .await
            .map_err(|_| SnapshotPgError::ReadFailed("the outbox is not installed"))?;
        let row = rows
            .first()
            .ok_or(SnapshotPgError::ReadFailed("the outbox has no state row"))?;
        let generation: i64 = row.try_get(0).map_err(read_failed)?;
        let seq: i64 = row.try_get(1).map_err(read_failed)?;
        let head = Head {
            generation: u64::try_from(generation)
                .map_err(|_| SnapshotPgError::ReadFailed("the outbox holds a negative position"))?,
            seq: u64::try_from(seq)
                .map_err(|_| SnapshotPgError::ReadFailed("the outbox holds a negative position"))?,
        };
        Ok(Self {
            tx,
            catalog: Arc::clone(catalog),
            entities,
            at: 0,
            after: None,
            page_size,
            head,
            pages: 0,
            records: 0,
        })
    }

    /// The outbox head the snapshot is as of.
    #[must_use]
    pub fn head(&self) -> Head {
        self.head
    }

    /// How many pages were read (the bounded-memory assertion counts these, not the process size).
    #[must_use]
    pub fn pages_read(&self) -> usize {
        self.pages
    }

    /// How many records were read.
    #[must_use]
    pub fn records_read(&self) -> u64 {
        self.records
    }

    /// The next page of images, in `(entity, id)` order, or `None` when every entity is read.
    ///
    /// # Errors
    /// [`SnapshotPgError::ReadFailed`] or [`SnapshotPgError::ImageInvalid`].
    pub async fn next_page(&mut self) -> Result<Option<Vec<ImageRecord>>, SnapshotPgError> {
        loop {
            let Some(entity) = self.entities.get(self.at).cloned() else {
                return Ok(None);
            };
            let after = self
                .after
                .clone()
                .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_owned());
            let limit = self.page_size;
            let rows = self
                .tx
                .raw_query(&document_page_sql(&entity), &[&after, &limit], None)
                .await
                .map_err(read_failed)?;
            if rows.is_empty() {
                self.at += 1;
                self.after = None;
                continue;
            }
            let page = self.decode(&entity, &rows).await?;
            self.after = page.last().map(|r| r.id.clone());
            if i64::try_from(rows.len()).unwrap_or(i64::MAX) < self.page_size {
                self.at += 1;
                self.after = None;
            }
            self.pages += 1;
            self.records += u64::try_from(page.len()).unwrap_or(0);
            return Ok(Some(page));
        }
    }

    /// Ends the snapshot (a read-only transaction has nothing to keep).
    pub async fn close(self) {
        let _ = self.tx.rollback().await;
    }

    async fn decode(
        &mut self,
        entity: &EntityDef,
        rows: &[Row],
    ) -> Result<Vec<ImageRecord>, SnapshotPgError> {
        let mut ids = Vec::with_capacity(rows.len());
        let mut images: Vec<(Map<String, Json>, u64)> = Vec::with_capacity(rows.len());
        for row in rows {
            let id = required(row, 0)?;
            let version = required(row, 1)?
                .parse::<u64>()
                .map_err(|_| SnapshotPgError::ReadFailed("a stored version is not a number"))?;
            let mut body = Map::new();
            body.insert("id".to_owned(), Json::String(id.clone()));
            body.insert("version".to_owned(), Json::from(version));
            for (at, field) in entity.fields.iter().enumerate() {
                let value = text_at(row, at + 2)?;
                body.insert(
                    field.name.as_str().to_owned(),
                    scalar(&field.ty, value.as_deref())?,
                );
            }
            ids.push(id);
            images.push((body, version));
        }
        let children = self.children(entity, &ids).await?;
        let mut out = Vec::with_capacity(images.len());
        for (((mut body, version), id), child) in images.into_iter().zip(ids).zip(children) {
            body.extend(child);
            out.push(ImageRecord {
                entity: entity.name.as_str().to_owned(),
                id,
                version,
                entity_schema: self.catalog.version,
                image: Json::Object(body),
            });
        }
        Ok(out)
    }

    /// One array of child rows per declared child table, for each of `ids`, in order.
    async fn children(
        &mut self,
        entity: &EntityDef,
        ids: &[String],
    ) -> Result<Vec<Map<String, Json>>, SnapshotPgError> {
        let mut per_record: Vec<Map<String, Json>> = vec![Map::new(); ids.len()];
        let position: BTreeMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(at, id)| (id.as_str(), at))
            .collect();
        for table in &entity.children {
            let child = self
                .catalog
                .entity(&table.entity)
                .map_err(|_| {
                    SnapshotPgError::OptionsInvalid("a child table names an unknown entity")
                })?
                .clone();
            let wanted: Vec<String> = ids.to_vec();
            let rows = self
                .tx
                .raw_query(&child_page_sql(&child), &[&wanted], None)
                .await
                .map_err(read_failed)?;
            let mut rows_of: BTreeMap<usize, Vec<Json>> = BTreeMap::new();
            for row in &rows {
                let parent = required(row, 0)?;
                let Some(at) = position.get(parent.as_str()).copied() else {
                    continue;
                };
                let id = required(row, 1)?;
                let idx = required(row, 2)?
                    .parse::<i64>()
                    .map_err(|_| SnapshotPgError::ReadFailed("a child position is not a number"))?;
                let mut out = Map::new();
                out.insert("id".to_owned(), Json::String(id));
                out.insert("idx".to_owned(), Json::from(idx));
                for (field_at, field) in child.fields.iter().enumerate() {
                    let value = text_at(row, field_at + 3)?;
                    out.insert(
                        field.name.as_str().to_owned(),
                        scalar(&field.ty, value.as_deref())?,
                    );
                }
                rows_of.entry(at).or_default().push(Json::Object(out));
            }
            for (at, map) in per_record.iter_mut().enumerate() {
                map.insert(
                    table.field.as_str().to_owned(),
                    Json::Array(rows_of.remove(&at).unwrap_or_default()),
                );
            }
        }
        Ok(per_record)
    }
}
