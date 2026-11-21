//! The golden state: what the rebuilt database must hold, per position, as committed bytes.
//!
//! The generator writes it, the rebuild test compares against it, and nothing else may change it.
//! Its form is fixed and canonical:
//!
//! ```text
//! {
//!   "format": "spot-journal-compat-golden-v1",
//!   "target_schema": 3,
//!   "final": { "position": 13, "records": [ <record>, ... ] },
//!   "prefixes": [ { "position": 1, "records": [ <record>, ... ] }, ... ]
//! }
//!
//! <record> = {"body":{...},"entity":"...","id":"...","schema":3,"version":4}
//! ```
//!
//! * One record per line, keys sorted, no floats ([`crate::json::canonical`]): a diff of this
//!   file is a diff of the expectation, and a decimal amount is preserved exactly as the string
//!   the image holds.
//! * Records are ordered by `(entity, id)`; the parser refuses another order, so two states
//!   compare equal exactly when they hold the same records.
//! * `prefixes` holds one checkpoint per position of the journal — the state after that position
//!   — so a point-in-time rebuild to any prefix has an expectation to compare with.

use serde_json::{Map, Value, json};
use spot_image_upcast::Image;

use crate::error::CompatError;
use crate::json::{canonical, object, text, unsigned};

/// The format tag the file carries.
pub const GOLDEN_FORMAT: &str = "spot-journal-compat-golden-v1";

/// One document as the rebuilt database must hold it.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// The version.
    pub version: u64,
    /// The catalog version the body is in: the target schema.
    pub schema: u64,
    /// The document body, in canonical form.
    pub body: Map<String, Value>,
}

impl Record {
    /// The record a sink stored.
    #[must_use]
    pub fn of_sink(entity: &str, id: &str, version: u64, image: &Image) -> Self {
        Self {
            entity: entity.to_owned(),
            id: id.to_owned(),
            version,
            schema: image.entity_schema(),
            body: image.body().clone(),
        }
    }

    /// The record's JSON form.
    fn to_json(&self) -> Value {
        json!({
            "body": Value::Object(self.body.clone()),
            "entity": self.entity,
            "id": self.id,
            "schema": self.schema,
            "version": self.version,
        })
    }

    /// The record's one-line, canonical text.
    ///
    /// # Errors
    /// [`CompatError`] when the body holds a float or cannot be encoded.
    pub fn line(&self) -> Result<String, CompatError> {
        canonical(&self.to_json())
    }

    /// The record a JSON value describes.
    fn parse(value: &Value) -> Result<Self, CompatError> {
        let fields = object(value, "a golden record")?;
        let field = |name: &str| {
            fields
                .get(name)
                .ok_or_else(|| CompatError::new(format!("a golden record has no {name}")))
        };
        Ok(Self {
            entity: text(field("entity")?, "entity")?.to_owned(),
            id: text(field("id")?, "id")?.to_owned(),
            version: unsigned(field("version")?, "version")?,
            schema: unsigned(field("schema")?, "schema")?,
            body: object(field("body")?, "body")?.clone(),
        })
    }
}

/// The state after one position.
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// The journal position the state is as of.
    pub position: u64,
    /// Every live record, ordered by `(entity, id)`.
    pub records: Vec<Record>,
}

impl Checkpoint {
    /// A checkpoint with its records in canonical order.
    #[must_use]
    pub fn new(position: u64, mut records: Vec<Record>) -> Self {
        records.sort_by(|a, b| (&a.entity, &a.id).cmp(&(&b.entity, &b.id)));
        Self { position, records }
    }

    /// The checkpoint's JSON form: the position and its records.
    fn to_json(&self) -> Value {
        json!({
            "position": self.position,
            "records": self.records.iter().map(Record::to_json).collect::<Vec<Value>>(),
        })
    }

    /// The checkpoint a JSON value describes.
    fn parse(value: &Value) -> Result<Self, CompatError> {
        let fields = object(value, "a golden checkpoint")?;
        let position = unsigned(
            fields
                .get("position")
                .ok_or_else(|| CompatError::new("a golden checkpoint has no position"))?,
            "position",
        )?;
        let records = fields
            .get("records")
            .and_then(Value::as_array)
            .ok_or_else(|| CompatError::new("a golden checkpoint has no records"))?
            .iter()
            .map(Record::parse)
            .collect::<Result<Vec<Record>, CompatError>>()?;
        let checkpoint = Self::new(position, records);
        checkpoint.check_order()?;
        Ok(checkpoint)
    }

    /// Refuses records that are not strictly ascending by `(entity, id)`, which is what makes two
    /// checkpoints comparable by equality alone.
    fn check_order(&self) -> Result<(), CompatError> {
        for pair in self.records.windows(2) {
            if (&pair[0].entity, &pair[0].id) >= (&pair[1].entity, &pair[1].id) {
                return Err(CompatError::new(
                    "a golden checkpoint's records are not ordered by (entity, id)",
                ));
            }
        }
        Ok(())
    }
}

/// The whole golden state of one fixture.
#[derive(Clone, Debug, PartialEq)]
pub struct Golden {
    /// The schema every rebuilt image must be in.
    pub target_schema: u64,
    /// The state at the journal's end.
    pub last: Checkpoint,
    /// One checkpoint per position, from the first to the last.
    pub prefixes: Vec<Checkpoint>,
}

impl Golden {
    /// A golden state; the checkpoints are ordered by position.
    #[must_use]
    pub fn new(target_schema: u64, last: Checkpoint, prefixes: Vec<Checkpoint>) -> Self {
        Self {
            target_schema,
            last,
            prefixes,
        }
    }

    /// The checkpoint of `position`, when the fixture holds one.
    #[must_use]
    pub fn checkpoint(&self, position: u64) -> Option<&Checkpoint> {
        self.prefixes
            .iter()
            .find(|checkpoint| checkpoint.position == position)
    }

    /// The file's text: the fixed form the committed bytes have.
    ///
    /// # Errors
    /// [`CompatError`] when a body holds a float or cannot be encoded.
    pub fn to_text(&self) -> Result<String, CompatError> {
        let last = self.last.to_json();
        let prefixes: Vec<Value> = self.prefixes.iter().map(Checkpoint::to_json).collect();
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!("  \"format\": \"{GOLDEN_FORMAT}\",\n"));
        out.push_str(&format!("  \"target_schema\": {},\n", self.target_schema));
        out.push_str(&format!("  \"final\": {},\n", canonical(&last)?));
        out.push_str("  \"prefixes\": [\n");
        for (index, prefix) in prefixes.iter().enumerate() {
            let comma = if index + 1 == prefixes.len() { "" } else { "," };
            out.push_str(&format!("    {}{comma}\n", canonical(prefix)?));
        }
        out.push_str("  ]\n}\n");
        Ok(out)
    }

    /// Parses the file's text.
    ///
    /// # Errors
    /// [`CompatError`] when the text is not the golden form, names another format, or its
    /// checkpoints are not one per position in order.
    pub fn parse(source: &str) -> Result<Self, CompatError> {
        let value: Value = serde_json::from_str(source)
            .map_err(|err| CompatError::new(format!("golden-state.json does not parse: {err}")))?;
        let fields = object(&value, "golden-state.json")?;
        let format = fields
            .get("format")
            .ok_or_else(|| CompatError::new("golden-state.json has no format tag"))?;
        if text(format, "format")? != GOLDEN_FORMAT {
            return Err(CompatError::new(
                "golden-state.json names another format than this harness writes",
            ));
        }
        let target_schema = unsigned(
            fields
                .get("target_schema")
                .ok_or_else(|| CompatError::new("golden-state.json has no target_schema"))?,
            "target_schema",
        )?;
        let last = Checkpoint::parse(
            fields
                .get("final")
                .ok_or_else(|| CompatError::new("golden-state.json has no final checkpoint"))?,
        )?;
        let prefixes = fields
            .get("prefixes")
            .and_then(Value::as_array)
            .ok_or_else(|| CompatError::new("golden-state.json has no prefixes"))?
            .iter()
            .map(Checkpoint::parse)
            .collect::<Result<Vec<Checkpoint>, CompatError>>()?;
        if prefixes.is_empty() || prefixes.last().map(|c| c.position) != Some(last.position) {
            return Err(CompatError::new(
                "golden-state.json's prefixes do not end at its final position",
            ));
        }
        for (index, checkpoint) in prefixes.iter().enumerate() {
            let expected = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            if checkpoint.position != expected {
                return Err(CompatError::new(
                    "golden-state.json's prefixes are not one per position from the first",
                ));
            }
        }
        Ok(Self {
            target_schema,
            last,
            prefixes,
        })
    }
}
