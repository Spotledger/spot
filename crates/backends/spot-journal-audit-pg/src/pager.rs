//! Reading a page: the row-to-summary decoding both sides share, and the paging iterator.
//!
//! One page is one keyset page of documents (`WHERE id > $1 ORDER BY id LIMIT $2`) plus, per
//! declared child table of the entity, one page of the child rows of exactly those documents. The
//! body built from those rows is the ADR-055 image of [`crate::encode`], and its digest is the one
//! the in-memory side computes, so the two sides of an audit are comparable record by record
//! (`spot_journal_audit_run::memory::body_digest`).

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Map, Value as Json};
use spot_journal_audit::{Fingerprint, RecordSummary};
use spot_pg_bridge::PooledConn;
use spot_storage_contract::EntityDef;

use crate::encode::{body_digest, child_row, scalar};
use crate::error::{PgSideError, QUERY_FAILED, options_invalid, query_failed};
use crate::session::Session;
use crate::sql::{Scope, build_fingerprint, child_page, document_page, fingerprint};

/// The smallest record id, so the first page needs no special statement.
const FIRST_ID: &str = "00000000-0000-0000-0000-000000000000";

type Param<'a> = &'a (dyn tokio_postgres::types::ToSql + Sync);

/// One bounded keyset page of summaries of `entity`, sorted by id.
///
/// # Errors
///
/// [`QUERY_FAILED`] when a statement fails, and the encoder's refusals for a row whose stored
/// value contradicts its declared type.
pub(crate) async fn page(
    conn: &PooledConn,
    scope: &Scope,
    entity: &EntityDef,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<RecordSummary>, PgSideError> {
    let after = after.unwrap_or(FIRST_ID).to_owned();
    let after: Param<'_> = &after;
    let limit: Param<'_> = &limit;
    let rows = conn
        .query(&document_page(scope, entity), &[after, limit])
        .await
        .map_err(|error| query_failed(&error))?;

    let mut ids: Vec<String> = Vec::with_capacity(rows.len());
    let mut versions: Vec<u64> = Vec::with_capacity(rows.len());
    let mut bodies: Vec<Map<String, Json>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let id = text(row, 0, "a record id")?;
        let version = text(row, 1, "a record version")?
            .parse::<u64>()
            .map_err(|_| query_failed_text("a stored record version is not a version"))?;
        let mut body = Map::new();
        for (at, field) in entity.fields.iter().enumerate() {
            let value = row
                .try_get::<usize, Option<String>>(at + 2)
                .map_err(|_| query_failed_text("a stored value is not text"))?;
            body.insert(
                field.name.as_str().to_owned(),
                scalar(&field.ty, value.as_deref())?,
            );
        }
        ids.push(id);
        versions.push(version);
        bodies.push(body);
    }
    if !ids.is_empty() {
        let children = read_children(conn, scope, entity, &ids).await?;
        for (body, child) in bodies.iter_mut().zip(children) {
            body.extend(child);
        }
    }

    let mut summaries = Vec::with_capacity(ids.len());
    for (at, id) in ids.iter().enumerate() {
        summaries.push(RecordSummary::new(
            entity.name.as_str(),
            id.as_str(),
            versions.get(at).copied().unwrap_or(0),
            body_digest(bodies.get(at).unwrap_or(&Map::new()))?,
        ));
    }
    Ok(summaries)
}

/// The declared child tables of `entity`, one array of row objects per table, for each of `ids` in
/// the same order.
async fn read_children(
    conn: &PooledConn,
    scope: &Scope,
    entity: &EntityDef,
    ids: &[String],
) -> Result<Vec<Map<String, Json>>, PgSideError> {
    let mut per_record: Vec<Map<String, Json>> = vec![Map::new(); ids.len()];
    if entity.children.is_empty() {
        return Ok(per_record);
    }
    let position: BTreeMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(at, id)| (id.as_str(), at))
        .collect();
    let ids: Vec<String> = ids.to_vec();
    for table in &entity.children {
        let child = scope.child_entity(&table.entity).ok_or_else(|| {
            options_invalid("a child table names an entity the catalogue does not declare")
        })?;
        let ids: Param<'_> = &ids;
        let rows = conn
            .query(&child_page(scope, child), &[ids])
            .await
            .map_err(|error| query_failed(&error))?;
        let mut rows_of: BTreeMap<usize, Vec<Json>> = BTreeMap::new();
        for row in &rows {
            let parent = text(row, 0, "a child row's parent")?;
            let Some(at) = position.get(parent.as_str()).copied() else {
                continue;
            };
            let id = text(row, 1, "a child row's id")?;
            let idx = text(row, 2, "a child row's position")?
                .parse::<i64>()
                .map_err(|_| query_failed_text("a child row's position is not an integer"))?;
            let mut fields = Vec::with_capacity(child.fields.len());
            for (field_at, field) in child.fields.iter().enumerate() {
                let value = row
                    .try_get::<usize, Option<String>>(field_at + 3)
                    .map_err(|_| query_failed_text("a stored value is not text"))?;
                fields.push((
                    field.name.as_str().to_owned(),
                    scalar(&field.ty, value.as_deref())?,
                ));
            }
            rows_of
                .entry(at)
                .or_default()
                .push(child_row(&id, idx, fields));
        }
        let field = table.field.as_str().to_owned();
        for (at, map) in per_record.iter_mut().enumerate() {
            let rows = rows_of.remove(&at).unwrap_or_default();
            map.insert(field.clone(), Json::Array(rows));
        }
    }
    Ok(per_record)
}

/// Every fingerprint this scope can answer, sorted as [`Fingerprint::new`] sorts them.
///
/// A declaration naming an entity the catalogue does not hold is skipped, so a tenant whose stage
/// has no ledger yet answers **no** fingerprints and the compare covers records only (WP-MIG-06
/// registers the real declarations when the ERP exists).
///
/// # Errors
///
/// [`QUERY_FAILED`] when a statement fails, or a refusal when a summed value is not the declared
/// exact form.
pub(crate) async fn fingerprints(
    conn: &PooledConn,
    scope: &Scope,
) -> Result<Vec<Fingerprint>, PgSideError> {
    let mut out = Vec::new();
    for query in scope.fingerprints() {
        if scope.entity(&query.entity).is_none() {
            continue;
        }
        let rows = conn
            .query(&fingerprint(scope, query)?, &[])
            .await
            .map_err(|error| query_failed(&error))?;
        let mut values = Vec::with_capacity(rows.len());
        for row in &rows {
            let key = text(row, 0, "a fingerprint key")?;
            let amount = text(row, 1, "a fingerprint amount")?;
            values.push((key, amount));
        }
        out.push(build_fingerprint(query, values)?);
    }
    Ok(out)
}

/// The value of column `at`, which the select list writes as text.
fn text(row: &tokio_postgres::Row, at: usize, what: &str) -> Result<String, PgSideError> {
    row.try_get::<usize, Option<String>>(at)
        .map_err(|_| query_failed_text("a stored value is not text"))?
        .ok_or_else(|| {
            crate::error::error(
                QUERY_FAILED,
                format!("{what} is null, which no record allows"),
            )
        })
}

fn query_failed_text(reason: &'static str) -> PgSideError {
    crate::error::error(QUERY_FAILED, reason.to_owned())
}

/// The summaries of one side, one bounded page at a time.
///
/// The iterator holds the current page and the resume key of the entity it is in, never the rows
/// it has already yielded: memory is constant in the size of the tables (`max(page size)`).
pub(crate) struct Pages<'a> {
    session: &'a Session,
    scope: &'a Scope,
    entity: usize,
    after: Option<String>,
    buffer: VecDeque<RecordSummary>,
    finished: bool,
}

impl<'a> Pages<'a> {
    /// A stream over every document entity of `scope`, in name order.
    pub(crate) fn new(session: &'a Session, scope: &'a Scope) -> Self {
        Self {
            session,
            scope,
            entity: 0,
            after: None,
            buffer: VecDeque::new(),
            finished: false,
        }
    }
}

impl Iterator for Pages<'_> {
    type Item = RecordSummary;

    /// The next summary, or `None` at the end of the stream. A failure ends the stream and is
    /// recorded on the side (`failure()`), because the trait's stream cannot carry one: a caller
    /// that must not trust a truncated read has to ask the side afterwards.
    fn next(&mut self) -> Option<RecordSummary> {
        loop {
            if let Some(summary) = self.buffer.pop_front() {
                return Some(summary);
            }
            if self.finished {
                return None;
            }
            let Some(entity) = self.scope.entities().get(self.entity) else {
                self.finished = true;
                return None;
            };
            match self.session.page(
                entity.name.as_str(),
                self.after.as_deref(),
                self.scope.page_size(),
            ) {
                Ok(page) if page.is_empty() => {
                    // The entity is exhausted: move to the next one, from its first id.
                    self.entity += 1;
                    self.after = None;
                }
                Ok(page) => {
                    self.after = page.last().map(|summary| summary.id.clone());
                    self.buffer.extend(page);
                }
                Err(error) => {
                    self.session.record(&error);
                    self.finished = true;
                    return None;
                }
            }
        }
    }
}
