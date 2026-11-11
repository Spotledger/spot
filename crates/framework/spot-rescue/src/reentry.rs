//! Operations committed after the snapshot, ordered causally and classified for the old
//! version.
//!
//! The source of truth is the outbox and the document history: the outbox gives the operations
//! the new version committed, the audit history gives the principal and the document's timeline.
//! [`OperationSource`] is the seam a host implements over those two; [`FixedOperations`] and
//! [`CombinedOperations`] are the reference readers for tests and for hosts that already hold
//! the journal in memory.
//!
//! The planner never writes. It checks each operation against [`OldSchema`] and applies the
//! declared [`DownTransform`]s, so the engine knows before the first write whether an item is
//! replayable as-is, needs a shape transform, or cannot be expressed in the old version at all.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use spot_permissions::Principal;

use crate::BoxFuture;
use crate::codes;
use crate::error::{Error, Result};

/// A re-entry item's identity: the key a person sees in the report and the journal.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationKey(String);

impl OperationKey {
    /// A key from an operation id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The key as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The id of an operation the new version committed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Validates and wraps an operation id: non-empty and printable.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the id is empty or carries a control character.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "operation id".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if id.chars().any(char::is_control) {
            return Err(Error::InvalidInput {
                what: "operation id".to_owned(),
                reason: "must be printable".to_owned(),
            });
        }
        Ok(Self(id))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What kind of operation the new version committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// A document created after the snapshot.
    Create,
    /// A document updated after the snapshot; the payload is its committed state.
    Update,
    /// A document deleted after the snapshot; the old version cannot resurrect it from the
    /// restored snapshot, so the planner always classifies this as blocked.
    Delete,
}

impl OperationKind {
    /// A short, stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// One operation recovered from the outbox and document history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedOperation {
    /// The operation's stable id (also its re-entry key).
    pub id: OperationId,
    /// Commit order in the journal; the deterministic tie-break of the causal order.
    pub sequence: u64,
    /// What happened.
    pub kind: OperationKind,
    /// The DocType the operation wrote.
    pub doctype: String,
    /// The committed payload, as the new version wrote it (ADR-055 value encoding as JSON).
    pub payload: Value,
    /// The identity that committed it; re-entry runs under the same person.
    pub principal: Principal,
    /// Unix microseconds the operation committed; must be strictly after the snapshot time.
    pub committed_at: i64,
    /// Operations that must be re-entered before this one, by id.
    #[serde(default)]
    pub causes: Vec<OperationId>,
}

impl CommittedOperation {
    /// An operation with no stated causes.
    #[must_use]
    pub fn new(
        id: OperationId,
        sequence: u64,
        kind: OperationKind,
        doctype: impl Into<String>,
        payload: Value,
        principal: Principal,
        committed_at: i64,
    ) -> Self {
        Self {
            id,
            sequence,
            kind,
            doctype: doctype.into(),
            payload,
            principal,
            committed_at,
            causes: Vec::new(),
        }
    }

    /// The same operation with these causal predecessors.
    #[must_use]
    pub fn with_causes(mut self, causes: Vec<OperationId>) -> Self {
        self.causes = causes;
        self
    }

    /// The re-entry key.
    #[must_use]
    pub fn key(&self) -> OperationKey {
        OperationKey::new(self.id.as_str())
    }
}

/// Reads the operations committed after the snapshot at `T`.
///
/// A real implementation unions the outbox (business events, payloads and command ids) with the
/// document audit history (principal, operation kind and committed time), deduplicating by
/// operation id. Neither store is touched by this crate directly.
pub trait OperationSource: Send + Sync {
    /// Every operation committed strictly after `snapshot_at`, in the source's own order.
    ///
    /// # Errors
    /// A backend error when the outbox or the history cannot be read.
    fn operations_after(&self, snapshot_at: i64) -> BoxFuture<'_, Result<Vec<CommittedOperation>>>;
}

/// An operation source over a fixed list, the reference reader for tests.
#[derive(Clone, Debug, Default)]
pub struct FixedOperations {
    operations: Vec<CommittedOperation>,
}

impl FixedOperations {
    /// A source returning `operations`.
    #[must_use]
    pub fn new(operations: Vec<CommittedOperation>) -> Self {
        Self { operations }
    }

    /// A source returning nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

impl OperationSource for FixedOperations {
    fn operations_after(&self, snapshot_at: i64) -> BoxFuture<'_, Result<Vec<CommittedOperation>>> {
        Box::pin(async move {
            Ok(self
                .operations
                .iter()
                .filter(|operation| operation.committed_at > snapshot_at)
                .cloned()
                .collect())
        })
    }
}

/// The union of several sources (the outbox and the document history), deduplicated by
/// operation id in first-seen order.
#[derive(Clone, Default)]
pub struct CombinedOperations {
    sources: Vec<std::sync::Arc<dyn OperationSource>>,
}

impl CombinedOperations {
    /// An empty union.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same union with `source` appended.
    #[must_use]
    pub fn with_source(mut self, source: std::sync::Arc<dyn OperationSource>) -> Self {
        self.sources.push(source);
        self
    }
}

impl OperationSource for CombinedOperations {
    fn operations_after(&self, snapshot_at: i64) -> BoxFuture<'_, Result<Vec<CommittedOperation>>> {
        Box::pin(async move {
            let mut seen = BTreeSet::new();
            let mut operations = Vec::new();
            for source in &self.sources {
                for operation in source.operations_after(snapshot_at).await? {
                    if seen.insert(operation.id.clone()) {
                        operations.push(operation);
                    }
                }
            }
            Ok(operations)
        })
    }
}

/// The fields the old version knows for one DocType.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OldDocType {
    /// Every field the old version accepts.
    pub fields: BTreeSet<String>,
    /// The fields the old version requires on a create or update.
    pub required: BTreeSet<String>,
}

impl OldDocType {
    /// A DocType over `fields` with no required field.
    #[must_use]
    pub fn new(fields: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            fields: fields.into_iter().map(Into::into).collect(),
            required: BTreeSet::new(),
        }
    }

    /// The same DocType with `required` fields.
    #[must_use]
    pub fn with_required(mut self, required: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.required = required.into_iter().map(Into::into).collect();
        self
    }
}

/// The old version's schema, as far as re-entry needs it: which DocTypes exist, which fields
/// they accept and which they require.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OldSchema {
    doctypes: BTreeMap<String, OldDocType>,
}

impl OldSchema {
    /// An empty schema.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same schema with `doctype` added or replaced.
    #[must_use]
    pub fn with_doctype(mut self, doctype: impl Into<String>, definition: OldDocType) -> Self {
        self.doctypes.insert(doctype.into(), definition);
        self
    }

    /// The DocType's definition, if the old version has it.
    #[must_use]
    pub fn doctype(&self, name: &str) -> Option<&OldDocType> {
        self.doctypes.get(name)
    }

    /// Every known DocType name, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.doctypes.keys().map(String::as_str).collect()
    }
}

/// A declared transform that maps a changed field back to the old version's shape.
///
/// The declaration lives with the release metadata (WP-MIG-02) beside the down-step; this crate
/// applies it and never guesses one. A transform that does not match a payload is simply not
/// used, so one declaration list serves every operation of its DocType.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "transform")]
pub enum DownTransform {
    /// A field was renamed: move `from` to `to`.
    Rename {
        /// The DocType the transform applies to.
        doctype: String,
        /// The new field name.
        from: String,
        /// The old field name.
        to: String,
    },
    /// A field was dropped by the new version: remove it for the old one.
    Drop {
        /// The DocType the transform applies to.
        doctype: String,
        /// The field to remove.
        field: String,
    },
    /// A field became required in the new version: supply the old version's value.
    Default {
        /// The DocType the transform applies to.
        doctype: String,
        /// The field to fill.
        field: String,
        /// The value to use when it is absent.
        value: Value,
    },
    /// A field's values were remapped (for example a renamed enum variant).
    Remap {
        /// The DocType the transform applies to.
        doctype: String,
        /// The field whose values changed.
        field: String,
        /// New value to old value.
        values: BTreeMap<String, String>,
    },
}

impl DownTransform {
    /// The DocType this transform applies to.
    #[must_use]
    pub fn doctype(&self) -> &str {
        match self {
            Self::Rename { doctype, .. }
            | Self::Drop { doctype, .. }
            | Self::Default { doctype, .. }
            | Self::Remap { doctype, .. } => doctype,
        }
    }

    /// A stable label for the report.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Rename { from, to, .. } => format!("rename:{from}->{to}"),
            Self::Drop { field, .. } => format!("drop:{field}"),
            Self::Default { field, .. } => format!("default:{field}"),
            Self::Remap { field, .. } => format!("remap:{field}"),
        }
    }

    /// Whether this transform has anything to do for `payload`.
    #[must_use]
    pub fn applies(&self, payload: &Map<String, Value>) -> bool {
        match self {
            Self::Rename { from, to, .. } => {
                payload.contains_key(from) && !payload.contains_key(to)
            }
            Self::Drop { field, .. } => payload.contains_key(field),
            Self::Default { field, .. } => !payload.contains_key(field),
            Self::Remap { field, values, .. } => payload
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| values.contains_key(value)),
        }
    }

    /// Applies the transform in place; returns whether it changed anything.
    pub fn apply(&self, payload: &mut Map<String, Value>) -> bool {
        match self {
            Self::Rename { from, to, .. } => match payload.remove(from) {
                Some(value) => {
                    payload.insert(to.clone(), value);
                    true
                }
                None => false,
            },
            Self::Drop { field, .. } => payload.remove(field).is_some(),
            Self::Default { field, value, .. } => {
                if payload.contains_key(field) {
                    false
                } else {
                    payload.insert(field.clone(), value.clone());
                    true
                }
            }
            Self::Remap { field, values, .. } => {
                let Some(value) = payload.get(field).and_then(Value::as_str) else {
                    return false;
                };
                let Some(mapped) = values.get(value) else {
                    return false;
                };
                payload.insert(field.clone(), Value::String(mapped.clone()));
                true
            }
        }
    }
}

/// What the old version can do with one operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "classification")]
pub enum Classification {
    /// The payload is already in the old version's shape.
    Replayable,
    /// One or more declared transforms map the payload back.
    NeedsTransform {
        /// The transforms that were applied, in declaration order.
        transforms: Vec<String>,
    },
    /// The old version cannot express the operation.
    Blocked {
        /// The stable reason code.
        code: String,
        /// A secret-free explanation for the report.
        detail: String,
    },
}

impl Classification {
    /// A short, stable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Replayable => "replayable",
            Self::NeedsTransform { .. } => "needs_transform",
            Self::Blocked { .. } => "blocked",
        }
    }

    /// Whether the operation can run.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }
}

/// One operation with the payload prepared for the old version and its classification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOperation {
    /// The operation as the outbox and history recorded it, original payload and principal.
    pub operation: CommittedOperation,
    /// The payload the old version receives (unchanged when replayable).
    pub prepared: Value,
    /// What the old version can do with it.
    pub classification: Classification,
    /// The idempotency key of the item's write, `<run>:reentry:<operation>` (ADR-071).
    pub command_id: String,
}

impl PlannedOperation {
    /// The operation key.
    #[must_use]
    pub fn key(&self) -> OperationKey {
        self.operation.key()
    }

    /// This operation as the item the old engine receives.
    #[must_use]
    pub fn item(&self, run_id: &str, attempt: u32) -> ReentryItem {
        ReentryItem {
            run_id: run_id.to_owned(),
            attempt,
            step: "reentry".to_owned(),
            key: self.key(),
            sequence: self.operation.sequence,
            kind: self.operation.kind,
            doctype: self.operation.doctype.clone(),
            payload: self.prepared.clone(),
            original_payload: self.operation.payload.clone(),
            principal: self.operation.principal.clone(),
            command_id: self.command_id.clone(),
        }
    }
}

/// One item handed to the old version's [`Reenter`](crate::engine::Reenter) engine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReentryItem {
    /// The ADR-071 run id.
    pub run_id: String,
    /// The ADR-071 attempt counter.
    pub attempt: u32,
    /// The run step (`reentry`).
    pub step: String,
    /// The item key.
    pub key: OperationKey,
    /// The operation's commit order, for stable reporting.
    pub sequence: u64,
    /// What happened.
    pub kind: OperationKind,
    /// The DocType to write.
    pub doctype: String,
    /// The prepared payload, in the old version's shape.
    pub payload: Value,
    /// The payload exactly as the new version committed it, preserved for a person.
    pub original_payload: Value,
    /// The principal the operation committed under.
    pub principal: Principal,
    /// The ADR-071 idempotency key.
    pub command_id: String,
}

/// Counts of operations by classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCounts {
    /// Operations that need no transform.
    pub replayable: u64,
    /// Operations a declared transform prepares.
    pub needs_transform: u64,
    /// Operations the old version cannot express.
    pub blocked: u64,
}

impl PlanCounts {
    /// The total number of planned operations.
    #[must_use]
    pub fn total(self) -> u64 {
        self.replayable + self.needs_transform + self.blocked
    }
}

/// Every operation committed after `T`, ordered causally and classified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReentryPlan {
    /// The restored snapshot time.
    pub snapshot_at: i64,
    /// Planned operations in causal order.
    pub items: Vec<PlannedOperation>,
}

impl ReentryPlan {
    /// The plan's counts.
    #[must_use]
    pub fn counts(&self) -> PlanCounts {
        let mut counts = PlanCounts::default();
        for item in &self.items {
            match item.classification {
                Classification::Replayable => counts.replayable += 1,
                Classification::NeedsTransform { .. } => counts.needs_transform += 1,
                Classification::Blocked { .. } => counts.blocked += 1,
            }
        }
        counts
    }

    /// Whether any item is blocked.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.classification.is_blocked())
    }

    /// The blocked items, in causal order.
    #[must_use]
    pub fn blocked(&self) -> Vec<&PlannedOperation> {
        self.items
            .iter()
            .filter(|item| item.classification.is_blocked())
            .collect()
    }

    /// The item with `key`, if planned.
    #[must_use]
    pub fn get(&self, key: &OperationKey) -> Option<&PlannedOperation> {
        self.items.iter().find(|item| item.key() == *key)
    }
}

/// The planner: the old schema and the declared transforms.
#[derive(Clone, Debug, Default)]
pub struct ReentryPlanner {
    old: OldSchema,
    transforms: Vec<DownTransform>,
}

impl ReentryPlanner {
    /// A planner for `old` with these declared transforms.
    #[must_use]
    pub fn new(old: OldSchema, transforms: Vec<DownTransform>) -> Self {
        Self { old, transforms }
    }

    /// The old schema.
    #[must_use]
    pub fn old_schema(&self) -> &OldSchema {
        &self.old
    }

    /// Builds the plan for operations committed after `snapshot_at`.
    ///
    /// Operations at or before the snapshot are not re-entered: the restored snapshot already
    /// contains them. The rest are ordered by their stated causes with commit order as the
    /// deterministic tie-break.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when two operations share an id or the causal links form a cycle.
    pub fn plan(
        &self,
        snapshot_at: i64,
        run_id: &str,
        operations: &[CommittedOperation],
    ) -> Result<ReentryPlan> {
        let later: Vec<&CommittedOperation> = operations
            .iter()
            .filter(|operation| operation.committed_at > snapshot_at)
            .collect();
        let ordered = causal_order(&later)?;
        let mut items = Vec::with_capacity(ordered.len());
        for operation in ordered {
            let (prepared, classification) = self.classify(operation);
            items.push(PlannedOperation {
                command_id: format!("{run_id}:reentry:{}", operation.id.as_str()),
                prepared,
                classification,
                operation: operation.clone(),
            });
        }
        Ok(ReentryPlan { snapshot_at, items })
    }

    /// Classifies one operation and prepares its payload.
    fn classify(&self, operation: &CommittedOperation) -> (Value, Classification) {
        if operation.kind == OperationKind::Delete {
            return (
                operation.payload.clone(),
                Classification::Blocked {
                    code: codes::UNSUPPORTED_OPERATION.to_owned(),
                    detail: "a document deleted after the snapshot cannot be resurrected by the \
                             restored old version; a person must decide how to express the \
                             deletion"
                        .to_owned(),
                },
            );
        }
        let Some(old) = self.old.doctype(&operation.doctype) else {
            return (
                operation.payload.clone(),
                Classification::Blocked {
                    code: codes::UNKNOWN_DOCTYPE.to_owned(),
                    detail: format!("the old version has no DocType `{}`", operation.doctype),
                },
            );
        };
        let Value::Object(original) = &operation.payload else {
            return (
                operation.payload.clone(),
                Classification::Blocked {
                    code: codes::UNSUPPORTED_OPERATION.to_owned(),
                    detail: "the payload is not a JSON object, so it has no field shape".to_owned(),
                },
            );
        };
        let mut prepared = original.clone();
        let mut used = Vec::new();
        for transform in &self.transforms {
            if transform.doctype() == operation.doctype
                && transform.applies(&prepared)
                && transform.apply(&mut prepared)
            {
                used.push(transform.label());
            }
        }
        let unknown: Vec<&String> = prepared
            .keys()
            .filter(|field| !old.fields.contains(*field))
            .collect();
        if !unknown.is_empty() {
            let fields = unknown
                .iter()
                .map(|field| field.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return (
                Value::Object(prepared),
                Classification::Blocked {
                    code: codes::UNKNOWN_FIELD.to_owned(),
                    detail: format!(
                        "the old version's `{}` has no field(s): {fields}",
                        operation.doctype
                    ),
                },
            );
        }
        let missing: Vec<&String> = old
            .required
            .iter()
            .filter(|field| !prepared.contains_key(*field))
            .collect();
        if !missing.is_empty() {
            let fields = missing
                .iter()
                .map(|field| field.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return (
                Value::Object(prepared),
                Classification::Blocked {
                    code: codes::MISSING_FIELD.to_owned(),
                    detail: format!(
                        "the old version's `{}` requires field(s) no declared transform supplies: \
                         {fields}",
                        operation.doctype
                    ),
                },
            );
        }
        if used.is_empty() {
            (Value::Object(prepared), Classification::Replayable)
        } else {
            (
                Value::Object(prepared),
                Classification::NeedsTransform { transforms: used },
            )
        }
    }
}

/// Orders operations by their stated causes, with commit order as the stable tie-break.
fn causal_order<'a>(operations: &[&'a CommittedOperation]) -> Result<Vec<&'a CommittedOperation>> {
    let mut by_id: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, operation) in operations.iter().enumerate() {
        if by_id.insert(operation.id.as_str(), index).is_some() {
            return Err(Error::InvalidInput {
                what: "operation id".to_owned(),
                reason: format!("`{}` appears more than once", operation.id),
            });
        }
    }
    let mut indegree = vec![0usize; operations.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); operations.len()];
    for (index, operation) in operations.iter().enumerate() {
        for cause in &operation.causes {
            if let Some(&parent) = by_id.get(cause.as_str()) {
                children[parent].push(index);
                indegree[index] += 1;
            }
        }
    }
    let mut ready: BTreeSet<(u64, usize)> = operations
        .iter()
        .enumerate()
        .filter(|(index, _)| indegree[*index] == 0)
        .map(|(index, operation)| (operation.sequence, index))
        .collect();
    let mut order = Vec::with_capacity(operations.len());
    while let Some((_, index)) = ready.pop_first() {
        order.push(operations[index]);
        for &child in &children[index] {
            indegree[child] -= 1;
            if indegree[child] == 0 {
                ready.insert((operations[child].sequence, child));
            }
        }
    }
    if order.len() != operations.len() {
        let cycle = operations
            .iter()
            .filter(|operation| !order.iter().any(|seen| seen.id == operation.id))
            .map(|operation| operation.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::InvalidInput {
            what: "operation causes".to_owned(),
            reason: format!("the causal links form a cycle among: {cycle}"),
        });
    }
    Ok(order)
}
