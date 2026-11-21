//! The declared fingerprint queries and the engine that runs them.
//!
//! The platform owns the *engine*, not the *contents*: a [`FingerprintSpec`] declares one
//! [`FingerprintQuery`] per section, and a [`FingerprintSource`] runs each declared query against
//! the clone. Foundation and ERP register the real queries later; the engine carries no ERP
//! DocType or account names.
//!
//! A query is an ordinary storage-contract [`Query`] (ADR-45 — no hand-built SQL) whose
//! `select` lists the declared dimension columns and the integer measure columns. The engine
//! reads the returned [`Rows`] by column alias, so a section is described entirely by its
//! declaration.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_storage_contract::{
    Access, Catalog, Isolation, Name, Query, QueryExecutor, Rows, StorageBackend, Transaction,
    TxOptions, Value,
};

use crate::BoxFuture;
use crate::clone::CloneTarget;
use crate::error::{Error, Result};
use crate::fingerprint::{Dimension, Fingerprint, FingerprintRow, FingerprintSection};

/// How an ageing section is bucketed.
///
/// The declared query returns one row per (entity, raw age); the engine replaces the raw age with
/// a bucket label and regroups. Buckets are half-open on the low side and closed on the high side:
/// `bounds = [30, 60, 90]` and `labels = ["0-30", "31-60", "61-90", "90+"]` puts an age of 30 in
/// `0-30`, 31 in `31-60`, and 91 in `90+`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketSpec {
    /// The declared dimension column that carries the raw integer age.
    pub age_column: String,
    /// The declared dimension name the bucket label is recorded under.
    pub bucket_dimension: String,
    /// The inclusive upper bounds, ascending.
    pub bounds: Vec<i64>,
    /// The labels, one more than the bounds.
    pub labels: Vec<String>,
}

impl BucketSpec {
    /// The label for `age`.
    fn label(&self, age: i64) -> &str {
        for (index, bound) in self.bounds.iter().enumerate() {
            if age <= *bound {
                return &self.labels[index];
            }
        }
        &self.labels[self.bounds.len()]
    }
}

/// One declared fingerprint query: a stable key, the dimensions and measures it produces, an
/// optional ageing bucket, and the storage query that produces its rows.
#[derive(Clone, Debug)]
pub struct FingerprintQuery {
    /// The section key, for example `trial_balance`. Unique within a spec.
    pub key: String,
    /// A human title.
    pub title: String,
    /// The `select` aliases that are dimension columns.
    pub dimensions: Vec<String>,
    /// The `select` aliases that are integer measure columns.
    pub measures: Vec<String>,
    /// The ageing bucket, when this is an ageing section.
    pub bucket: Option<BucketSpec>,
    /// The declared query.
    pub query: Query,
}

/// A whole fingerprint spec: the sections a rehearsal measures.
#[derive(Clone, Debug, Default)]
pub struct FingerprintSpec {
    /// The declared queries, in declaration order.
    pub sections: Vec<FingerprintQuery>,
}

impl FingerprintSpec {
    /// A spec from its queries.
    pub fn new(sections: Vec<FingerprintQuery>) -> FingerprintSpec {
        FingerprintSpec { sections }
    }

    /// Checks the declarations: unique keys, a measure on every section and a well-formed bucket.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] naming what is wrong.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for section in &self.sections {
            if section.key.is_empty() {
                return Err(invalid("fingerprint spec", "a section key is empty"));
            }
            if !seen.insert(section.key.as_str()) {
                return Err(invalid(
                    "fingerprint spec",
                    &format!("section key `{}` is declared twice", section.key),
                ));
            }
            if section.measures.is_empty() {
                return Err(invalid(
                    "fingerprint spec",
                    &format!("section `{}` declares no measure", section.key),
                ));
            }
            if let Some(bucket) = &section.bucket {
                if bucket.labels.len() != bucket.bounds.len() + 1 {
                    return Err(invalid(
                        "fingerprint spec",
                        &format!(
                            "section `{}` declares {} bucket labels for {} bounds",
                            section.key,
                            bucket.labels.len(),
                            bucket.bounds.len()
                        ),
                    ));
                }
                if bucket.bounds.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(invalid(
                        "fingerprint spec",
                        &format!("section `{}` has non-ascending bucket bounds", section.key),
                    ));
                }
                if !section.dimensions.contains(&bucket.age_column) {
                    return Err(invalid(
                        "fingerprint spec",
                        &format!(
                            "section `{}` buckets column `{}`, which is not a declared dimension",
                            section.key, bucket.age_column
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Measures the whole fingerprint by running every declared query through `source`.
    ///
    /// # Errors
    /// [`Error::FingerprintFailed`] when a query fails or a returned value is not an integer, and
    /// [`Error::InvalidInput`] when a declared column is missing from the result.
    pub async fn measure(
        &self,
        source: &dyn FingerprintSource,
        clone: &CloneTarget,
    ) -> Result<Fingerprint> {
        self.validate()?;
        let mut sections = Vec::with_capacity(self.sections.len());
        for section in &self.sections {
            let rows = source.run(clone, section).await?;
            sections.push(section.reduce(rows)?);
        }
        Ok(Fingerprint::new(sections))
    }
}

impl FingerprintQuery {
    /// Reduces the query's `rows` to one fingerprint section.
    fn reduce(&self, rows: Rows) -> Result<FingerprintSection> {
        let index = |name: &str| -> Result<usize> {
            rows.columns
                .iter()
                .position(|c| c.as_str() == name)
                .ok_or_else(|| Error::FingerprintFailed {
                    section: self.key.clone(),
                    reason: format!("the query returned no column `{name}`"),
                })
        };
        let dimension_columns: Vec<(String, usize)> = self
            .dimensions
            .iter()
            .map(|name| Ok((name.clone(), index(name)?)))
            .collect::<Result<_>>()?;
        let measure_columns: Vec<(String, usize)> = self
            .measures
            .iter()
            .map(|name| Ok((name.clone(), index(name)?)))
            .collect::<Result<_>>()?;

        let mut grouped: BTreeMap<Vec<(String, String)>, BTreeMap<String, i128>> = BTreeMap::new();
        for row in &rows.rows {
            let mut dimensions = Vec::with_capacity(dimension_columns.len());
            for (name, column) in &dimension_columns {
                let value = row.get(*column).unwrap_or(&Value::Null);
                dimensions.push(Dimension::new(
                    name.clone(),
                    self.dimension_text(name, value)?,
                ));
            }
            if let Some(bucket) = &self.bucket {
                dimensions = self.apply_bucket(dimensions, &dimension_columns, row, bucket)?;
            }
            let mut measures = BTreeMap::new();
            for (name, column) in &measure_columns {
                let value = row.get(*column).unwrap_or(&Value::Null);
                measures.insert(name.clone(), measure(name, value, &self.key)?);
            }
            let key: Vec<(String, String)> = dimensions
                .iter()
                .map(|d| (d.name.clone(), d.value.clone()))
                .collect();
            let entry = grouped.entry(key).or_default();
            for (name, value) in measures {
                *entry.entry(name).or_insert(0) += value;
            }
        }

        let out = grouped
            .into_iter()
            .map(|(key, measures)| {
                FingerprintRow::new(
                    key.into_iter()
                        .map(|(name, value)| Dimension::new(name, value))
                        .collect(),
                    measures,
                )
            })
            .collect();
        let dimensions = match &self.bucket {
            Some(bucket) => self
                .dimensions
                .iter()
                .map(|name| {
                    if name == &bucket.age_column {
                        bucket.bucket_dimension.clone()
                    } else {
                        name.clone()
                    }
                })
                .collect(),
            None => self.dimensions.clone(),
        };
        Ok(FingerprintSection::new(
            self.key.clone(),
            self.title.clone(),
            dimensions,
            self.measures.clone(),
            out,
        ))
    }

    /// The text of a dimension value.
    fn dimension_text(&self, name: &str, value: &Value) -> Result<String> {
        match value {
            Value::Null => Ok(String::new()),
            Value::Text(text) => Ok(text.clone()),
            Value::Int(number) => Ok(number.to_string()),
            Value::Bool(flag) => Ok(flag.to_string()),
            Value::Id(id) => Ok(id.to_string()),
            Value::Date(date) => Ok(date.to_string()),
            other => Err(Error::FingerprintFailed {
                section: self.key.clone(),
                reason: format!(
                    "dimension `{name}` is not a text, id, date or integer value: {other:?}"
                ),
            }),
        }
    }

    /// Replaces the raw age dimension with its bucket label.
    fn apply_bucket(
        &self,
        dimensions: Vec<Dimension>,
        columns: &[(String, usize)],
        row: &[Value],
        bucket: &BucketSpec,
    ) -> Result<Vec<Dimension>> {
        let age_column = columns
            .iter()
            .find(|(name, _)| name == &bucket.age_column)
            .map(|(_, column)| *column)
            .ok_or_else(|| Error::FingerprintFailed {
                section: self.key.clone(),
                reason: format!(
                    "bucket age column `{}` is not a declared dimension",
                    bucket.age_column
                ),
            })?;
        let raw = row.get(age_column).unwrap_or(&Value::Null);
        let age = match raw {
            Value::Int(number) => *number,
            other => {
                return Err(Error::FingerprintFailed {
                    section: self.key.clone(),
                    reason: format!(
                        "bucket age column `{}` is not an integer: {other:?}",
                        bucket.age_column
                    ),
                });
            }
        };
        let label = bucket.label(age).to_owned();
        let mut out: Vec<Dimension> = dimensions
            .into_iter()
            .filter(|d| d.name != bucket.age_column)
            .collect();
        out.push(Dimension::new(bucket.bucket_dimension.clone(), label));
        out.sort();
        Ok(out)
    }
}

/// The integer value of a measure column.
fn measure(name: &str, value: &Value, section: &str) -> Result<i128> {
    match value {
        Value::Null => Ok(0),
        Value::Int(number) => Ok(i128::from(*number)),
        Value::Decimal(decimal) if decimal.fract().is_zero() => decimal
            .to_string()
            .parse::<i128>()
            .map_err(|_| Error::FingerprintFailed {
                section: section.to_owned(),
                reason: format!("measure `{name}` overflows an integer"),
            }),
        other => Err(Error::FingerprintFailed {
            section: section.to_owned(),
            reason: format!("measure `{name}` is not an integer: {other:?}"),
        }),
    }
}

/// Runs one declared fingerprint query against the clone and returns its rows.
///
/// The source is what makes the engine independent of the backend: tests use an in-memory fake,
/// production wires a backend for the clone's scratch database. The reason in an error must name
/// no secret.
pub trait FingerprintSource: Send + Sync {
    /// Runs `query` against `clone`.
    fn run<'a>(
        &'a self,
        clone: &'a CloneTarget,
        query: &'a FingerprintQuery,
    ) -> BoxFuture<'a, Result<Rows>>;
}

/// A [`FingerprintSource`] that runs the declared queries through a storage backend's typed query
/// IR, in a read-only repeatable-read transaction so a measurement never races a writer.
///
/// The backend is the *clone's* backend: in tests the in-memory fake, in production one connected
/// to the scratch database. The `clone` argument is ignored — one source serves the one clone the
/// rehearsal owns.
pub struct StorageFingerprintSource<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
}

impl<B: StorageBackend> StorageFingerprintSource<B> {
    /// A source over `backend` and its catalogue.
    pub fn new(backend: Arc<B>, catalog: Arc<Catalog>) -> StorageFingerprintSource<B> {
        StorageFingerprintSource { backend, catalog }
    }
}

impl<B: StorageBackend> FingerprintSource for StorageFingerprintSource<B> {
    fn run<'a>(
        &'a self,
        _clone: &'a CloneTarget,
        query: &'a FingerprintQuery,
    ) -> BoxFuture<'a, Result<Rows>> {
        Box::pin(async move {
            let name =
                Name::new("rehearsal_fingerprint").map_err(|e| Error::FingerprintFailed {
                    section: query.key.clone(),
                    reason: e.to_string(),
                })?;
            let mut options = TxOptions::new(name, Isolation::RepeatableRead);
            options.access = Access::ReadOnly;
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|e| Error::FingerprintFailed {
                    section: query.key.clone(),
                    reason: e.to_string(),
                })?;
            let rows = QueryExecutor::query(&mut tx, &query.query).await;
            let _ = tx.rollback().await;
            rows.map_err(|e| Error::FingerprintFailed {
                section: query.key.clone(),
                reason: e.to_string(),
            })
        })
    }
}

/// A short constructor for [`Error::InvalidInput`].
fn invalid(what: &str, reason: &str) -> Error {
    Error::InvalidInput {
        what: what.to_owned(),
        reason: reason.to_owned(),
    }
}
