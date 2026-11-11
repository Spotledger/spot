//! Plain-data catalog and redactor, for tests and for adapters that have no live catalog yet.

use crate::error::ClosureError;
use crate::model::RecordKey;
use crate::traits::{Redactor, References, element_id};
use serde_json::Value;
use std::collections::BTreeMap;

/// Where a link points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// A fixed target entity.
    Fixed(String),
    /// A dynamic link: the entity is read from `type_field` (a sibling in the same object) and must
    /// be one of `allowed` (the declared `allowed_targets`).
    Dynamic {
        /// The field holding the target entity name.
        type_field: String,
        /// The closed list of entities the link may point to.
        allowed: Vec<String>,
    },
}

/// One declared link of an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpec {
    /// `None` for a field of the parent, `Some(table)` for a field of every row of that child table.
    pub child_table: Option<String>,
    /// The link field.
    pub field: String,
    /// Where it points.
    pub target: LinkTarget,
}

/// A catalog held as plain data: entity name to its declared links.
#[derive(Debug, Clone, Default)]
pub struct StaticReferences {
    links: BTreeMap<String, Vec<LinkSpec>>,
}

impl StaticReferences {
    /// An empty catalog (no entity has references).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a link of `entity`.
    #[must_use]
    pub fn with_link(mut self, entity: &str, spec: LinkSpec) -> Self {
        self.links.entry(entity.to_string()).or_default().push(spec);
        self
    }

    /// Declare a fixed link in the parent.
    #[must_use]
    pub fn with_fixed(self, entity: &str, field: &str, target: &str) -> Self {
        self.with_link(
            entity,
            LinkSpec {
                child_table: None,
                field: field.into(),
                target: LinkTarget::Fixed(target.into()),
            },
        )
    }

    /// Declare a fixed link in every row of a child table.
    #[must_use]
    pub fn with_fixed_child(self, entity: &str, table: &str, field: &str, target: &str) -> Self {
        self.with_link(
            entity,
            LinkSpec {
                child_table: Some(table.into()),
                field: field.into(),
                target: LinkTarget::Fixed(target.into()),
            },
        )
    }

    /// Declare a dynamic link in the parent.
    #[must_use]
    pub fn with_dynamic(
        self,
        entity: &str,
        field: &str,
        type_field: &str,
        allowed: &[&str],
    ) -> Self {
        self.with_link(
            entity,
            LinkSpec {
                child_table: None,
                field: field.into(),
                target: LinkTarget::Dynamic {
                    type_field: type_field.into(),
                    allowed: allowed.iter().map(|s| (*s).to_string()).collect(),
                },
            },
        )
    }
}

fn read_link(
    entity: &str,
    spec: &LinkSpec,
    object: &Value,
    out: &mut Vec<RecordKey>,
) -> Result<(), ClosureError> {
    let Some(id) = object.get(&spec.field).and_then(element_id) else {
        return Ok(()); // an empty link is not a reference
    };
    let target = match &spec.target {
        LinkTarget::Fixed(t) => t.clone(),
        LinkTarget::Dynamic {
            type_field,
            allowed,
        } => {
            let unreadable = || ClosureError::ReferenceUnreadable {
                entity: entity.to_string(),
                field: spec.field.clone(),
            };
            let t = object
                .get(type_field)
                .and_then(Value::as_str)
                .ok_or_else(unreadable)?;
            if !allowed.iter().any(|a| a == t) {
                return Err(unreadable());
            }
            t.to_string()
        }
    };
    out.push(RecordKey::new(target, id));
    Ok(())
}

impl References for StaticReferences {
    fn references(&self, entity: &str, image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        let mut out = Vec::new();
        for spec in self
            .links
            .get(entity)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            match &spec.child_table {
                None => read_link(entity, spec, image, &mut out)?,
                Some(table) => {
                    for row in image
                        .get(table)
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                    {
                        read_link(entity, spec, row, &mut out)?;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// A redactor held as plain data: top-level fields to drop per entity.
#[derive(Debug, Clone, Default)]
pub struct StaticRedactor {
    down: BTreeMap<String, Vec<String>>,
}

impl StaticRedactor {
    /// A redactor that drops nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop `field` from every `entity` image going down.
    #[must_use]
    pub fn excluding(mut self, entity: &str, field: &str) -> Self {
        self.down
            .entry(entity.to_string())
            .or_default()
            .push(field.to_string());
        self
    }
}

impl Redactor for StaticRedactor {
    fn redact_down(&self, entity: &str, image: &mut Value) {
        if let (Some(fields), Some(map)) = (self.down.get(entity), image.as_object_mut()) {
            for f in fields {
                map.remove(f);
            }
        }
    }
}
