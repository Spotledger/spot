//! The world one question is answered against: the permission engine built from the read
//! model's roles, duties, assignments, field grants, groups and shares, plus the duty lookup
//! and the query-filter evaluation the four questions share.
//!
//! [`build`] is pure: it reads only the model and the instant. Nothing here caches across
//! calls, so a question's answer depends on the model version it was handed and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_iam_roles::{
    OutsideGuard, Registry, RoleName, SnapshotInput, build_snapshot_input_guarded,
};
use spot_perm_query::{PermissionFilter, ReadFilter, ReadTarget};
use spot_permissions::{Model, PermissionEngine, Snapshot};
use spot_storage_contract::{CmpOp, Expr, Name, Value};
use time::OffsetDateTime;

use crate::error::QuestionsError;
use crate::model::{AccessModel, DocumentRecord};

/// A value restriction a caller asks about: one link or select field and the value a document
/// would carry. `who_can_do` answers for a document with exactly these values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ScopeValue {
    /// The link or select field name on the document.
    pub field: String,
    /// The value, as the scope's allowed list writes it (a record id's text, or a select
    /// value).
    pub value: String,
}

/// The scope a question about a whole DocType is asked in: the values a document of interest
/// would carry. Empty means "everywhere" (a document with no restricted value).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ScopeQuery {
    /// The values the document carries, in field order.
    #[serde(default)]
    pub values: Vec<ScopeValue>,
}

impl ScopeQuery {
    /// The scope of a document that carries no restricted value.
    #[must_use]
    pub fn everywhere() -> ScopeQuery {
        ScopeQuery::default()
    }

    /// The scope at one value of one field.
    #[must_use]
    pub fn at(field: &str, value: &str) -> ScopeQuery {
        ScopeQuery {
            values: vec![ScopeValue {
                field: field.to_owned(),
                value: value.to_owned(),
            }],
        }
    }

    /// The same query with one more value.
    #[must_use]
    pub fn and(mut self, field: &str, value: &str) -> ScopeQuery {
        self.values.push(ScopeValue {
            field: field.to_owned(),
            value: value.to_owned(),
        });
        self
    }

    /// The document facts these values describe.
    #[must_use]
    pub fn document(&self, doctype: &str, name: &str) -> DocumentRecord {
        let mut doc = DocumentRecord::named(doctype, name);
        for v in &self.values {
            doc.values
                .insert(v.field.clone(), Value::Text(v.value.clone()));
        }
        doc
    }

    /// The query's text, for the audit row.
    #[must_use]
    pub fn subject(&self) -> String {
        if self.values.is_empty() {
            return "everywhere".to_owned();
        }
        self.values
            .iter()
            .map(|v| format!("{}= {}", v.field, v.value))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The read target `spot-perm-query` compiles against: the source alias and the creator
/// column. The questions evaluate the compiled filter against one document record, so the
/// alias only has to be consistent. `None` only if the fixed names were somehow rejected,
/// which cannot happen for the literals below; callers fail closed when it does.
#[must_use]
pub fn read_target() -> Option<ReadTarget> {
    Some(ReadTarget {
        alias: Name::new("doc").ok()?,
        owner_field: Name::new("created_by").ok()?,
    })
}

/// The engine, the snapshot input and the duty lookup one question runs against.
#[derive(Clone, Debug)]
pub struct World {
    /// The decision core, over the model's snapshot.
    pub engine: PermissionEngine,
    /// The per-user active assignments and the resolved roles, for the why-chain.
    pub input: SnapshotInput,
    /// The instant the model was built at.
    pub at: OffsetDateTime,
    /// Every duty each role reaches, directly or through inheritance.
    duties: BTreeMap<RoleName, BTreeSet<String>>,
    /// The DocType names the metadata knows, in name order.
    doctypes: Vec<String>,
}

impl World {
    /// The principal for `user`: their active assignments, each with its own scope.
    #[must_use]
    pub fn principal(&self, user: &str) -> spot_permissions::Principal {
        self.input.principal(user)
    }

    /// The active assignment ids of `user`, in order.
    #[must_use]
    pub fn assignment_ids(&self, user: &str) -> Vec<String> {
        self.input
            .assignments
            .get(user)
            .map(|a| a.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default()
    }

    /// Every user with at least one active assignment, sorted.
    #[must_use]
    pub fn users(&self) -> Vec<String> {
        self.input.assignments.keys().cloned().collect()
    }

    /// Whether the metadata knows `doctype`.
    #[must_use]
    pub fn knows_doctype(&self, doctype: &str) -> bool {
        self.doctypes.iter().any(|d| d == doctype)
    }

    /// The DocType names the metadata knows, in name order.
    #[must_use]
    pub fn doctypes(&self) -> &[String] {
        &self.doctypes
    }

    /// Whether `user` holds `duty` in any active assignment, directly or through a role's
    /// inheritance.
    #[must_use]
    pub fn has_duty(&self, user: &str, duty: &str) -> bool {
        let Some(assignments) = self.input.assignments.get(user) else {
            return false;
        };
        assignments.iter().any(|a| {
            a.roles.iter().any(|r| {
                self.duties
                    .get(&RoleName(r.as_str().to_owned()))
                    .is_some_and(|d| d.contains(duty))
            })
        })
    }
}

/// Builds the world for `model` at `at`.
///
/// # Errors
///
/// [`QuestionsError::ModelInvalid`] when an assignment is invalid, names an unknown role, a
/// grant names an unknown field or an undeclared protection group.
///
/// The portal rule (ADR-083 Barrier 3) is applied to the model's
/// [`AccessModel::portal_users`]: an active assignment of a portal user whose role fails the
/// allow-list grants nothing and is listed in `input.refused`. A caller with its own guard
/// uses [`build_guarded`].
pub fn build(model: &dyn AccessModel, at: OffsetDateTime) -> Result<World, QuestionsError> {
    let guard = OutsideGuard::new(model.registry(), model.portal_users().iter().cloned());
    build_guarded(model, at, &guard)
}

/// [`build`] with the portal rule (ADR-083 Barrier 3) applied to the snapshot input: an active
/// assignment of a portal user whose role fails the allow-list grants nothing, and is listed in
/// `World`'s `input.refused`.
///
/// # Errors
///
/// As [`build`].
pub fn build_guarded(
    model: &dyn AccessModel,
    at: OffsetDateTime,
    guard: &OutsideGuard,
) -> Result<World, QuestionsError> {
    let input = build_snapshot_input_guarded(model.registry(), model.assignments(), at, guard)
        .map_err(|e| QuestionsError::ModelInvalid {
            subject: "assignments".to_owned(),
            detail: e.to_string(),
        })?;
    let mut field_grants = input.field_grants.clone();
    field_grants.extend(model.field_grants().iter().cloned());
    field_grants.sort();
    let perm_model = Model {
        grants: input.grants.clone(),
        field_grants,
        groups: model.groups().to_vec(),
        shares: model.shares().iter().map(|s| s.to_share()).collect(),
    };
    let mut snapshot =
        Snapshot::build(model.meta(), &perm_model).map_err(|e| QuestionsError::ModelInvalid {
            subject: "snapshot".to_owned(),
            detail: e.to_string(),
        })?;
    for doctype in model.doctypes() {
        let facts = model.field_facts(&doctype);
        if facts.is_empty() {
            continue;
        }
        snapshot = snapshot.with_field_facts(&doctype, facts).map_err(|e| {
            QuestionsError::ModelInvalid {
                subject: format!("field facts: {doctype}"),
                detail: e.to_string(),
            }
        })?;
    }
    let duties = duties_by_role(model.registry());
    let doctypes = model.doctypes();
    Ok(World {
        engine: PermissionEngine::new(Arc::new(snapshot)),
        input,
        at,
        duties,
        doctypes,
    })
}

/// Every duty each role reaches, directly or through its inheritance.
fn duties_by_role(registry: &Registry) -> BTreeMap<RoleName, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    for role in registry.roles() {
        let mut duties: BTreeSet<String> = BTreeSet::new();
        let mut seen: BTreeSet<RoleName> = BTreeSet::new();
        collect_duties(registry, &role.name, &mut seen, &mut duties);
        out.insert(role.name.clone(), duties);
    }
    out
}

fn collect_duties(
    registry: &Registry,
    role: &RoleName,
    seen: &mut BTreeSet<RoleName>,
    out: &mut BTreeSet<String>,
) {
    if !seen.insert(role.clone()) {
        return;
    }
    let Some(definition) = registry.role(role) else {
        return;
    };
    for used in &definition.duties {
        out.insert(used.name.as_str().to_owned());
    }
    for parent in &definition.parents {
        collect_duties(registry, parent, seen, out);
    }
}

/// The integer epoch second an audit row uses for `at`.
#[must_use]
pub fn epoch(at: OffsetDateTime) -> i64 {
    at.unix_timestamp()
}
///
/// `All` is true, `None` is false, and a filter's expression is evaluated over the document's
/// facts. The permission filter only emits `Field`, `Value`, `Cmp` (`=`/`<>`), `And`, `Or`,
/// `Not` and `In`; any other expression shape is treated as **false** (fail closed), never as
/// a silent allow.
#[must_use]
pub fn read_filter_allows(filter: &ReadFilter, doc: &DocumentRecord, target: &ReadTarget) -> bool {
    match filter {
        ReadFilter::All => true,
        ReadFilter::None => false,
        ReadFilter::Filter(expr) => eval_bool(expr, doc, target),
    }
}

/// Compiles the read filter for `user`'s principal on `doctype` and evaluates it against
/// `doc`.
///
/// # Errors
///
/// [`QuestionsError::ModelInvalid`] when the query machinery refuses the filter (too many
/// shares or too large a filter).
pub fn user_can_read(
    world: &World,
    user: &str,
    doc: &DocumentRecord,
) -> Result<bool, QuestionsError> {
    let principal = world.principal(user);
    let target = read_target().ok_or_else(|| QuestionsError::ModelInvalid {
        subject: "read target".to_owned(),
        detail: "the fixed alias `doc` or owner column `created_by` was rejected".to_owned(),
    })?;
    let filter = PermissionFilter::for_read(&world.engine, &principal, &doc.doctype, &target)
        .map_err(|e| QuestionsError::ModelInvalid {
            subject: format!("read filter for {user} on {}", doc.doctype),
            detail: e.to_string(),
        })?;
    Ok(read_filter_allows(&filter, doc, &target))
}

fn eval_bool(expr: &Expr, doc: &DocumentRecord, target: &ReadTarget) -> bool {
    match expr {
        Expr::And(parts) => parts.iter().all(|p| eval_bool(p, doc, target)),
        Expr::Or(parts) => parts.iter().any(|p| eval_bool(p, doc, target)),
        Expr::Not(inner) => !eval_bool(inner, doc, target),
        Expr::IsNull(inner) => matches!(eval_value(inner, doc, target), Value::Null),
        Expr::Cmp(a, op, b) => {
            let left = eval_value(a, doc, target);
            let right = eval_value(b, doc, target);
            match op {
                CmpOp::Eq => values_equal(&left, &right),
                CmpOp::Ne => !values_equal(&left, &right),
                _ => false,
            }
        }
        Expr::In(inner, list) => {
            let value = eval_value(inner, doc, target);
            list.iter().any(|candidate| values_equal(&value, candidate))
        }
        _ => false,
    }
}

fn eval_value(expr: &Expr, doc: &DocumentRecord, target: &ReadTarget) -> Value {
    match expr {
        Expr::Value(value) => value.clone(),
        Expr::Field(field) => {
            if field.alias != target.alias {
                return Value::Null;
            }
            let name = field.field.as_str();
            if name == "id" {
                return doc.id.map_or(Value::Null, Value::Id);
            }
            if name == target.owner_field.as_str() {
                return doc.created_by.clone().map_or(Value::Null, Value::Text);
            }
            doc.values.get(name).cloned().unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Id(x), Value::Id(y)) => x == y,
        (Value::Id(x), Value::Text(t)) | (Value::Text(t), Value::Id(x)) => x.to_string() == *t,
        _ => a == b,
    }
}
