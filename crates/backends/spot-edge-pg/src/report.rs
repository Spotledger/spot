//! The drift report: what one verify run found, and how a repair reads it back.
//!
//! A report is one header row (`spot_edge_drift_report`) plus one row per drift
//! (`spot_edge_drift_entry`). The header carries when the run happened, how many documents it paged
//! through and whether the tenant was clean; an entry carries the kind of drift, the edge's
//! identity, the properties the documents imply (`expected`) and the properties the store holds
//! (`found`). A repair turns the entries back into [`EdgeChanges`] and applies them; it never
//! removes an edge that is not named by an entry.

use serde_json::Value as Json;
use spot_edge_sync::{Drift, Edge, EdgeChanges, EdgeUpdate};
use spot_postgres::PgTransaction;
use spot_storage_contract::NodeRef;
use time::OffsetDateTime;
use tokio_postgres::types::ToSql;

use crate::convert::{
    decode_properties, edge_type_of, encode_properties, entity_of, record_id_of, uuid_of,
};
use crate::error::{EdgePgError, Result};

/// What kind of drift one entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriftKind {
    /// The documents imply an edge the store does not hold.
    Missing,
    /// The store holds an edge no document implies.
    Extra,
    /// Both hold the edge and its properties differ.
    Mismatch,
}

impl DriftKind {
    /// The text stored in the `kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Extra => "extra",
            Self::Mismatch => "mismatch",
        }
    }

    /// The kind a stored text is.
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "missing" => Ok(Self::Missing),
            "extra" => Ok(Self::Extra),
            "mismatch" => Ok(Self::Mismatch),
            other => Err(EdgePgError::decode(format!(
                "`{other}` is not a drift kind"
            ))),
        }
    }
}

/// One drift, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriftEntry {
    /// What kind of drift it is.
    pub kind: DriftKind,
    /// The edge type id.
    pub edge_type: String,
    /// The edge's owner.
    pub owner: NodeRef,
    /// The edge's source.
    pub from: NodeRef,
    /// The edge's target.
    pub to: NodeRef,
    /// The properties the documents imply, when the entry has them.
    pub expected: Option<Json>,
    /// The properties the store holds, when the entry has them.
    pub found: Option<Json>,
}

impl DriftEntry {
    /// The entry one [`Drift`] becomes.
    pub fn from_drift(drift: &Drift) -> Result<Self> {
        let (kind, identity, expected, found) = match drift {
            Drift::Missing { expected } => (
                DriftKind::Missing,
                expected.as_ref(),
                Some(encode_properties(&expected.properties)?),
                None,
            ),
            Drift::Extra { found } => (
                DriftKind::Extra,
                found.as_ref(),
                None,
                Some(encode_properties(&found.properties)?),
            ),
            Drift::Mismatch {
                expected, found, ..
            } => (
                DriftKind::Mismatch,
                expected.as_ref(),
                Some(encode_properties(&expected.properties)?),
                Some(encode_properties(&found.properties)?),
            ),
        };
        Ok(Self {
            kind,
            edge_type: identity.edge_type.as_str().to_owned(),
            owner: identity.owner.clone(),
            from: identity.from.clone(),
            to: identity.to.clone(),
            expected,
            found,
        })
    }

    /// The edge the documents imply, when the entry carries it.
    pub fn expected_edge(&self) -> Result<Option<Edge>> {
        self.edge_from(self.expected.as_ref())
    }

    /// The edge the store holds, when the entry carries it.
    pub fn found_edge(&self) -> Result<Option<Edge>> {
        self.edge_from(self.found.as_ref())
    }

    fn edge_from(&self, properties: Option<&Json>) -> Result<Option<Edge>> {
        let Some(properties) = properties else {
            return Ok(None);
        };
        Ok(Some(Edge {
            edge_type: edge_type_of(&self.edge_type)?,
            owner: self.owner.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            properties: decode_properties(properties)?,
        }))
    }
}

/// One verify run's report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriftReport {
    /// The caller's key for this run.
    pub report_key: String,
    /// When the run happened (an argument, never read from a clock).
    pub generated_at: OffsetDateTime,
    /// How many documents the run paged through.
    pub checked_documents: u64,
    /// Every drift it found, in edge-key order.
    pub entries: Vec<DriftEntry>,
}

impl DriftReport {
    /// A report with no drift yet.
    #[must_use]
    pub fn new(report_key: impl Into<String>, generated_at: OffsetDateTime) -> Self {
        Self {
            report_key: report_key.into(),
            generated_at,
            checked_documents: 0,
            entries: Vec::new(),
        }
    }

    /// Whether the run found nothing to repair.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// The change that would fix this report, built only from its entries.
    ///
    /// A repair uses this, so it can only ever remove an edge the report named as
    /// [`DriftKind::Extra`] or change one it named as [`DriftKind::Mismatch`].
    pub fn changes(&self) -> Result<EdgeChanges> {
        let mut changes = EdgeChanges::default();
        for entry in &self.entries {
            match entry.kind {
                DriftKind::Missing => {
                    if let Some(edge) = entry.expected_edge()? {
                        changes.added.insert(edge);
                    }
                }
                DriftKind::Extra => {
                    if let Some(edge) = entry.found_edge()? {
                        changes.removed.insert(edge);
                    }
                }
                DriftKind::Mismatch => {
                    if let (Some(after), Some(before)) =
                        (entry.expected_edge()?, entry.found_edge()?)
                    {
                        changes.updated.push(EdgeUpdate {
                            key: after.key(),
                            before,
                            after,
                        });
                    }
                }
            }
        }
        Ok(changes)
    }
}

/// Writes a report and its entries in the caller's transaction.
///
/// # Errors
/// `edge_pg.write_failed` when the tables refuse the rows.
pub async fn write_report(tx: &mut PgTransaction, report: &DriftReport) -> Result<()> {
    let failed = |e: spot_storage_contract::StorageError| EdgePgError::write(e.explanation);
    let drift_count = i64::try_from(report.entries.len())
        .map_err(|_| EdgePgError::write("too many drift entries to store"))?;
    let checked = i64::try_from(report.checked_documents)
        .map_err(|_| EdgePgError::write("too many documents to count"))?;
    let header: [&(dyn ToSql + Sync); 5] = [
        &report.report_key,
        &report.generated_at,
        &checked,
        &drift_count,
        &report.is_clean(),
    ];
    tx.raw_execute(
        "INSERT INTO spot_edge_drift_report \
         (report_key, generated_at, checked_documents, drift_count, clean) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (report_key) DO UPDATE SET generated_at = EXCLUDED.generated_at, \
         checked_documents = EXCLUDED.checked_documents, drift_count = EXCLUDED.drift_count, \
         clean = EXCLUDED.clean",
        &header,
    )
    .await
    .map_err(failed)?;
    tx.raw_execute(
        "DELETE FROM spot_edge_drift_entry WHERE report_key = $1",
        &[&report.report_key],
    )
    .await
    .map_err(failed)?;
    for (ordinal, entry) in report.entries.iter().enumerate() {
        let ordinal = i32::try_from(ordinal)
            .map_err(|_| EdgePgError::write("too many drift entries to store"))?;
        let kind = entry.kind.as_str();
        let owner_id = uuid_of(entry.owner.id);
        let from_id = uuid_of(entry.from.id);
        let to_id = uuid_of(entry.to.id);
        let params: [&(dyn ToSql + Sync); 12] = [
            &report.report_key,
            &ordinal,
            &kind,
            &entry.edge_type,
            &entry.owner.entity.as_str().to_owned(),
            &owner_id,
            &entry.from.entity.as_str().to_owned(),
            &from_id,
            &entry.to.entity.as_str().to_owned(),
            &to_id,
            &entry.expected,
            &entry.found,
        ];
        tx.raw_execute(
            "INSERT INTO spot_edge_drift_entry \
             (report_key, ordinal, kind, edge_type, owner_entity, owner_id, from_entity, from_id, \
              to_entity, to_id, expected, found) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &params,
        )
        .await
        .map_err(failed)?;
    }
    Ok(())
}

/// Reads a report and its entries back, or `edge_pg.report_not_found`.
pub async fn read_report(tx: &mut PgTransaction, report_key: &str) -> Result<DriftReport> {
    let failed = |e: spot_storage_contract::StorageError| EdgePgError::write(e.explanation);
    let rows = tx
        .raw_query(
            "SELECT generated_at, checked_documents FROM spot_edge_drift_report WHERE report_key = $1",
            &[&report_key],
            None,
        )
        .await
        .map_err(failed)?;
    let Some(row) = rows.first() else {
        return Err(EdgePgError::ReportNotFound {
            report_key: report_key.to_owned(),
        });
    };
    let bad = |e: tokio_postgres::Error| EdgePgError::decode(e.to_string());
    let generated_at: OffsetDateTime = row.try_get(0).map_err(bad)?;
    let checked: i64 = row.try_get(1).map_err(bad)?;
    let entries = tx
        .raw_query(
            "SELECT kind, edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, \
             to_id, expected, found FROM spot_edge_drift_entry WHERE report_key = $1 ORDER BY ordinal",
            &[&report_key],
            None,
        )
        .await
        .map_err(failed)?;
    let mut decoded = Vec::with_capacity(entries.len());
    for row in &entries {
        decoded.push(decode_entry(row)?);
    }
    Ok(DriftReport {
        report_key: report_key.to_owned(),
        generated_at,
        checked_documents: u64::try_from(checked)
            .map_err(|_| EdgePgError::decode("a document count is negative"))?,
        entries: decoded,
    })
}

fn decode_entry(row: &tokio_postgres::Row) -> Result<DriftEntry> {
    let bad = |e: tokio_postgres::Error| EdgePgError::decode(e.to_string());
    let kind: String = row.try_get(0).map_err(bad)?;
    let owner_entity: String = row.try_get(2).map_err(bad)?;
    let owner_id: uuid::Uuid = row.try_get(3).map_err(bad)?;
    let from_entity: String = row.try_get(4).map_err(bad)?;
    let from_id: uuid::Uuid = row.try_get(5).map_err(bad)?;
    let to_entity: String = row.try_get(6).map_err(bad)?;
    let to_id: uuid::Uuid = row.try_get(7).map_err(bad)?;
    Ok(DriftEntry {
        kind: DriftKind::parse(&kind)?,
        edge_type: row.try_get(1).map_err(bad)?,
        owner: NodeRef {
            entity: entity_of(&owner_entity)?,
            id: record_id_of(owner_id)?,
        },
        from: NodeRef {
            entity: entity_of(&from_entity)?,
            id: record_id_of(from_id)?,
        },
        to: NodeRef {
            entity: entity_of(&to_entity)?,
            id: record_id_of(to_id)?,
        },
        expected: row.try_get(8).map_err(bad)?,
        found: row.try_get(9).map_err(bad)?,
    })
}
