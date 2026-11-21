//! Loads an app's `seed.toml` through the document engine, as the system principal.
//!
//! The seed's row ids are placeholders. Each becomes a record id derived from the app name and
//! the placeholder, so a second run finds the rows already there and writes nothing. A
//! placeholder naming a user that does not exist (`user_ama`) resolves to the Administrator.
use crate::config::{Loaded, Site};
use crate::{Error, Result, error};
use spot_app_api::context::DocumentService;
use spot_app_api::{Actor, ChildRow, Document, Invocation};
use spot_meta::{Definition, EffectiveMeta, FieldType};
use spot_storage_contract::{Name, RecordId, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

/// How many rows a seed run wrote and skipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Rows created now.
    pub created: usize,
    /// Rows that were already there.
    pub present: usize,
    /// Seed values skipped because the DocType has no such field.
    pub skipped: Vec<String>,
}

fn name(text: &str) -> Result<Name> {
    Name::new(text).map_err(error)
}

/// A UUIDv7-shaped id derived from `(app, placeholder)`; stable across runs and platforms.
fn derived(app: &str, placeholder: &str) -> Result<RecordId> {
    let mut bytes = [0u8; 16];
    for (half, seed) in [0xcbf2_9ce4_8422_2325_u64, 0x8422_2325_cbf2_9ce4]
        .iter()
        .enumerate()
    {
        let mut hash = *seed;
        for byte in format!("spot-seed:{app}:{placeholder}").bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        bytes[half * 8..half * 8 + 8].copy_from_slice(&hash.to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes).map_err(error)
}

/// `time_entries` is the DocType `time_entry`; `labels` is `label`.
fn singular(table: &str) -> String {
    table.strip_suffix("ies").map_or_else(
        || table.strip_suffix('s').unwrap_or(table).to_owned(),
        |stem| format!("{stem}y"),
    )
}

struct Seeder<'a> {
    skipped: std::cell::RefCell<Vec<String>>,
    meta: &'a EffectiveMeta,
    ids: BTreeMap<String, RecordId>,
    administrator: RecordId,
}

impl Seeder<'_> {
    fn reference(&self, text: &str) -> Result<RecordId> {
        if let Some(id) = self.ids.get(text) {
            return Ok(*id);
        }
        if text.starts_with("user_") {
            return Ok(self.administrator);
        }
        Err(Error(format!("seed reference `{text}` resolves to no row")))
    }

    fn value(&self, spec: &spot_meta::FieldSpec, raw: &toml::Value) -> Result<Value> {
        let wrong = || Error(format!("seed field `{}` has the wrong type", spec.name));
        Ok(match spec.ty {
            FieldType::Bool => Value::Bool(raw.as_bool().ok_or_else(wrong)?),
            FieldType::Int => Value::Int(raw.as_integer().ok_or_else(wrong)?),
            FieldType::Decimal => {
                let text = match raw {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    _ => return Err(wrong()),
                };
                Value::Decimal(rust_decimal::Decimal::from_str(&text).map_err(|_| wrong())?)
            }
            FieldType::Date => Value::Date(
                time::Date::parse(
                    raw.as_str().ok_or_else(wrong)?,
                    time::macros::format_description!("[year]-[month]-[day]"),
                )
                .map_err(|_| wrong())?,
            ),
            FieldType::DateTime => Value::DateTime(
                time::OffsetDateTime::parse(
                    raw.as_str().ok_or_else(wrong)?,
                    &time::format_description::well_known::Rfc3339,
                )
                .map_err(|_| wrong())?,
            ),
            FieldType::Link | FieldType::DynamicLink => {
                Value::Id(self.reference(raw.as_str().ok_or_else(wrong)?)?)
            }
            FieldType::Text | FieldType::LongText | FieldType::Select => {
                Value::Text(raw.as_str().ok_or_else(wrong)?.to_owned())
            }
            _ => {
                return Err(Error(format!(
                    "seed field `{}` has a type the seed loader does not fill",
                    spec.name
                )));
            }
        })
    }

    fn child(&self, definition: &Definition, raw: &toml::Table) -> Result<ChildRow> {
        let mut fields = BTreeMap::new();
        for (key, value) in raw {
            let spec = definition
                .fields
                .iter()
                .find(|f| &f.name == key)
                .ok_or_else(|| Error(format!("`{}` has no field `{key}`", definition.name)))?;
            fields.insert(name(key)?, self.value(spec, value)?);
        }
        Ok(ChildRow {
            id: RecordId::new_v7(),
            fields,
        })
    }

    fn document(&self, doctype: &str, row: &toml::Table) -> Result<Document> {
        let definition = self
            .meta
            .definition(doctype)
            .ok_or_else(|| Error(format!("the seed names unknown DocType `{doctype}`")))?;
        let placeholder = row
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| Error(format!("a `{doctype}` seed row has no id")))?;
        let mut doc = Document::new(name(doctype)?, self.ids[placeholder]);
        for (key, raw) in row {
            if key == "id" {
                continue;
            }
            let Some(spec) = definition.fields.iter().find(|f| &f.name == key) else {
                self.skipped.borrow_mut().push(format!("{doctype}.{key}"));
                continue;
            };
            let target = spec
                .options
                .as_deref()
                .and_then(|o| self.meta.definition(o));
            match (spec.ty, target) {
                (FieldType::Table, Some(child)) => {
                    let rows = raw
                        .as_array()
                        .ok_or_else(|| Error(format!("seed field `{key}` must be a list")))?;
                    for item in rows {
                        let table = item
                            .as_table()
                            .ok_or_else(|| Error(format!("seed field `{key}` must hold tables")))?;
                        doc.rows_mut(name(key)?).push(self.child(child, table)?);
                    }
                }
                (FieldType::TableMultiselect, Some(child)) => {
                    let pick = child
                        .fields
                        .iter()
                        .find(|f| f.ty == FieldType::Link)
                        .ok_or_else(|| Error(format!("`{}` has no link field", child.name)))?;
                    let items = raw
                        .as_array()
                        .ok_or_else(|| Error(format!("seed field `{key}` must be a list")))?;
                    for item in items {
                        let mut fields = BTreeMap::new();
                        fields.insert(name(&pick.name)?, self.value(pick, item)?);
                        doc.rows_mut(name(key)?).push(ChildRow {
                            id: RecordId::new_v7(),
                            fields,
                        });
                    }
                }
                _ => doc.set(name(key)?, self.value(spec, raw)?),
            }
        }
        Ok(doc)
    }
}

/// DocTypes ordered so that a DocType comes after the seeded DocTypes it links to.
fn order(meta: &EffectiveMeta, tables: &BTreeMap<String, Vec<toml::Table>>) -> Vec<String> {
    let mut wanted: BTreeSet<String> = tables.keys().cloned().collect();
    let mut ordered = Vec::new();
    while !wanted.is_empty() {
        let ready: Vec<String> = wanted
            .iter()
            .filter(|t| {
                meta.definition(t).is_none_or(|d| {
                    d.fields.iter().all(|f| {
                        f.options.as_ref().is_none_or(|o| {
                            o == *t
                                || !matches!(f.ty, FieldType::Link | FieldType::TableMultiselect)
                                || !wanted.contains(o)
                                || meta.definition(o).is_some_and(|c| c.is_child)
                        })
                    })
                })
            })
            .cloned()
            .collect();
        let step = if ready.is_empty() {
            wanted.iter().cloned().collect()
        } else {
            ready
        };
        for t in step {
            wanted.remove(&t);
            ordered.push(t);
        }
    }
    ordered
}

/// Creates the seed rows of `app` on `site`. Rows already present are left untouched.
pub async fn run(
    loaded: &Loaded,
    site: &Site,
    meta: std::sync::Arc<EffectiveMeta>,
    app: &str,
    seed_text: &str,
    administrator: RecordId,
) -> Result<Report> {
    let file: toml::Table =
        toml::from_str(seed_text).map_err(|e| Error(format!("seed.toml: {}", e.message())))?;
    let mut tables: BTreeMap<String, Vec<toml::Table>> = BTreeMap::new();
    for (key, value) in &file {
        let rows = value
            .as_array()
            .ok_or_else(|| Error(format!("seed.toml: `{key}` must be a list of rows")))?
            .iter()
            .map(|r| r.as_table().cloned())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Error(format!("seed.toml: `{key}` must hold tables")))?;
        tables.insert(singular(key), rows);
    }
    let mut seeder = Seeder {
        skipped: std::cell::RefCell::new(Vec::new()),
        meta: &meta,
        ids: BTreeMap::new(),
        administrator,
    };
    for rows in tables.values() {
        for row in rows {
            let placeholder = row
                .get("id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| Error("a seed row has no id".into()))?;
            if seeder
                .ids
                .insert(placeholder.to_owned(), derived(app, placeholder)?)
                .is_some()
            {
                return Err(Error(format!("seed id `{placeholder}` appears twice")));
            }
        }
    }
    let backend = crate::schema::backend(loaded, site).await?;
    let engine = spot_document::engine::Engine::new(
        backend,
        meta.clone(),
        std::sync::Arc::new(crate::platform_registry()?),
        std::sync::Arc::new(spot_core::SystemClock::new()),
        std::sync::Arc::new(spot_document::engine::V7Ids),
    )
    .map_err(error)?;
    let invocation = Invocation {
        site: spot_core::SiteId::new(&site.site).map_err(error)?,
        actor: Actor::System {
            component: name("spot_cli")?,
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::now_utc(),
    };
    let mut report = Report::default();
    for doctype in order(&meta, &tables) {
        for row in &tables[&doctype] {
            let doc = seeder.document(&doctype, row)?;
            if engine
                .peek(&doctype, doc.id())
                .await
                .map_err(error)?
                .is_some()
            {
                report.present += 1;
                continue;
            }
            engine
                .create(&invocation, doc)
                .await
                .map_err(|e| Error(format!("seeding `{doctype}` failed: {}", e.message)))?;
            report.created += 1;
        }
    }
    let mut skipped = seeder.skipped.into_inner();
    skipped.sort();
    skipped.dedup();
    report.skipped = skipped;
    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn table_names_map_to_doctype_names() {
        assert_eq!(singular("time_entries"), "time_entry");
        assert_eq!(singular("labels"), "label");
        assert_eq!(singular("projects"), "project");
    }

    #[test]
    fn seed_ids_are_stable_distinct_and_uuid_v7_shaped() {
        let a = derived("sample", "prj_platform").unwrap();
        assert_eq!(a, derived("sample", "prj_platform").unwrap());
        assert_ne!(a, derived("sample", "prj_rollout").unwrap());
        assert_ne!(a, derived("other", "prj_platform").unwrap());
        assert_eq!(a.as_bytes()[6] >> 4, 7);
        assert_eq!(a.as_bytes()[8] >> 6, 0b10);
    }
}
