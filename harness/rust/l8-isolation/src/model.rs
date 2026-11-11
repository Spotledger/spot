//! The test model the isolation workloads run on.
//!
//! One document entity, `l8_item`, is one logical **key**: a JSON `body` field (the list of
//! append workload) and an integer `reg` field (the register of the rw-register workload). Each
//! key row is created once in setup and only ever updated by the workload, so every workload
//! write carries the version it was read at and a correct backend hands out exactly one winner
//! per version — the raw material a lost-update check needs.
//!
//! The entity is deliberately tiny: an isolation checker cares about *which* version of a key
//! a transaction observed, not about the key's business meaning.

use std::sync::Arc;

use spot_storage_contract::{
    Catalog, Collation, DocumentData, EntityDef, EntityKind, FieldDef, FieldType, Fields, IndexDef,
    Name, RecordId, StorageResult, Value,
};

/// The catalog and the field names the workload and the checker use.
#[derive(Clone, Debug)]
pub struct Model {
    /// The catalog every transaction of a run uses.
    pub catalog: Arc<Catalog>,
    /// Entity `l8_item`.
    pub item: Name,
    /// Field `key`: the logical key, unique.
    pub key: Name,
    /// Field `body`: the JSON list (list-append workload).
    pub body: Name,
    /// Field `reg`: the integer register (rw-register workload).
    pub reg: Name,
}

impl Default for Model {
    fn default() -> Self {
        Model::new().unwrap_or_else(|e| panic!("the L8 model must build: {e}"))
    }
}

impl Model {
    /// Builds the model.
    pub fn new() -> StorageResult<Model> {
        let n = Name::new;
        let field = |name: &str, ty: FieldType| FieldDef {
            name: n(name).unwrap_or_else(|e| panic!("{e}")),
            ty,
            nullable: false,
        };
        let index = |name: &str, fields: &[&str]| -> StorageResult<IndexDef> {
            Ok(IndexDef {
                name: n(name)?,
                fields: fields
                    .iter()
                    .map(|f| n(f))
                    .collect::<StorageResult<Vec<_>>>()?,
                unique: true,
                method: spot_storage_contract::IndexMethod::BTree,
                opclass: None,
                include: vec![],
            })
        };
        let entity = EntityDef {
            name: n("l8_item")?,
            kind: EntityKind::Document,
            fields: vec![
                field(
                    "key",
                    FieldType::Text {
                        max_chars: Some(80),
                    },
                ),
                field("body", FieldType::Json),
                field("reg", FieldType::Int),
            ],
            children: vec![],
            indexes: vec![index("l8_item_key", &["key"])?],
            exclusions: vec![],
            search: None,
        };
        let catalog = Catalog::new(1, Collation::new("en")?, vec![entity])?;
        Ok(Model {
            catalog: Arc::new(catalog),
            item: n("l8_item")?,
            key: n("key")?,
            body: n("body")?,
            reg: n("reg")?,
        })
    }

    /// A fresh key document: an empty list body and register 0.
    pub fn empty_item(&self, id: RecordId, key: &str) -> DocumentData {
        let mut fields = Fields::new();
        fields.insert(self.key.clone(), Value::Text(key.to_owned()));
        fields.insert(self.body.clone(), Value::Json("[]".to_owned()));
        fields.insert(self.reg.clone(), Value::Int(0));
        DocumentData {
            entity: self.item.clone(),
            id,
            fields,
            children: std::collections::BTreeMap::new(),
        }
    }
}
