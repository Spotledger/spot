//! Which sealed values are under which key version (read-only). Counts only, never a value.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::prelude::{Name, RecordId, Value};
use spot_meta::{EffectiveMeta, FieldType};
use spot_postgres::PgBackend;
use spot_secrets::Sealed;
use spot_storage_contract::{
    Catalog, CmpOp, DocumentStore, Expr, Isolation, Nulls, Order, OrderDir, Query, QueryExecutor,
    QueryPath, RowLock, Selection, StorageBackend, Transaction, TxOptions,
};

use crate::{Error, Result};

/// The sealed values of one secret field, counted by the key that wrapped them. Counts only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldVersions {
    /// The DocType.
    pub doctype: String,
    /// The field.
    pub field: String,
    /// Rows by key (id and version).
    pub by_key: BTreeMap<(String, u32), u64>,
    /// Stored values that are not in the sealed format.
    pub malformed: u64,
}

pub(super) fn storage(_: spot_storage_contract::StorageError) -> Error {
    Error("the database refused the read (storage error)".into())
}

pub(super) fn name_of(text: &str) -> Result<Name> {
    Name::new(text).map_err(|_| Error(format!("`{text}` is not a valid name")))
}

/// The rows' sealed columns counted by key version, for every top-level DocType with a `secret`
/// field. Read-only. `newest` limits each field to its newest rows (the start-up check); `None`
/// reads every row.
///
/// # Errors
///
/// A storage error (the text carries no row data).
pub async fn scan(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    meta: &EffectiveMeta,
    newest: Option<u32>,
) -> Result<Vec<FieldVersions>> {
    const PAGE: u32 = 200;
    let mut out = Vec::new();
    for def in meta.definitions() {
        if def.is_child {
            continue;
        }
        let fields: Vec<&str> = def
            .fields
            .iter()
            .filter(|f| f.ty == FieldType::Secret)
            .map(|f| f.name.as_str())
            .collect();
        if fields.is_empty() {
            continue;
        }
        let entity = name_of(&def.name)?;
        let alias = name_of("d")?;
        let id_col = name_of("id")?;
        let columns = fields
            .iter()
            .map(|f| name_of(&spot_meta::sealed_column(f)))
            .collect::<Result<Vec<_>>>()?;
        let mut counts: Vec<FieldVersions> = fields
            .iter()
            .map(|f| FieldVersions {
                doctype: def.name.clone(),
                field: (*f).to_owned(),
                by_key: BTreeMap::new(),
                malformed: 0,
            })
            .collect();
        let mut after: Option<RecordId> = None;
        loop {
            let limit = newest.unwrap_or(PAGE);
            let label = name_of("secrets_scan")?;
            let mut tx = backend
                .begin(
                    catalog.clone(),
                    TxOptions::new(label, Isolation::ReadCommitted).read_only(),
                )
                .await
                .map_err(storage)?;
            let mut query = Query::from(&entity, &alias, QueryPath::Transactional);
            query.select = vec![Selection {
                expr: Expr::field(&alias, &id_col),
                alias: id_col.clone(),
            }];
            if let Some(after) = after {
                query.filter =
                    Some(Expr::field(&alias, &id_col).cmp(CmpOp::Gt, Expr::value(after)));
            }
            query.order_by = vec![Order {
                expr: Expr::field(&alias, &id_col),
                dir: if newest.is_some() {
                    OrderDir::Desc
                } else {
                    OrderDir::Asc
                },
                nulls: Nulls::Last,
            }];
            query.limit = Some(limit);
            let rows = match tx.query(&query).await {
                Ok(rows) => rows.rows,
                Err(e) => {
                    let _ = tx.rollback().await;
                    return Err(storage(e));
                }
            };
            let ids: Vec<RecordId> = rows
                .into_iter()
                .filter_map(|row| match row.first() {
                    Some(Value::Id(id)) => Some(*id),
                    _ => None,
                })
                .collect();
            for id in &ids {
                let stored = match tx.load(&entity, *id, RowLock::None).await {
                    Ok(stored) => stored,
                    Err(e) => {
                        let _ = tx.rollback().await;
                        return Err(storage(e));
                    }
                };
                let Some(stored) = stored else { continue };
                for (count, column) in counts.iter_mut().zip(&columns) {
                    if let Some(Value::Bytes(bytes)) = stored.data.fields.get(column) {
                        match Sealed::from_bytes(bytes) {
                            Ok(sealed) => {
                                let key = sealed.key_ref();
                                *count
                                    .by_key
                                    .entry((key.id.clone(), key.version))
                                    .or_insert(0) += 1;
                            }
                            Err(_) => count.malformed += 1,
                        }
                    }
                }
            }
            let _ = tx.rollback().await;
            if newest.is_some() || ids.len() < usize::try_from(PAGE).unwrap_or(usize::MAX) {
                break;
            }
            after = ids.last().copied();
        }
        out.extend(counts);
    }
    Ok(out)
}

/// Rows under a key version other than `current`, over every field of `found`.
pub fn rows_on_old_versions(found: &[FieldVersions], current: u32) -> u64 {
    found
        .iter()
        .flat_map(|f| f.by_key.iter())
        .filter(|((_, version), _)| *version != current)
        .map(|(_, n)| *n)
        .sum()
}
