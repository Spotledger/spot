//! The remedy vocabulary: ids, metadata, parameters, checks, plans, changes and undo records.
//!
//! Everything here is plain data with no clock and no randomness. A [`RemedyId`] is dotted
//! lower-case and is the remedy's stable address; [`RemedyMeta`] carries the tier, risk, the
//! document types it may affect, its parameter schema and its precondition text; a [`Plan`] is
//! the dry-run's sealed promise of exactly what apply will do; and an [`UndoRecord`] is what an
//! apply stores so the change can be reversed inside its window.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use spot_agent_tiers::{Effect, Tier};

use crate::error::RemedyError;

/// A remedy's stable address: dotted lower-case, at least two segments (`job.retry_failed`).
///
/// Construct one with [`RemedyId::new`] for a known-good literal (the seeds do) or
/// [`RemedyId::parse`] for anything that arrives from data. [`RemedyId::is_valid`] is the rule
/// both share; the registry refuses a remedy whose id is not valid.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemedyId(String);

impl RemedyId {
    /// Wraps a known-good id. Used with literals the author has checked; the registry still
    /// validates every id on registration, so a typo is refused before a remedy can run.
    #[must_use]
    pub fn new(id: &str) -> Self {
        Self(id.to_owned())
    }

    /// Parses an id, refusing anything that is not dotted lower-case.
    ///
    /// # Errors
    ///
    /// [`RemedyError::InvalidId`] when the text is not dotted lower-case.
    pub fn parse(id: &str) -> Result<Self, RemedyError> {
        if Self::is_valid(id) {
            Ok(Self(id.to_owned()))
        } else {
            Err(RemedyError::InvalidId { id: id.to_owned() })
        }
    }

    /// Whether `id` is dotted lower-case: at least two segments, each matching
    /// `[a-z][a-z0-9_]*` (`job.retry_failed`, `quarantine.retry_all`).
    #[must_use]
    pub fn is_valid(id: &str) -> bool {
        let mut segments = id.split('.');
        let Some(first) = segments.next() else {
            return false;
        };
        if !segment_ok(first) {
            return false;
        }
        let mut count = 1;
        for segment in segments {
            if !segment_ok(segment) {
                return false;
            }
            count += 1;
        }
        count >= 2
    }

    /// The id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether one dotted segment matches `[a-z][a-z0-9_]*`.
fn segment_ok(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl fmt::Display for RemedyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A document type a remedy may affect, by name (`Job`, `Case`, `Stock Ledger Entry`).
///
/// The name is the DocType's own name, not an ERP-specific one; generic remedies leave the list
/// empty because they touch infrastructure, not documents.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocTypeRef(String);

impl DocTypeRef {
    /// A document-type reference, refusing an empty or malformed name.
    ///
    /// # Errors
    ///
    /// [`RemedyError::MetaInvalid`] when the name is empty or holds a character a DocType name
    /// cannot.
    pub fn new(name: &str) -> Result<Self, RemedyError> {
        let mut chars = name.chars();
        match chars.next() {
            Some(first) if first.is_ascii_alphabetic() => {}
            _ => {
                return Err(RemedyError::MetaInvalid {
                    remedy: String::new(),
                    detail: format!("the document type `{name}` must start with a letter"),
                });
            }
        }
        if chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ') {
            Ok(Self(name.to_owned()))
        } else {
            Err(RemedyError::MetaInvalid {
                remedy: String::new(),
                detail: format!("the document type `{name}` holds an invalid character"),
            })
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DocTypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How much a remedy can go wrong if it is wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Derived data only: a cache, an index, a lease.
    Low,
    /// Business state that can be restored from the undo record.
    Medium,
    /// State whose reversal is not guaranteed; the author must say so.
    High,
}

impl Risk {
    /// The stable snake_case text form, which is also the serde and TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a remedy's apply can be undone, and if not, why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UndoKind {
    /// Undo restores the state the apply left; the record holds what to restore.
    Reversible {
        /// What undo restores.
        detail: String,
    },
    /// Undo is not meaningful; the author says why. Such a remedy must be T3.
    Irreversible {
        /// Why the change cannot be undone.
        reason: String,
    },
}

impl UndoKind {
    /// A reversible undo with its detail.
    #[must_use]
    pub fn reversible(detail: &str) -> Self {
        UndoKind::Reversible {
            detail: detail.to_owned(),
        }
    }

    /// An irreversible undo with its reason.
    #[must_use]
    pub fn irreversible(reason: &str) -> Self {
        UndoKind::Irreversible {
            reason: reason.to_owned(),
        }
    }

    /// Whether undo is meaningful for this remedy.
    #[must_use]
    pub fn is_reversible(&self) -> bool {
        matches!(self, UndoKind::Reversible { .. })
    }
}

/// The JSON type one parameter has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// A string.
    String,
    /// A JSON integer (never a float; money and quantities are integers on this platform).
    Integer,
    /// A boolean.
    Boolean,
}

impl ParamType {
    /// Whether a JSON value is of this type.
    #[must_use]
    pub fn accepts(self, value: &Json) -> bool {
        match self {
            ParamType::String => value.is_string(),
            ParamType::Integer => value.is_i64() || value.is_u64(),
            ParamType::Boolean => value.is_boolean(),
        }
    }

    /// The JSON Schema `type` name.
    #[must_use]
    pub fn json_name(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Integer => "integer",
            ParamType::Boolean => "boolean",
        }
    }
}

/// A remedy's parameter schema: a subset of JSON Schema (an object with typed properties,
/// required names and a closed or open tail).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamSchema {
    /// The named properties and their types.
    pub properties: BTreeMap<String, ParamType>,
    /// The names that must be present.
    pub required: Vec<String>,
    /// Whether keys outside `properties` are allowed.
    pub additional: bool,
}

impl ParamSchema {
    /// An empty schema: no parameters, no extra keys allowed.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The schema with one required property added.
    #[must_use]
    pub fn with_required(mut self, name: &str, kind: ParamType) -> Self {
        self.properties.insert(name.to_owned(), kind);
        self.required.push(name.to_owned());
        self
    }

    /// The schema with one optional property added.
    #[must_use]
    pub fn with_optional(mut self, name: &str, kind: ParamType) -> Self {
        self.properties.insert(name.to_owned(), kind);
        self
    }

    /// Validates a JSON value against the schema.
    ///
    /// # Errors
    ///
    /// A [`ParamViolation`] naming the offending path and what was wrong: the value is not an
    /// object, a required name is missing, a value has the wrong type, or an unknown key appears
    /// with `additional` false.
    pub fn validate(&self, value: &Json) -> Result<(), ParamViolation> {
        let Json::Object(map) = value else {
            return Err(ParamViolation {
                path: String::new(),
                detail: "the parameters must be a JSON object".to_owned(),
            });
        };
        for name in &self.required {
            if !map.contains_key(name) {
                return Err(ParamViolation {
                    path: name.clone(),
                    detail: "the required parameter is missing".to_owned(),
                });
            }
        }
        for (name, value) in map {
            match self.properties.get(name) {
                Some(kind) => {
                    if !kind.accepts(value) {
                        return Err(ParamViolation {
                            path: name.clone(),
                            detail: format!("the value must be a JSON {}", kind.json_name()),
                        });
                    }
                }
                None if self.additional => {}
                None => {
                    return Err(ParamViolation {
                        path: name.clone(),
                        detail: "the parameter is not in the schema".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// The schema as a JSON Schema document (`{"type":"object", ...}`), for the agent's tools.
    #[must_use]
    pub fn to_json_schema(&self) -> Json {
        let mut properties = Map::new();
        for (name, kind) in &self.properties {
            let mut spec = Map::new();
            spec.insert("type".to_owned(), Json::String(kind.json_name().to_owned()));
            properties.insert(name.clone(), Json::Object(spec));
        }
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Json::String("object".to_owned()));
        schema.insert("properties".to_owned(), Json::Object(properties));
        schema.insert(
            "required".to_owned(),
            Json::Array(self.required.iter().cloned().map(Json::String).collect()),
        );
        schema.insert(
            "additionalProperties".to_owned(),
            Json::Bool(self.additional),
        );
        Json::Object(schema)
    }
}

/// Why a value did not satisfy a [`ParamSchema`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamViolation {
    /// The offending path (the property name, or empty for the whole object).
    pub path: String,
    /// What was wrong.
    pub detail: String,
}

/// Everything a remedy says about itself before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemedyMeta {
    /// The human-facing title.
    pub title: String,
    /// The tier the remedy needs.
    pub tier: Tier,
    /// How much it can go wrong.
    pub risk: Risk,
    /// The document types it may affect (empty for infrastructure remedies).
    pub affects: Vec<DocTypeRef>,
    /// The parameter schema.
    pub params: ParamSchema,
    /// The precondition, in words, for the operator.
    pub preconditions: String,
    /// Whether and how undo works.
    pub undo: UndoKind,
}

impl RemedyMeta {
    /// The action effect the tier maps to, for the tier authoriser's catalogue: T0 reads, T1 is
    /// reversible, T2 repairs data, T3 is structural (blueprint WS13).
    #[must_use]
    pub fn effect(&self) -> Effect {
        match self.tier {
            Tier::T0 => Effect::Read,
            Tier::T1 => Effect::Reversible,
            Tier::T2 => Effect::DataRepair,
            Tier::T3 => Effect::Structural,
        }
    }
}

/// The precondition's verdict: ready to run, or blocked with a reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// The remedy may run.
    Ready {
        /// A sentence describing what the precondition saw.
        summary: String,
    },
    /// The remedy must not run.
    Blocked {
        /// A stable code (by convention `remedies.precondition_failed`).
        code: String,
        /// Why it is blocked.
        summary: String,
    },
}

impl Check {
    /// A ready verdict with its summary.
    #[must_use]
    pub fn ready(summary: &str) -> Self {
        Check::Ready {
            summary: summary.to_owned(),
        }
    }

    /// A blocked verdict with its code and summary.
    #[must_use]
    pub fn blocked(code: &str, summary: &str) -> Self {
        Check::Blocked {
            code: code.to_owned(),
            summary: summary.to_owned(),
        }
    }

    /// Whether the remedy may run.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Check::Ready { .. })
    }

    /// The summary either way.
    #[must_use]
    pub fn summary(&self) -> &str {
        match self {
            Check::Ready { summary } | Check::Blocked { summary, .. } => summary,
        }
    }
}

/// One target's state before and after a change: the unit of a plan's promise and an undo
/// record's restoration.
///
/// `target` is a typed reference string (`job:17`, `cache:sessions`, `doc:SINV-1`); `before` and
/// `after` are stable state fingerprints the framework can read back to detect a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// The typed target reference.
    pub target: String,
    /// The state fingerprint before the change.
    pub before: String,
    /// The state fingerprint after the change.
    pub after: String,
}

impl Change {
    /// A change from `before` to `after` on a typed target.
    #[must_use]
    pub fn new(target: &str, before: &str, after: &str) -> Self {
        Self {
            target: target.to_owned(),
            before: before.to_owned(),
            after: after.to_owned(),
        }
    }
}

/// One step of a dry-run plan: what will be done to which target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// The typed target reference.
    pub target: String,
    /// The typed operation's name (`job.retry`, `index.rebuild`).
    pub operation: String,
    /// A sentence describing the step.
    pub detail: String,
}

impl PlanStep {
    /// A plan step.
    #[must_use]
    pub fn new(target: &str, operation: &str, detail: &str) -> Self {
        Self {
            target: target.to_owned(),
            operation: operation.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

/// The hash that binds a [`Plan`] to its remedy, parameters and steps.
///
/// Apply recomputes it: a plan whose hash does not match was not produced by this remedy's
/// dry-run for these parameters, and is refused as stale. The hash is a 64-bit FNV-1a over the
/// canonical JSON of the parameters and the plan's steps, so it is deterministic and needs no
/// hashing dependency.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanHash(String);

impl PlanHash {
    /// Computes the hash for a remedy, its parameters and its steps.
    #[must_use]
    pub fn compute(remedy: &RemedyId, params: &Json, steps: &[PlanStep]) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(remedy.as_str().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(canonical_json(params).as_bytes());
        for step in steps {
            bytes.push(0);
            bytes.extend_from_slice(step.target.as_bytes());
            bytes.push(b'|');
            bytes.extend_from_slice(step.operation.as_bytes());
            bytes.push(b'|');
            bytes.extend_from_slice(step.detail.as_bytes());
        }
        Self(format!("{:016x}", fnv1a(&bytes)))
    }

    /// The hash as lower-case hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A dry-run plan: exactly what apply will do, sealed with a [`PlanHash`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The remedy the plan belongs to.
    pub remedy: RemedyId,
    /// The parameters the plan was built from.
    pub params: Json,
    /// The steps, in order.
    pub steps: Vec<PlanStep>,
    /// The binding hash.
    pub hash: PlanHash,
}

impl Plan {
    /// Seals a plan: computes its hash from the remedy, parameters and steps.
    #[must_use]
    pub fn seal(remedy: RemedyId, params: Json, steps: Vec<PlanStep>) -> Self {
        let hash = PlanHash::compute(&remedy, &params, &steps);
        Self {
            remedy,
            params,
            steps,
            hash,
        }
    }

    /// Whether the plan's hash still binds it to its remedy, parameters and steps.
    #[must_use]
    pub fn verify(&self) -> bool {
        self.hash == PlanHash::compute(&self.remedy, &self.params, &self.steps)
    }

    /// A one-line summary of the plan.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("{} step(s) for {}", self.steps.len(), self.remedy)
    }
}

/// The token that names an apply's undo record.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UndoToken(String);

impl UndoToken {
    /// A token derived deterministically from the plan hash, the apply instant and a sequence
    /// number, so two applies of the same plan never share one.
    #[must_use]
    pub fn derive(plan_hash: &PlanHash, applied_at: i64, seq: u64) -> Self {
        let bytes = format!("{}:{}:{}", plan_hash.as_str(), applied_at, seq);
        Self(format!("undo-{:016x}", fnv1a(bytes.as_bytes())))
    }

    /// The token's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UndoToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an apply produced: the changes it made and the token that names its undo record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    /// The remedy.
    pub remedy: RemedyId,
    /// The plan's hash (the promise the apply kept).
    pub plan_hash: PlanHash,
    /// The changes, each with its before and after fingerprints.
    pub changes: Vec<Change>,
    /// The undo token.
    pub undo_token: UndoToken,
}

/// What an undo produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Undone {
    /// The remedy.
    pub remedy: RemedyId,
    /// The token undone.
    pub token: UndoToken,
    /// The changes reversed.
    pub changes: Vec<Change>,
}

/// The record an apply stores so the change can be reversed inside its window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoRecord {
    /// The token.
    pub token: UndoToken,
    /// The remedy.
    pub remedy: RemedyId,
    /// The plan hash.
    pub plan_hash: PlanHash,
    /// When the apply ran (unix seconds).
    pub applied_at: i64,
    /// When the undo window closes (unix seconds).
    pub expires_at: i64,
    /// The changes, whose `after` fingerprints guard against later changes.
    pub changes: Vec<Change>,
    /// Whether undo is meaningful.
    pub undo: UndoKind,
}

impl UndoRecord {
    /// Whether the window has closed at `now`.
    #[must_use]
    pub fn is_expired(&self, now: i64) -> bool {
        now > self.expires_at
    }
}

/// The canonical JSON text of a value: objects sorted by key, no insignificant whitespace.
///
/// `serde_json`'s default object is a `BTreeMap`, so serialisation is already key-sorted; this
/// only fixes the text form for hashing.
#[must_use]
pub fn canonical_json(value: &Json) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("null"))
}

/// A 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
