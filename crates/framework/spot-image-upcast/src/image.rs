//! The document image: the whole document as it was stored, with its entity and the catalog
//! version it was written under (ADR-061 s3).
//!
//! The body is a JSON object in the native value encoding (ADR-055): every declared field of
//! the document, and every child row inside its parent's body, under the child table's name as
//! an array of objects.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::UpcastError;

/// A document body: every declared field and every child row, keyed by field or child name.
pub type Body = Map<String, Value>;

/// One stored document image.
///
/// The wire form is `{"entity": "...", "entity_schema": 3, "body": {...}}`
/// ([`Image::from_json`]). In a journal event the three parts travel separately — the entity in
/// the envelope's subject, `entity_schema` and `image` in the `doc_state` payload (ADR-061 s3) —
/// so [`Image::from_parts`] builds one from those parts without inventing a nested document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// The entity (DocType) the document belongs to.
    entity: String,
    /// The catalog version the image was written under.
    entity_schema: u64,
    /// The document body.
    body: Body,
}

impl Image {
    /// An image of `entity` written under `entity_schema`.
    #[must_use]
    pub fn new(entity: impl Into<String>, entity_schema: u64, body: Body) -> Self {
        Self {
            entity: entity.into(),
            entity_schema,
            body,
        }
    }

    /// An image from an event payload's parts: the subject's entity, the payload's
    /// `entity_schema` and its `image` value, which must be a JSON object.
    pub fn from_parts(
        entity: impl Into<String>,
        entity_schema: u64,
        image: Value,
    ) -> Result<Self, UpcastError> {
        match image {
            Value::Object(body) => Ok(Self {
                entity: entity.into(),
                entity_schema,
                body,
            }),
            _ => Err(UpcastError::ImageInvalid(
                "the body is not a JSON object".to_owned(),
            )),
        }
    }

    /// An image from its wire form.
    pub fn from_json(value: &Value) -> Result<Self, UpcastError> {
        serde_json::from_value(value.clone())
            .map_err(|err| UpcastError::ImageInvalid(err.to_string()))
    }

    /// The image's wire form.
    pub fn to_json(&self) -> Result<Value, UpcastError> {
        serde_json::to_value(self).map_err(|err| UpcastError::ImageInvalid(err.to_string()))
    }

    /// The entity the document belongs to.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The catalog version the image was written under.
    #[must_use]
    pub fn entity_schema(&self) -> u64 {
        self.entity_schema
    }

    /// The whole body.
    #[must_use]
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// One declared field's value, when the image carries it.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.body.get(name)
    }

    /// One child table's rows, when the image carries it as an array.
    ///
    /// `None` when the child table is absent, or when the value stored under that name is not an
    /// array (a shape the table's own schema forbids).
    #[must_use]
    pub fn rows(&self, child: &str) -> Option<&[Value]> {
        match self.body.get(child) {
            Some(Value::Array(rows)) => Some(rows.as_slice()),
            _ => None,
        }
    }

    /// The body, mutably. Steps rewrite the body through this; nothing outside the crate does.
    pub(crate) fn body_mut(&mut self) -> &mut Body {
        &mut self.body
    }

    /// Records that the image is now at `schema`. Only the walk does this.
    pub(crate) fn set_schema(&mut self, schema: u64) {
        self.entity_schema = schema;
    }
}
