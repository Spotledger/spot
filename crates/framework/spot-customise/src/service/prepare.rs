//! Preparing a change: turning one call into record operations, and the rules that need the
//! caller, the live snapshot or the database before anything is validated.

use spot_meta::{CUSTOM_PREFIX, Definition, FieldSpec, FieldType, Property};
use spot_storage_contract::Value;

use super::CustomiseService;
use crate::assemble::CUSTOM_MODULE;
use crate::error::{CustomiseError, CustomiseResult, Issue, codes};
use crate::host::Caller;
use crate::records::{DocTypeRecord, FieldRecord, PropertyRecord, RecordOp, RecordState, Records};

/// Property keys a site may only tighten: clearing one on a standard field would loosen the
/// protection the site (or the app) put there.
const SECURITY_KEYS: [&str; 4] = ["protection_group", "classification", "masking", "permlevel"];

pub(super) enum Change<'a> {
    CreateDocTypes {
        defs: &'a [&'a str],
    },
    UpdateDocType {
        name: &'a str,
        def_toml: &'a str,
    },
    AddField {
        doctype: &'a str,
        spec: &'a FieldSpec,
        insert_after: Option<&'a str>,
    },
    RemoveField {
        doctype: &'a str,
        name: &'a str,
    },
    SetProperty {
        doctype: &'a str,
        field: Option<&'a str>,
        property: &'a str,
        value: &'a str,
    },
    ClearProperty {
        doctype: &'a str,
        field: Option<&'a str>,
        property: &'a str,
    },
}

pub(super) struct Prepared {
    pub(super) ops: Vec<RecordOp>,
    pub(super) pre: Vec<Issue>,
}

impl CustomiseService {
    // ---------------------------------------------------------------- preparing a change

    pub(super) async fn prepare(
        &self,
        caller: &Caller,
        change: &Change<'_>,
        records: &Records,
        intent: u64,
    ) -> CustomiseResult<Prepared> {
        let actor = caller.user.clone();
        let mut pre = Vec::new();
        let mut ops = Vec::new();
        match change {
            Change::CreateDocTypes { defs } => {
                let mut parse_issues = Vec::new();
                for def_toml in *defs {
                    let definition = match parse_definition(def_toml) {
                        Ok(definition) => definition,
                        Err(error) => {
                            parse_issues.extend(error.issues().iter().cloned());
                            continue;
                        }
                    };
                    let name = definition.name.clone();
                    let module = definition
                        .module
                        .clone()
                        .unwrap_or_else(|| CUSTOM_MODULE.to_owned());
                    if module != CUSTOM_MODULE && !self.access.is_platform_admin(caller) {
                        pre.push(Issue::new(
                            codes::MODULE_NOT_ALLOWED,
                            name.clone(),
                            format!("only a platform administrator may use the module `{module}`; custom DocTypes live in `{CUSTOM_MODULE}`"),
                        ));
                    }
                    let earlier = ops
                        .iter()
                        .any(|op| matches!(op, RecordOp::PutDocType(r) if r.name == name));
                    if earlier
                        || records
                            .doctype(&name)
                            .is_some_and(|r| r.state != RecordState::Failed)
                    {
                        pre.push(Issue::new(
                            codes::NAME_COLLISION,
                            name.clone(),
                            format!("a custom DocType named `{name}` already exists"),
                        ));
                    }
                    ops.push(RecordOp::PutDocType(DocTypeRecord {
                        name,
                        module,
                        definition: (*def_toml).to_owned(),
                        state: RecordState::Pending,
                        reason: None,
                        intent,
                        actor: actor.clone(),
                        prior: None,
                    }));
                }
                if !parse_issues.is_empty() {
                    parse_issues.extend(pre);
                    return Err(CustomiseError::Invalid {
                        issues: parse_issues,
                    });
                }
            }
            Change::UpdateDocType { name, def_toml } => {
                let existing = records
                    .doctype(name)
                    .filter(|r| r.state == RecordState::Applied)
                    .ok_or_else(|| CustomiseError::NotFound {
                        what: format!("custom DocType `{name}`"),
                    })?;
                let definition = parse_definition(def_toml)?;
                if definition.name != *name {
                    pre.push(Issue::new(
                        codes::BAD_DEFINITION,
                        (*name).to_owned(),
                        format!(
                            "the definition names `{}`; a DocType cannot be renamed",
                            definition.name
                        ),
                    ));
                }
                if definition
                    .module
                    .as_ref()
                    .is_some_and(|m| m != &existing.module)
                {
                    pre.push(Issue::new(
                        codes::MODULE_NOT_ALLOWED,
                        (*name).to_owned(),
                        "a DocType's module cannot change".to_owned(),
                    ));
                }
                ops.push(RecordOp::PutDocType(DocTypeRecord {
                    definition: (*def_toml).to_owned(),
                    state: RecordState::Pending,
                    reason: None,
                    intent,
                    actor,
                    prior: Some(existing.definition.clone()),
                    ..existing.clone()
                }));
            }
            Change::AddField {
                doctype,
                spec,
                insert_after,
            } => {
                let path = format!("{doctype}.{}", spec.name);
                if records
                    .field(doctype, &spec.name)
                    .is_some_and(|r| !matches!(r.state, RecordState::Failed | RecordState::Removed))
                {
                    pre.push(Issue::new(
                        codes::FIELD_COLLISION,
                        path.clone(),
                        format!(
                            "`{doctype}` already has a custom field named `{}`",
                            spec.name
                        ),
                    ));
                }
                if spec.required {
                    let rows = self
                        .schema
                        .has_rows(doctype)
                        .await
                        .map_err(|message| CustomiseError::Schema { message })?;
                    let is_child = self
                        .gate
                        .live()
                        .current()
                        .meta()
                        .definition(doctype)
                        .is_some_and(|d| d.is_child);
                    if rows && spec.default.is_none() {
                        pre.push(Issue::new(
                            codes::NEEDS_DEFAULT,
                            path.clone(),
                            format!("`{doctype}` already holds records, so a mandatory field needs a default to fill them with"),
                        ));
                    } else if rows && (is_child || backfill_value(spec).is_none()) {
                        pre.push(Issue::new(
                            codes::NEEDS_DEFAULT,
                            path.clone(),
                            format!("`{doctype}` already holds records and this default cannot be written into them: use a text, select, integer or true/false default, on a DocType that is not a child table"),
                        ));
                    }
                }
                let text = toml::to_string(spec).map_err(|e| CustomiseError::Invalid {
                    issues: vec![Issue::new(
                        codes::BAD_DEFINITION,
                        path.clone(),
                        e.to_string(),
                    )],
                })?;
                ops.push(RecordOp::PutField(FieldRecord {
                    doctype: (*doctype).to_owned(),
                    name: spec.name.clone(),
                    spec: text,
                    insert_after: insert_after.map(str::to_owned),
                    state: RecordState::Pending,
                    reason: None,
                    intent,
                    actor,
                }));
            }
            Change::RemoveField { doctype, name } => {
                let existing = records
                    .field(doctype, name)
                    .filter(|r| r.state == RecordState::Applied)
                    .ok_or_else(|| CustomiseError::NotFound {
                        what: format!("custom field `{doctype}.{name}`"),
                    })?;
                let path = format!("{doctype}.{name}");
                for other in records.fields.iter().filter(|o| {
                    o.doctype == *doctype
                        && o.name != *name
                        && o.insert_after.as_deref() == Some(name)
                        && matches!(o.state, RecordState::Pending | RecordState::Applied)
                }) {
                    pre.push(Issue::new(
                        codes::REFERENCED,
                        path.clone(),
                        format!(
                            "still referred to by field `{}` (placed after it)",
                            other.name
                        ),
                    ));
                }
                for referrer in self.referrers_of(doctype, name) {
                    pre.push(Issue::new(
                        codes::REFERENCED,
                        path.clone(),
                        format!("still referred to by {} `{}`", referrer.0, referrer.1),
                    ));
                }
                ops.push(RecordOp::PutField(FieldRecord {
                    state: RecordState::Removing,
                    intent,
                    actor: actor.clone(),
                    ..existing.clone()
                }));
                for p in records.properties.iter().filter(|p| {
                    p.doctype == *doctype
                        && p.field.as_deref() == Some(name)
                        && p.state == RecordState::Applied
                }) {
                    ops.push(RecordOp::PutProperty(PropertyRecord {
                        state: RecordState::Removing,
                        intent,
                        actor: actor.clone(),
                        ..p.clone()
                    }));
                }
            }
            Change::SetProperty {
                doctype,
                field,
                property,
                value,
            } => {
                let path = path_of(doctype, *field);
                let parsed =
                    Property::parse(property, value).map_err(|e| CustomiseError::Invalid {
                        issues: vec![Issue::new(codes::BAD_DEFINITION, path.clone(), e.message)],
                    })?;
                if let (Property::Permlevel(new), Some(field)) = (&parsed, field)
                    && let Some(old) = self.standard_field(doctype, field)
                    && *new < old.permlevel
                {
                    pre.push(Issue::new(
                        codes::SECURITY_LOOSENED,
                        path.clone(),
                        format!(
                            "permlevel {new} is below the current {}: a site may only tighten",
                            old.permlevel
                        ),
                    ));
                }
                let existing = records
                    .property(doctype, *field, property)
                    .filter(|r| r.state == RecordState::Applied);
                ops.push(RecordOp::PutProperty(PropertyRecord {
                    doctype: (*doctype).to_owned(),
                    field: field.map(str::to_owned),
                    property: (*property).to_owned(),
                    value: (*value).to_owned(),
                    state: RecordState::Pending,
                    reason: None,
                    intent,
                    actor,
                    prior: existing.map(|r| r.value.clone()),
                }));
            }
            Change::ClearProperty {
                doctype,
                field,
                property,
            } => {
                let existing = records
                    .property(doctype, *field, property)
                    .filter(|r| r.state == RecordState::Applied)
                    .ok_or_else(|| CustomiseError::NotFound {
                        what: format!(
                            "property override `{}.{property}`",
                            path_of(doctype, *field)
                        ),
                    })?;
                let standard = field.is_some_and(|f| self.standard_field(doctype, f).is_some());
                if SECURITY_KEYS.contains(property) && standard {
                    pre.push(Issue::new(
                        codes::SECURITY_LOOSENED,
                        path_of(doctype, *field),
                        format!("clearing `{property}` would loosen the protection of a standard field: a site may only tighten"),
                    ));
                }
                ops.push(RecordOp::PutProperty(PropertyRecord {
                    state: RecordState::Removing,
                    intent,
                    actor,
                    ..existing.clone()
                }));
            }
        }
        Ok(Prepared { ops, pre })
    }

    /// The field of the live snapshot, when it is a standard one (not a `custom_` field).
    pub(super) fn standard_field(&self, doctype: &str, field: &str) -> Option<FieldSpec> {
        if field.starts_with(CUSTOM_PREFIX) {
            return None;
        }
        self.gate
            .live()
            .current()
            .meta()
            .definition(doctype)
            .and_then(|d| d.fields.iter().find(|f| f.name == field).cloned())
    }

    /// Everything that still refers to `doctype.field`: the metadata's own references and
    /// whatever the host knows (rules, views, permissions).
    fn referrers_of(&self, doctype: &str, field: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        let current = self.gate.live().current();
        if let Some(def) = current.meta().definition(doctype) {
            if def.title_field.as_deref() == Some(field) {
                found.push(("title field".to_owned(), doctype.to_owned()));
            }
            for index in &def.indexes {
                if index.fields.iter().any(|f| f == field) {
                    found.push(("index".to_owned(), index.fields.join(",")));
                }
            }
            for other in def.fields.iter().filter(|f| f.name != field) {
                let expressions = [
                    &other.depends_on,
                    &other.mandatory_depends_on,
                    &other.read_only_depends_on,
                ];
                let in_expression = expressions
                    .iter()
                    .filter_map(|e| e.as_deref())
                    .any(|text| mentions(text, field));
                let in_fetch = other
                    .fetch_from
                    .as_deref()
                    .is_some_and(|f| f.split('.').next() == Some(field));
                if in_expression || in_fetch {
                    found.push(("field".to_owned(), other.name.clone()));
                }
            }
        }
        for r in self.referrers.referrers(doctype, field) {
            found.push((r.kind, r.name));
        }
        found
    }
}

fn parse_definition(text: &str) -> CustomiseResult<Definition> {
    Definition::parse("custom.toml", text).map_err(|e| CustomiseError::Invalid {
        issues: vec![Issue::new(codes::BAD_DEFINITION, String::new(), e.message)],
    })
}

fn path_of(doctype: &str, field: Option<&str>) -> String {
    match field {
        Some(field) => format!("{doctype}.{field}"),
        None => doctype.to_owned(),
    }
}

/// Whether `field` appears in `expression` as a whole identifier.
fn mentions(expression: &str, field: &str) -> bool {
    expression
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|token| token == field)
}

/// The value a new field's static default stands for, when it can be written into existing rows.
///
/// Text, long text, select, integer and boolean defaults can; an expression (`=doc.other`), a
/// dynamic default (`today`, `now`) or a default of any other type is applied by the engine when
/// a document is created, but is not written into rows that already exist.
pub(super) fn backfill_value(spec: &FieldSpec) -> Option<Value> {
    let default = spec.default.as_deref()?;
    if default.starts_with('=') {
        return None;
    }
    match spec.ty {
        FieldType::Text | FieldType::LongText | FieldType::Select => {
            Some(Value::Text(default.to_owned()))
        }
        FieldType::Int => default.parse().ok().map(Value::Int),
        FieldType::Bool => match default {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}
