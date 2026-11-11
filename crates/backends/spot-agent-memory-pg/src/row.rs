//! The rows: one entry written as a row, and a row read back as one entry.
//!
//! Every statement here is a `const` with `$n` placeholders — a value ever reaches SQL as a
//! bound parameter, and the only identifiers in a statement are table and column names written
//! in this crate. Structured columns (`actions`, `steps`, `confirmations`, `value`) are bound as
//! text and cast with `::jsonb`, and read back with `::text`, so the driver needs no JSON type
//! and the crate keeps decoding through the domain constructors
//! ([`crate::wire`]).
//!
//! Reads are tenant-scoped: every statement filters on `tenant = $1`, so a store bound to one
//! tenant cannot return another's row even if it is handed the wrong database.

use spot_agent_memory::{
    AuditRef, Entry, EntryMeta, Episode, Fact, MemoryError, MemoryKind, Procedure, Sanitiser,
    SourceKind, Symptom, TenantId,
};
use tokio_postgres::Client;
use tokio_postgres::Row;

use crate::codec::{
    confidence_from_i16, confidence_to_i16, corrupt, episode_id_from_text, epoch_from_i64,
    epoch_to_i64, fact_id_from_text, fact_key_from_text, outcome_from_text, procedure_id_from_text,
    record_from_columns, source_from_text, source_to_text, summary_from_text, symptom_from_text,
    tenant_from_text, tier_from_i16, tier_to_i16, vector_from_text, vector_to_text,
};
use crate::error::store;
use crate::wire;

/// The columns an episode row is read by, in the order [`episode_from_row`] decodes them.
const EPISODE_COLUMNS: &str = "id, incident_doctype, incident_id, symptoms, actions::text, outcome, \
     summary, audits, remedies, tier, created_ms, ttl_ms, confidence_bp, source, embedding::text";

/// The columns a procedure row is read by.
const PROCEDURE_COLUMNS: &str = "id, title, symptoms, steps::text, confirmations::text, remedies, tier, created_ms, ttl_ms, \
     confidence_bp, source, embedding::text";

/// The columns a fact row is read by.
const FACT_COLUMNS: &str = "id, fact_key, value::text, epoch, tier, created_ms, ttl_ms, confidence_bp, source, \
     embedding::text";

const PUT_EPISODE: &str = "INSERT INTO spot_memory_episode (tenant, id, incident_doctype, \
     incident_id, symptoms, actions, outcome, summary, audits, remedies, tier, created_ms, ttl_ms, \
     confidence_bp, source, embedding) \
     VALUES ($1, $2, $3, $4, $5, $6::text::jsonb, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16::text::vector) \
     ON CONFLICT (tenant, id) DO UPDATE SET incident_doctype = EXCLUDED.incident_doctype, \
     incident_id = EXCLUDED.incident_id, symptoms = EXCLUDED.symptoms, actions = EXCLUDED.actions, \
     outcome = EXCLUDED.outcome, summary = EXCLUDED.summary, audits = EXCLUDED.audits, \
     remedies = EXCLUDED.remedies, tier = EXCLUDED.tier, created_ms = EXCLUDED.created_ms, \
     ttl_ms = EXCLUDED.ttl_ms, confidence_bp = EXCLUDED.confidence_bp, source = EXCLUDED.source, \
     embedding = EXCLUDED.embedding";

const PUT_PROCEDURE: &str = "INSERT INTO spot_memory_procedure (tenant, id, title, symptoms, steps, \
     confirmations, remedies, tier, created_ms, ttl_ms, confidence_bp, source, embedding) \
     VALUES ($1, $2, $3, $4, $5::text::jsonb, $6::text::jsonb, $7, $8, $9, $10, $11, $12, $13::text::vector) \
     ON CONFLICT (tenant, id) DO UPDATE SET title = EXCLUDED.title, symptoms = EXCLUDED.symptoms, \
     steps = EXCLUDED.steps, confirmations = EXCLUDED.confirmations, remedies = EXCLUDED.remedies, \
     tier = EXCLUDED.tier, created_ms = EXCLUDED.created_ms, ttl_ms = EXCLUDED.ttl_ms, \
     confidence_bp = EXCLUDED.confidence_bp, source = EXCLUDED.source, \
     embedding = EXCLUDED.embedding";

const PUT_FACT: &str = "INSERT INTO spot_memory_fact (tenant, id, fact_key, value, epoch, \
     display_text, tier, created_ms, ttl_ms, confidence_bp, source, embedding) \
     VALUES ($1, $2, $3, $4::text::jsonb, $5, $6, $7, $8, $9, $10, $11, $12::text::vector) \
     ON CONFLICT (tenant, id) DO UPDATE SET fact_key = EXCLUDED.fact_key, value = EXCLUDED.value, \
     epoch = EXCLUDED.epoch, display_text = EXCLUDED.display_text, tier = EXCLUDED.tier, \
     created_ms = EXCLUDED.created_ms, ttl_ms = EXCLUDED.ttl_ms, \
     confidence_bp = EXCLUDED.confidence_bp, source = EXCLUDED.source, \
     embedding = EXCLUDED.embedding";

/// Writes one entry, replacing the entry of the same tenant, kind and id.
///
/// # Errors
/// [`MemoryError::EntryInvalid`] when the entry is not storable, and [`MemoryError::Store`] when
/// the memory database refuses.
pub(crate) async fn put(client: &Client, entry: &Entry) -> Result<(), MemoryError> {
    entry.validate()?;
    let meta = entry.meta();
    let tenant = meta.tenant.as_str();
    let id = entry.id_str();
    let symptoms = symptom_texts(entry);
    let vector = vector_to_text(entry.vector())?;
    let tier = tier_to_i16(meta.tier);
    let confidence = confidence_to_i16(meta.confidence);
    let source = source_to_text(meta.source);
    let created = meta.created_ms;
    if !(0..=MAX_TIME_MS).contains(&created) {
        return Err(MemoryError::Store {
            detail: "an entry's creation instant is outside the range the store keeps".to_owned(),
        });
    }
    // "Never expires" (T1 allows `i64::MAX`) is stored as the longest life the column keeps.
    let ttl = meta.ttl_ms.map(|ttl| ttl.min(MAX_TTL_MS));
    match entry {
        Entry::Episode(episode) => {
            let actions = wire::actions_to_json(&episode.actions)?;
            let summary = episode.summary.as_str();
            let audits = audit_texts(episode);
            let remedies = remedy_texts(entry);
            client
                .execute(
                    PUT_EPISODE,
                    &[
                        &tenant,
                        &id,
                        &episode.incident.doctype,
                        &episode.incident.id,
                        &symptoms,
                        &actions,
                        &episode.outcome.as_str(),
                        &summary,
                        &audits,
                        &remedies,
                        &tier,
                        &created,
                        &ttl,
                        &confidence,
                        &source,
                        &vector,
                    ],
                )
                .await
                .map_err(|error| store(&error))?;
        }
        Entry::Procedure(procedure) => {
            let steps = wire::steps_to_json(&procedure.steps)?;
            let confirmations = wire::confirmations_to_json(&procedure.confirmations)?;
            let remedies = remedy_texts(entry);
            let title = procedure.title.as_str();
            client
                .execute(
                    PUT_PROCEDURE,
                    &[
                        &tenant,
                        &id,
                        &title,
                        &symptoms,
                        &steps,
                        &confirmations,
                        &remedies,
                        &tier,
                        &created,
                        &ttl,
                        &confidence,
                        &source,
                        &vector,
                    ],
                )
                .await
                .map_err(|error| store(&error))?;
        }
        Entry::Fact(fact) => {
            let value = wire::fact_value_to_json(&fact.value)?;
            let display = fact.text();
            let key = fact.key.as_str();
            let epoch = epoch_to_i64(fact.epoch)?;
            client
                .execute(
                    PUT_FACT,
                    &[
                        &tenant,
                        &id,
                        &key,
                        &value,
                        &epoch,
                        &display,
                        &tier,
                        &created,
                        &ttl,
                        &confidence,
                        &source,
                        &vector,
                    ],
                )
                .await
                .map_err(|error| store(&error))?;
        }
    }
    Ok(())
}

/// The entry of `kind` with `id` in `tenant`, if it is there.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a stored row cannot be decoded.
pub(crate) async fn get(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
    id: &str,
    sanitiser: &Sanitiser,
) -> Result<Option<Entry>, MemoryError> {
    let owner = tenant.as_str();
    let statement = format!(
        "SELECT {} FROM {} WHERE tenant = $1 AND id = $2",
        columns(kind),
        table(kind)
    );
    let rows = client
        .query(&statement, &[&owner, &id])
        .await
        .map_err(|error| store(&error))?;
    match rows.first() {
        Some(row) => entry_from_row(row, kind, tenant, sanitiser).map(Some),
        None => Ok(None),
    }
}

/// The most rows one page may hold, whatever the caller asks for.
pub const MAX_PAGE: usize = 1_000;

/// The largest value a time column may hold: the last millisecond of year 9999.
///
/// A bound on `created_ms` and on the `now_ms` a read is made at keeps every subtraction and
/// product in the recall statement far from `bigint` overflow.
pub const MAX_TIME_MS: i64 = 253_402_300_799_999;

/// The longest time to live the table stores, about 100 years in milliseconds. A longer one (T1
/// allows `i64::MAX`, "never") is stored as this: no entry lives that long, and the column
/// stays a value arithmetic cannot overflow on.
pub const MAX_TTL_MS: i64 = 3_153_600_000_000;

const LIST_PAGE: &str = "SELECT {COLUMNS} FROM {TABLE} WHERE tenant = $1 \
     AND ($2::text IS NULL OR id > $2::text) ORDER BY id LIMIT $3";

/// One page of entries of `kind` in `tenant`, ordered by id, starting after `after_id`.
///
/// `limit` is clamped to `1..=`[`MAX_PAGE`]. The last id of a page is the `after_id` of the next;
/// a page shorter than the limit is the last.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a stored row cannot be decoded.
pub(crate) async fn list_page(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
    after_id: Option<&str>,
    limit: usize,
    sanitiser: &Sanitiser,
) -> Result<Vec<Entry>, MemoryError> {
    let owner = tenant.as_str();
    let limit = clamp_page(limit);
    let statement = LIST_PAGE
        .replace("{COLUMNS}", columns(kind))
        .replace("{TABLE}", table(kind));
    let rows = client
        .query(&statement, &[&owner, &after_id, &limit])
        .await
        .map_err(|error| store(&error))?;
    rows.iter()
        .map(|row| entry_from_row(row, kind, tenant, sanitiser))
        .collect()
}

/// A page size as the `LIMIT` parameter: at least one row, at most [`MAX_PAGE`].
pub(crate) fn clamp_page(limit: usize) -> i64 {
    i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1)
}

/// Every entry of `kind` in `tenant`, ordered by id, read page by page and stopping at
/// `max_entries` rows (the store's own per-kind cap, so a table filled behind the store's back
/// cannot make one call read without end).
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a stored row cannot be decoded.
pub(crate) async fn list(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
    max_entries: usize,
    sanitiser: &Sanitiser,
) -> Result<Vec<Entry>, MemoryError> {
    let mut all: Vec<Entry> = Vec::new();
    let mut after: Option<String> = None;
    while all.len() < max_entries {
        let want = (max_entries - all.len()).min(MAX_PAGE);
        let page = list_page(client, tenant, kind, after.as_deref(), want, sanitiser).await?;
        let full = page.len() >= want;
        if let Some(last) = page.last() {
            after = Some(last.id_str().to_owned());
        }
        all.extend(page);
        if !full {
            break;
        }
    }
    Ok(all)
}

const PURGE: &str = "DELETE FROM {TABLE} WHERE tenant = $1 AND id IN (\
     SELECT id FROM {TABLE} WHERE tenant = $1 AND ttl_ms IS NOT NULL \
     AND ttl_ms <= $2::bigint - created_ms ORDER BY created_ms, id LIMIT $3)";

/// Deletes the entries of `tenant` whose time to live has run out at `now_ms`, at most `batch`
/// rows per statement, until none are left. Returns how many went.
///
/// This is the hard TTL only: it does not know a legal hold (that is the domain retention plan,
/// which reads through [`list`]). `now_ms` is clamped to `0..=`[`MAX_TIME_MS`].
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses.
pub(crate) async fn purge_expired(
    client: &Client,
    tenant: &TenantId,
    now_ms: i64,
    batch: usize,
) -> Result<u64, MemoryError> {
    let owner = tenant.as_str();
    let now = now_ms.clamp(0, MAX_TIME_MS);
    let limit = clamp_page(batch);
    let mut total: u64 = 0;
    for kind in MemoryKind::all() {
        let statement = PURGE.replace("{TABLE}", table(kind));
        loop {
            let removed = client
                .execute(&statement, &[&owner, &now, &limit])
                .await
                .map_err(|error| store(&error))?;
            total = total.saturating_add(removed);
            if i64::try_from(removed).is_ok_and(|removed| removed < limit) {
                break;
            }
        }
    }
    Ok(total)
}

/// Refuses a write that would add a new row beyond `max_entries` of `kind` for `tenant`.
///
/// A replacement of an existing id always passes. The check and the insert are two statements,
/// so a burst of concurrent writers can overshoot the cap by the number of writers; the cap
/// bounds growth, it is not an exact quota.
///
/// # Errors
/// [`MemoryError::Store`] when the kind is full or the memory database refuses.
pub(crate) async fn ensure_room(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
    id: &str,
    max_entries: usize,
) -> Result<(), MemoryError> {
    let owner = tenant.as_str();
    let statement = format!(
        "SELECT count(*)::bigint, coalesce(bool_or(id = $2), false) FROM {} WHERE tenant = $1",
        table(kind)
    );
    let row = client
        .query_one(&statement, &[&owner, &id])
        .await
        .map_err(|error| store(&error))?;
    let count: i64 = row.try_get(0).map_err(|_| corrupt("count"))?;
    let present: bool = row.try_get(1).map_err(|_| corrupt("count"))?;
    let max = i64::try_from(max_entries).unwrap_or(i64::MAX);
    if !present && count >= max {
        return Err(MemoryError::Store {
            detail: format!(
                "the {} memory of this tenant is full; run retention before writing more",
                kind.as_str()
            ),
        });
    }
    Ok(())
}

/// Removes one entry, reporting whether it was there.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses.
pub(crate) async fn remove(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
    id: &str,
) -> Result<bool, MemoryError> {
    let owner = tenant.as_str();
    let statement = format!("DELETE FROM {} WHERE tenant = $1 AND id = $2", table(kind));
    let removed = client
        .execute(&statement, &[&owner, &id])
        .await
        .map_err(|error| store(&error))?;
    Ok(removed > 0)
}

/// The most tenants one call to [`tenants`] returns.
pub const MAX_TENANTS: usize = 1_000;

/// One page of the tenants one of the three tables holds a row for, ordered, after `after`.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses.
pub(crate) async fn tenants_page(
    client: &Client,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<TenantId>, MemoryError> {
    let limit = clamp_page(limit);
    let rows = client
        .query(
            "SELECT tenant FROM (SELECT tenant FROM spot_memory_episode \
             UNION SELECT tenant FROM spot_memory_procedure \
             UNION SELECT tenant FROM spot_memory_fact) t \
             WHERE $1::text IS NULL OR tenant > $1::text ORDER BY 1 LIMIT $2",
            &[&after, &limit],
        )
        .await
        .map_err(|error| store(&error))?;
    let mut tenants = Vec::with_capacity(rows.len());
    for row in rows {
        let text: String = row.try_get(0).map_err(|_| corrupt("tenant"))?;
        tenants.push(tenant_from_text("tenant", &text)?);
    }
    Ok(tenants)
}

/// Every tenant the tables hold a row for, read in pages, at most [`MAX_TENANTS`].
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses.
pub(crate) async fn tenants(client: &Client) -> Result<Vec<TenantId>, MemoryError> {
    let mut all: Vec<TenantId> = Vec::new();
    let mut after: Option<String> = None;
    while all.len() < MAX_TENANTS {
        let want = (MAX_TENANTS - all.len()).min(MAX_PAGE);
        let page = tenants_page(client, after.as_deref(), want).await?;
        let full = page.len() >= want;
        if let Some(last) = page.last() {
            after = Some(last.as_str().to_owned());
        }
        all.extend(page);
        if !full {
            break;
        }
    }
    Ok(all)
}

/// How many rows one kind holds for `tenant`.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses.
pub(crate) async fn count(
    client: &Client,
    tenant: &TenantId,
    kind: MemoryKind,
) -> Result<i64, MemoryError> {
    let owner = tenant.as_str();
    let statement = format!("SELECT count(*) FROM {} WHERE tenant = $1", table(kind));
    client
        .query_one(&statement, &[&owner])
        .await
        .map_err(|error| store(&error))?
        .try_get(0)
        .map_err(|_| corrupt("count"))
}

/// The table one kind lives in.
#[must_use]
pub(crate) fn table(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Episode => "spot_memory_episode",
        MemoryKind::Procedure => "spot_memory_procedure",
        MemoryKind::Fact => "spot_memory_fact",
    }
}

/// The columns one kind is read by.
fn columns(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Episode => EPISODE_COLUMNS,
        MemoryKind::Procedure => PROCEDURE_COLUMNS,
        MemoryKind::Fact => FACT_COLUMNS,
    }
}

/// One entry of `kind` from its row.
fn entry_from_row(
    row: &Row,
    kind: MemoryKind,
    tenant: &TenantId,
    sanitiser: &Sanitiser,
) -> Result<Entry, MemoryError> {
    let entry = match kind {
        MemoryKind::Episode => Entry::Episode(episode_from_row(row, tenant, sanitiser)?),
        MemoryKind::Procedure => Entry::Procedure(procedure_from_row(row, tenant, sanitiser)?),
        MemoryKind::Fact => Entry::Fact(fact_from_row(row, tenant, sanitiser)?),
    };
    // A row that no longer satisfies its kind's rules is a corrupt row, not a valid entry: the
    // store refuses it rather than handing a caller something a fresh record would not produce.
    entry.validate().map_err(|_| corrupt(kind.as_str()))?;
    Ok(entry)
}

fn episode_from_row(
    row: &Row,
    tenant: &TenantId,
    sanitiser: &Sanitiser,
) -> Result<Episode, MemoryError> {
    let id = episode_id_from_text("id", &text(row, 0, "id")?)?;
    let doctype = text(row, 1, "incident_doctype")?;
    let record_id = text(row, 2, "incident_id")?;
    let incident = record_from_columns("incident_doctype", "incident_id", &doctype, &record_id)?;
    let symptoms = symptoms_from(row, 3, "symptoms")?;
    let actions = wire::actions_from_json("actions", &text(row, 4, "actions")?)?;
    let outcome = outcome_from_text("outcome", &text(row, 5, "outcome")?)?;
    let summary = summary_from_text("summary", sanitiser, &text(row, 6, "summary")?)?;
    let audits = audits_from(row, 7)?;
    let vector = vector_from_text("embedding", &text(row, 14, "embedding")?)?;
    Ok(Episode {
        id,
        incident,
        symptoms,
        actions,
        outcome,
        summary,
        audits,
        meta: meta_from_row(row, 9, tenant)?,
        vector,
    })
}

fn procedure_from_row(
    row: &Row,
    tenant: &TenantId,
    sanitiser: &Sanitiser,
) -> Result<Procedure, MemoryError> {
    let id = procedure_id_from_text("id", &text(row, 0, "id")?)?;
    let title = summary_from_text("title", sanitiser, &text(row, 1, "title")?)?;
    let symptoms = symptoms_from(row, 2, "symptoms")?;
    let steps = wire::steps_from_json("steps", sanitiser, &text(row, 3, "steps")?)?;
    let confirmations =
        wire::confirmations_from_json("confirmations", &text(row, 4, "confirmations")?)?;
    let vector = vector_from_text("embedding", &text(row, 11, "embedding")?)?;
    Ok(Procedure {
        id,
        title,
        symptoms,
        steps,
        confirmations,
        meta: meta_from_row(row, 6, tenant)?,
        vector,
    })
}

fn fact_from_row(row: &Row, tenant: &TenantId, sanitiser: &Sanitiser) -> Result<Fact, MemoryError> {
    let id = fact_id_from_text("id", &text(row, 0, "id")?)?;
    let key = fact_key_from_text("fact_key", &text(row, 1, "fact_key")?)?;
    let value = wire::fact_value_from_json("value", sanitiser, &text(row, 2, "value")?)?;
    let epoch = epoch_from_i64("epoch", integer(row, 3, "epoch")?)?;
    let vector = vector_from_text("embedding", &text(row, 9, "embedding")?)?;
    Ok(Fact {
        id,
        key,
        value,
        epoch,
        meta: meta_from_row(row, 4, tenant)?,
        vector,
    })
}

/// The metadata columns, which start at `base` on every table.
fn meta_from_row(row: &Row, base: usize, tenant: &TenantId) -> Result<EntryMeta, MemoryError> {
    let tier = tier_from_i16("tier", smallint(row, base, "tier")?)?;
    let created_ms = integer(row, base + 1, "created_ms")?;
    let ttl_ms = row
        .try_get::<_, Option<i64>>(base + 2)
        .map_err(|_| corrupt("ttl_ms"))?;
    let confidence =
        confidence_from_i16("confidence_bp", smallint(row, base + 3, "confidence_bp")?)?;
    let source: SourceKind = source_from_text("source", &text(row, base + 4, "source")?)?;
    Ok(EntryMeta {
        tier,
        tenant: tenant.clone(),
        created_ms,
        ttl_ms,
        confidence,
        source,
    })
}

fn row_value<'a, T>(row: &'a Row, index: usize, column: &str) -> Result<T, MemoryError>
where
    T: tokio_postgres::types::FromSql<'a>,
{
    row.try_get(index).map_err(|_| corrupt(column))
}

fn text(row: &Row, index: usize, column: &str) -> Result<String, MemoryError> {
    row_value(row, index, column)
}

fn integer(row: &Row, index: usize, column: &str) -> Result<i64, MemoryError> {
    row_value(row, index, column)
}

fn smallint(row: &Row, index: usize, column: &str) -> Result<i16, MemoryError> {
    row_value(row, index, column)
}

fn strings(row: &Row, index: usize, column: &str) -> Result<Vec<String>, MemoryError> {
    row_value(row, index, column)
}

fn symptoms_from(row: &Row, index: usize, column: &str) -> Result<Vec<Symptom>, MemoryError> {
    strings(row, index, column)?
        .iter()
        .map(|tag| symptom_from_text(column, tag))
        .collect()
}

fn audits_from(row: &Row, index: usize) -> Result<Vec<AuditRef>, MemoryError> {
    strings(row, index, "audits")?
        .iter()
        .map(|id| AuditRef::parse(id).map_err(|_| corrupt("audits")))
        .collect()
}

/// An entry's symptom tags as the `text[]` column.
fn symptom_texts(entry: &Entry) -> Vec<String> {
    entry
        .symptoms()
        .iter()
        .map(|symptom| symptom.as_str().to_owned())
        .collect()
}

/// An entry's audits as the `text[]` column: an episode's, and empty for the other kinds.
fn audit_texts(episode: &Episode) -> Vec<String> {
    episode
        .audits
        .iter()
        .map(|audit| audit.as_str().to_owned())
        .collect()
}

/// An entry's remedies as the `text[]` column, so a recall can filter on them with an index.
fn remedy_texts(entry: &Entry) -> Vec<String> {
    let mut remedies: Vec<String> = entry
        .remedies()
        .into_iter()
        .map(ToString::to_string)
        .collect();
    remedies.sort_unstable();
    remedies.dedup();
    remedies
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_own_table_and_columns() {
        for kind in MemoryKind::all() {
            assert!(table(kind).starts_with("spot_memory_"));
            assert!(columns(kind).contains("embedding::text"));
        }
        assert_eq!(MemoryKind::all().len(), 3);
    }

    #[test]
    fn the_statements_bind_every_value() {
        for statement in [PUT_EPISODE, PUT_PROCEDURE, PUT_FACT] {
            assert!(statement.contains("$1"), "{statement}");
            assert!(!statement.contains("'"), "no literal reaches the statement");
        }
    }
}
