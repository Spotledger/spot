//! The compiled plan of one DocType: the dense field index, the per-field data, the link,
//! table and dynamic-link maps, [`DocPlan::check_value`] and the canonical [`PlanHash`].
//!
//! A plan is compiled from the **definition** and its compiled **app view** (both of one
//! `EffectiveMeta`, so they always correspond): the view contributes the [`FieldKind`] and the
//! already-parsed default `Value`; the definition contributes the flags and the constraints
//! the app view does not carry (`unique`, `max_chars`, `scale`, `precision`) and the raw
//! default text, which is cross-checked against the view so a malformed default is a
//! **compile** error, never a runtime one.
//!
//! One constraint the plan adds rather than copies: the **naming backstop** (WP-DOC-05.T4). A
//! DocType with a `naming` pattern has unique names, and the plan says so — its title field is
//! [`PlanField::unique`] whether or not the definition says `unique`, and the plan lists the
//! unique index the storage must create for it ([`DocPlan::unique_indexes`]), so the database
//! refuses a duplicate name even when some writer skipped the engine's name lock.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use spot_app_api::meta::{
    AttachFormat, DecimalFormat, DocTypeDef, FieldKind, IntFormat, JsonFormat, LongTextFormat,
    TextFormat,
};
use spot_app_api::prelude::{Decimal, Name, Value};
use spot_expr::{Expr, parse as parse_expr};
use spot_meta::Definition;

use crate::error::PlanError;
use crate::hash::PlanHash;
use crate::indexes::{PlanIndex, naming_title_field};
use crate::naming::NamingPlan;
use crate::rules::RuleDependencyGraph;

/// Dense position of a field in a [`DocPlan`], in declaration order (engine-managed fields
/// included).
///
/// Indexes are handed out `0..n` in the order the fields are declared; the name index makes
/// the lookup O(1). A `FieldIx` is only meaningful for the `DocPlan` it indexes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldIx(usize);

impl FieldIx {
    /// The field index at `index` in a plan's field order.
    #[must_use]
    pub fn new(index: usize) -> Self {
        Self(index)
    }

    /// The position in the plan's field order.
    #[must_use]
    pub fn as_usize(self) -> usize {
        self.0
    }
}

/// A link field and its target field for `fetch_from`.
///
/// `link_field.target_field` fetches the value from the linked document.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchFromPath {
    /// The link field to follow.
    pub link_field: String,
    /// The field in the target document to read.
    pub target_field: String,
}

impl FetchFromPath {
    /// Parses a fetch_from path like `link_field.target_field`.
    /// Returns None if the path is malformed.
    pub(crate) fn parse(path: &str) -> Option<Self> {
        let mut parts = path.split('.');
        let link_field = parts.next()?.to_owned();
        let target_field = parts.next()?.to_owned();
        // No more parts allowed
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            link_field,
            target_field,
        })
    }
}

/// The raw rule texts of one field, kept so a diagnostic can point at the exact position of an
/// unknown field in the expression the author wrote (the parsed [`Expr`] carries no offsets).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct RuleTexts {
    /// The `depends_on` text, when set.
    pub(crate) depends_on: Option<String>,
    /// The `mandatory_depends_on` text, when set.
    pub(crate) mandatory_depends_on: Option<String>,
    /// The `read_only_depends_on` text, when set.
    pub(crate) read_only_depends_on: Option<String>,
    /// The `=`-prefixed default text (without the leading `=`), when set.
    pub(crate) default: Option<String>,
}

/// One planned field: everything the engine needs about it, precomputed.
///
/// `kind` and `default` come from the field's compiled app view (so a malformed default is a
/// compile error that never reaches here); the rules and the constraints come from the
/// definition. `options` is the select field's values as a **sorted, distinct** set, whatever
/// the declaration order was.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanField {
    /// Storage name.
    pub name: Name,
    /// The field's kind, as the app API sees it (ADR-068 §6).
    pub kind: FieldKind,
    /// Must be non-null when saved.
    pub required: bool,
    /// Not editable by users; the engine decides per state.
    pub read_only: bool,
    /// May change on a posted document.
    pub allow_on_posted: bool,
    /// Values must be unique.
    ///
    /// True when the definition says `unique`, and also on the title field of a DocType with a
    /// `naming` pattern even when the definition does not: the engine locks a name and the
    /// storage keeps it unique, so a duplicate name is impossible either way (WP-DOC-05.T4).
    pub unique: bool,
    /// The default a new document starts with, already parsed into a [`Value`] at compile
    /// time; `None` when the field has no default or its default is an expression.
    pub default: Option<Value>,
    /// The default when it is an **expression** (`=doc.qty * 2`), parsed once at compile time;
    /// the host evaluates it when the document is created (never this crate).
    pub default_expr: Option<Arc<Expr>>,
    /// Longest text (`text` only).
    pub max_chars: Option<u32>,
    /// Digits after the point (`decimal` only).
    pub scale: Option<u8>,
    /// Total digits (`decimal` only).
    pub precision: Option<u8>,
    /// The allowed values of a `select` field, sorted and distinct; empty on every other
    /// field.
    pub options: BTreeSet<String>,
    /// Expression: field is mandatory (required) when this is true.
    pub mandatory_depends_on: Option<Arc<Expr>>,
    /// Expression: field is read-only when this is true.
    pub read_only_depends_on: Option<Arc<Expr>>,
    /// Expression: field is visible when this is true.
    pub depends_on: Option<Arc<Expr>>,
    /// Path to fetch a value from a linked document.
    pub fetch_from: Option<FetchFromPath>,
    /// The raw rule texts, for diagnostics.
    pub(crate) sources: RuleTexts,
}

/// The compiled plan of one DocType: everything the engine needs to validate and shape a
/// document, precomputed, immutable and cheap to share.
#[derive(Clone, Debug, PartialEq)]
pub struct DocPlan {
    doctype: Name,
    fields: Vec<PlanField>,
    by_name: HashMap<String, FieldIx>,
    links: BTreeMap<Name, Name>,
    tables: BTreeMap<Name, Name>,
    dynamic_links: BTreeMap<Name, Name>,
    rules: RuleDependencyGraph,
    naming: Option<NamingPlan>,
    unique_indexes: Vec<PlanIndex>,
    hash: PlanHash,
}

impl DocPlan {
    /// Compiles the plan of one DocType from its definition and its compiled app view.
    ///
    /// Both must be the pair one `EffectiveMeta` carries: the definition's fields and the
    /// view's fields correspond position by position. Every discovered problem is collected,
    /// never just the first; a raw default that the view carries no parsed value for is a
    /// compile error naming the field.
    ///
    /// # Errors
    ///
    /// Returns every [`PlanError`] found; nothing is built on any error.
    pub fn compile(doctype: &Definition, view: &DocTypeDef) -> Result<Self, Vec<PlanError>> {
        let dname = view.name.as_str().to_owned();
        let mut errors: Vec<PlanError> = Vec::new();
        if view.fields.len() != doctype.fields.len() {
            return Err(vec![PlanError::ViewMismatch {
                doctype: dname,
                message: format!(
                    "the definition has {} fields but the app view has {}",
                    doctype.fields.len(),
                    view.fields.len()
                ),
            }]);
        }

        let (mut fields, mut by_name) = (
            Vec::with_capacity(doctype.fields.len()),
            HashMap::with_capacity(doctype.fields.len()),
        );
        let (mut links, mut tables, mut dynamic_links) =
            (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        for (spec, view_field) in doctype.fields.iter().zip(view.fields.iter()) {
            if spec.name != view_field.name.as_str() {
                errors.push(PlanError::ViewMismatch {
                    doctype: dname.clone(),
                    message: format!(
                        "the definition's field `{}` is `{}` in the app view",
                        spec.name, view_field.name
                    ),
                });
                continue;
            }
            // The app view holds the default already parsed into a `Value`. A definition
            // default that produced none does not parse for the field's type — unless it is an
            // expression default (`=...`), which spot-meta keeps as text and never parses by
            // type. spot-meta's validation refuses a malformed literal default before anything
            // compiles, so within one `EffectiveMeta` this cannot fire; the check keeps the
            // compile-time contract for every pair a caller hands over.
            let is_expr_default = spec.default.as_deref().is_some_and(is_expression_default);
            if !is_expr_default
                && let Some(raw) = &spec.default
                && view_field.default.is_none()
            {
                errors.push(PlanError::BadDefault {
                    doctype: dname.clone(),
                    field: spec.name.clone(),
                    default: raw.clone(),
                    message:
                        "it does not parse for this field's type; the app view carries no parsed default"
                            .into(),
                });
                continue;
            }
            let name = view_field.name.clone();
            let (mut link, mut table, mut dynamic) = (None, None, None);
            match &view_field.kind {
                FieldKind::Link { doctype } => link = Some(doctype.clone()),
                FieldKind::DynamicLink { type_field } => dynamic = Some(type_field.clone()),
                FieldKind::Table { doctype } => table = Some(doctype.clone()),
                FieldKind::TableMultiselect { doctype, .. } => table = Some(doctype.clone()),
                _ => {}
            }
            if let Some(target) = &link {
                links.insert(name.clone(), target.clone());
            }
            if let Some(child) = &table {
                tables.insert(name.clone(), child.clone());
            }
            if let Some(type_field) = &dynamic {
                dynamic_links.insert(name.clone(), type_field.clone());
            }
            // The constraints come from the compiled kind, which applies the decimal defaults
            // (scale 0, precision 18) and carries the select values.
            let (max_chars, scale, precision) = match &view_field.kind {
                FieldKind::Text { max_chars, .. } => (*max_chars, None, None),
                FieldKind::Decimal {
                    scale, precision, ..
                } => (None, Some(*scale), Some(*precision)),
                _ => (None, None, None),
            };
            let options = select_options(&view_field.kind);
            by_name.insert(name.as_str().to_owned(), FieldIx::new(fields.len()));
            // The rule expressions are parsed **once**, here, so a malformed rule fails when
            // the metadata changes, never when a user saves a document.
            let mandatory_depends_on = parse_rule(
                &dname,
                &spec.name,
                "mandatory_depends_on",
                spec.mandatory_depends_on.as_deref(),
                &mut errors,
            );
            let read_only_depends_on = parse_rule(
                &dname,
                &spec.name,
                "read_only_depends_on",
                spec.read_only_depends_on.as_deref(),
                &mut errors,
            );
            let depends_on = parse_rule(
                &dname,
                &spec.name,
                "depends_on",
                spec.depends_on.as_deref(),
                &mut errors,
            );
            let default_expr = if is_expr_default {
                spec.default
                    .as_deref()
                    .and_then(|text| text.strip_prefix('='))
                    .and_then(|source| {
                        parse_expr_source(&dname, &spec.name, "default", source, &mut errors)
                    })
            } else {
                None
            };
            let fetch_from =
                parse_fetch_from(&dname, &spec.name, spec.fetch_from.as_deref(), &mut errors);
            let sources = RuleTexts {
                depends_on: spec.depends_on.clone(),
                mandatory_depends_on: spec.mandatory_depends_on.clone(),
                read_only_depends_on: spec.read_only_depends_on.clone(),
                default: if is_expr_default {
                    spec.default
                        .as_deref()
                        .and_then(|t| t.strip_prefix('='))
                        .map(str::to_owned)
                } else {
                    None
                },
            };
            fields.push(PlanField {
                name,
                kind: view_field.kind.clone(),
                required: spec.required,
                read_only: spec.read_only,
                allow_on_posted: spec.allow_on_posted,
                unique: spec.unique,
                default: view_field.default.clone(),
                default_expr,
                max_chars,
                scale,
                precision,
                options,
                mandatory_depends_on,
                read_only_depends_on,
                depends_on,
                fetch_from,
                sources,
            });
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        // The dependency graph and the naming plan are validated once, against the whole field
        // set: an unknown field, a cycle and a bad `field:` reference are compile errors.
        let mut plan = Self {
            doctype: view.name.clone(),
            fields,
            by_name,
            links,
            tables,
            dynamic_links,
            rules: RuleDependencyGraph::default(),
            naming: None,
            unique_indexes: Vec::new(),
            hash: PlanHash::empty(),
        };
        match RuleDependencyGraph::build(&plan) {
            Ok(graph) => plan.rules = graph,
            Err(mut es) => errors.append(&mut es),
        }
        if let Some(pattern) = &doctype.naming {
            match NamingPlan::parse(pattern) {
                Ok(naming) => {
                    let mut unknown = false;
                    for field in &naming.field_refs {
                        if plan.field_ix(field).is_none() {
                            errors.push(PlanError::NamingInvalid {
                                doctype: dname.clone(),
                                message: format!("the pattern names field `{field}`, which the DocType does not have"),
                            });
                            unknown = true;
                        }
                    }
                    if !unknown {
                        plan.naming = Some(naming);
                    }
                }
                Err(message) => errors.push(PlanError::NamingInvalid {
                    doctype: dname.clone(),
                    message,
                }),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        // The naming backstop (WP-DOC-05.T4): a DocType with a `naming` pattern has unique
        // names, so its title field is `unique` and the storage must hold a unique index on it
        // — a second row with the same name is refused by the database, not only by the
        // engine's name lock. A title field the definition already marks `unique`, or a
        // declared unique index over the same column, gives the same index name, so there is
        // exactly one index and never a second.
        if let Some(title) = naming_title_field(doctype)
            && let Some(ix) = plan.field_ix(title)
        {
            plan.fields[ix.as_usize()].unique = true;
        }
        plan.unique_indexes = plan.collect_unique_indexes();
        let hash = PlanHash::of_bytes(&canonical_bytes(&plan));
        Ok(Self { hash, ..plan })
    }

    /// The DocType the plan is for.
    #[must_use]
    pub fn name(&self) -> &Name {
        &self.doctype
    }

    /// Every planned field, in declaration order (engine-managed fields included).
    #[must_use]
    pub fn fields(&self) -> &[PlanField] {
        &self.fields
    }

    /// A field's dense index by name; O(1).
    #[must_use]
    pub fn field_ix(&self, name: &str) -> Option<FieldIx> {
        self.by_name.get(name).copied()
    }

    /// A planned field by name; O(1).
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&PlanField> {
        self.field_ix(name).map(|ix| &self.fields[ix.as_usize()])
    }

    /// Link fields to their target DocType (`link` fields only, engine-managed
    /// `amended_from` included; an `attach` points at the file records, which the engine
    /// checks).
    #[must_use]
    pub fn links(&self) -> &BTreeMap<Name, Name> {
        &self.links
    }

    /// Table fields to their child DocType (`table` and `table_multiselect`).
    #[must_use]
    pub fn tables(&self) -> &BTreeMap<Name, Name> {
        &self.tables
    }

    /// Dynamic-link fields to the field naming their target DocType.
    #[must_use]
    pub fn dynamic_links(&self) -> &BTreeMap<Name, Name> {
        &self.dynamic_links
    }

    /// The rule dependency graph: for each field, which fields its rules read, and the reverse
    /// map of which fields a change affects.
    #[must_use]
    pub fn rules(&self) -> &RuleDependencyGraph {
        &self.rules
    }

    /// The parsed naming plan, when the DocType declares a naming pattern.
    #[must_use]
    pub fn naming(&self) -> Option<&NamingPlan> {
        self.naming.as_ref()
    }

    /// The single-column unique indexes the plan asks the storage for, in field declaration
    /// order.
    ///
    /// One per field whose plan says [`PlanField::unique`], which includes the **naming
    /// backstop**: the title field of a DocType with a `naming` pattern is listed here even
    /// when the definition does not mark it `unique`, so the storage refuses a duplicate name
    /// whatever wrote it (WP-DOC-05.T4). The names follow the storage catalogue's rule
    /// (`uq_<doctype>_<column>`, cut and hashed when over the identifier limit), so a field
    /// that is already `unique` and a naming backstop over the same column are one index, not
    /// two.
    ///
    /// Multi-field indexes a DocType declares stay the storage catalogue's business and are
    /// not repeated here.
    #[must_use]
    pub fn unique_indexes(&self) -> &[PlanIndex] {
        &self.unique_indexes
    }

    /// The unique indexes implied by the field flags, in field order.
    fn collect_unique_indexes(&self) -> Vec<PlanIndex> {
        self.fields
            .iter()
            .filter(|field| field.unique)
            .map(|field| PlanIndex::over(self.doctype.as_str(), &field.name))
            .collect()
    }

    /// The stable hash of the plan's canonical form: two builds of the same metadata are
    /// provably identical. The metadata version is not part of it.
    #[must_use]
    pub fn plan_hash(&self) -> &PlanHash {
        &self.hash
    }

    /// Checks one value against one field of the plan.
    ///
    /// The checks, in order: the field exists; the kind accepts the value (exactly
    /// [`FieldKind::accepts`], so a child table accepts nothing here and `Null` fits every
    /// scalar kind); the text is not too long; the decimal fits the field's scale and
    /// precision (rejected, never rounded, ADR-12 — the storage rule, so a value whose
    /// over-scale digits are all trailing zeros passes); the text of a select field is one of
    /// its options. Never panics: an unknown field is reported, not unwrapped.
    ///
    /// # Errors
    ///
    /// [`PlanError::UnknownField`], [`PlanError::KindMismatch`], [`PlanError::TooLong`],
    /// [`PlanError::Scale`], [`PlanError::Precision`] or [`PlanError::NotAnOption`], each
    /// naming the field and its DocType.
    pub fn check_value(&self, field: &str, value: &Value) -> Result<(), PlanError> {
        let Some(&ix) = self.by_name.get(field) else {
            return Err(PlanError::UnknownField {
                doctype: self.doctype.as_str().to_owned(),
                field: field.to_owned(),
            });
        };
        // `ix` was handed out from this very vector, so the lookup is in bounds by
        // construction; it cannot panic for any caller input.
        let f = &self.fields[ix.as_usize()];
        if !f.kind.accepts(value) {
            return Err(PlanError::KindMismatch {
                doctype: self.doctype.as_str().to_owned(),
                field: f.name.as_str().to_owned(),
                got: value_shape(value),
            });
        }
        match value {
            Value::Text(s) => {
                if let Some(max) = f.max_chars
                    && s.chars().count() > max as usize
                {
                    return Err(PlanError::TooLong {
                        doctype: self.doctype.as_str().to_owned(),
                        field: f.name.as_str().to_owned(),
                        max,
                    });
                }
                if matches!(f.kind, FieldKind::Select { .. }) && !f.options.contains(s) {
                    return Err(PlanError::NotAnOption {
                        doctype: self.doctype.as_str().to_owned(),
                        field: f.name.as_str().to_owned(),
                        value: s.clone(),
                    });
                }
            }
            Value::Decimal(d) => {
                if let Some(scale) = f.scale
                    && d.scale() > u32::from(scale)
                    && d.normalize().scale() > u32::from(scale)
                {
                    return Err(PlanError::Scale {
                        doctype: self.doctype.as_str().to_owned(),
                        field: f.name.as_str().to_owned(),
                        scale,
                    });
                }
                if let (Some(scale), Some(precision)) = (f.scale, f.precision)
                    && decimal_int_digits(d)
                        > usize::from(precision).saturating_sub(usize::from(scale))
                {
                    return Err(PlanError::Precision {
                        doctype: self.doctype.as_str().to_owned(),
                        field: f.name.as_str().to_owned(),
                        precision,
                        scale,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Whether a default text is an **expression** default (`=doc.qty * 2`), as opposed to a
/// literal default that happens to start with `=` (written `==`, per the metadata format).
fn is_expression_default(text: &str) -> bool {
    text.starts_with('=') && !text.starts_with("==")
}

/// Parses one optional rule expression, reporting a parse error with its position.
fn parse_rule(
    doctype: &str,
    field: &str,
    rule: &str,
    text: Option<&str>,
    errors: &mut Vec<PlanError>,
) -> Option<Arc<Expr>> {
    text.and_then(|source| parse_expr_source(doctype, field, rule, source, errors))
}

/// Parses an expression's source, reporting a parse error with its position.
fn parse_expr_source(
    doctype: &str,
    field: &str,
    rule: &str,
    source: &str,
    errors: &mut Vec<PlanError>,
) -> Option<Arc<Expr>> {
    match parse_expr(source) {
        Ok(expr) => Some(Arc::new(expr)),
        Err(error) => {
            errors.push(PlanError::ExprInvalid {
                doctype: doctype.to_owned(),
                field: field.to_owned(),
                rule: rule.to_owned(),
                position: error.offset,
                message: error.message,
            });
            None
        }
    }
}

/// Parses a `fetch_from` path; spot-meta validates its shape, so a failure here is defensive.
fn parse_fetch_from(
    doctype: &str,
    field: &str,
    text: Option<&str>,
    errors: &mut Vec<PlanError>,
) -> Option<FetchFromPath> {
    match text {
        None => None,
        Some(text) => match FetchFromPath::parse(text) {
            Some(path) => Some(path),
            None => {
                errors.push(PlanError::ExprInvalid {
                    doctype: doctype.to_owned(),
                    field: field.to_owned(),
                    rule: "fetch_from".to_owned(),
                    position: 0,
                    message: "expected `<link_field>.<target_field>`".to_owned(),
                });
                None
            }
        },
    }
}

/// The sorted, distinct select values of a compiled kind; empty for every other kind.
fn select_options(kind: &FieldKind) -> BTreeSet<String> {
    match kind {
        FieldKind::Select { values } => values.iter().cloned().collect(),
        _ => BTreeSet::new(),
    }
}

/// The integer digits of a decimal, as the storage rule counts them, without any float.
fn decimal_int_digits(d: &Decimal) -> usize {
    d.trunc().abs().to_string().trim_start_matches('0').len()
}

/// What a value is, in words, for error messages.
fn value_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Int(_) => "an integer",
        Value::Decimal(_) => "a decimal",
        Value::Text(_) => "text",
        Value::Date(_) => "a date",
        Value::Time(_) => "a time",
        Value::DateTime(_) => "an instant",
        Value::Id(_) => "a link id",
        Value::Json(_) => "a JSON document",
        Value::Bytes(_) => "bytes",
        Value::GeoPoint(_) => "a geo point",
        Value::Range(_) => "a range",
    }
}

/// The canonical form of a plan: compact JSON (`serde_json` sorts object keys; the field list
/// keeps declaration order). The metadata version is not part of a plan, so it is not hashed.
pub(crate) fn canonical_bytes(plan: &DocPlan) -> Vec<u8> {
    let fields: Vec<serde_json::Value> = plan.fields.iter().map(field_json).collect();
    let unique_indexes: Vec<serde_json::Value> = plan
        .unique_indexes
        .iter()
        .map(|index| {
            serde_json::json!({
                "name": index.name(),
                "column": index.column().as_str(),
            })
        })
        .collect();
    let map = |m: &BTreeMap<Name, Name>| {
        m.iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_owned(),
                    serde_json::Value::String(v.as_str().to_owned()),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>()
    };
    let canonical = serde_json::json!({
        "doctype": plan.doctype.as_str(),
        "fields": fields,
        "links": map(&plan.links),
        "tables": map(&plan.tables),
        "dynamic_links": map(&plan.dynamic_links),
        "naming": plan.naming.as_ref().map(|n| format!("{n:?}")),
        "unique_indexes": unique_indexes,
    });
    serde_json::to_vec(&canonical).unwrap_or_default()
}

/// One field's canonical data.
fn field_json(f: &PlanField) -> serde_json::Value {
    let options: Vec<&String> = f.options.iter().collect();
    serde_json::json!({
        "name": f.name.as_str(),
        "kind": kind_json(&f.kind),
        "required": f.required,
        "read_only": f.read_only,
        "allow_on_posted": f.allow_on_posted,
        "unique": f.unique,
        "default": f.default.as_ref().map(value_json),
        "default_expr": f.sources.default,
        "max_chars": f.max_chars,
        "scale": f.scale,
        "precision": f.precision,
        "options": options,
        "mandatory_depends_on": f.sources.mandatory_depends_on,
        "read_only_depends_on": f.sources.read_only_depends_on,
        "depends_on": f.sources.depends_on,
        "fetch_from": f.fetch_from.as_ref().map(|ff| serde_json::json!({
            "link_field": ff.link_field,
            "target_field": ff.target_field,
        })),
    })
}

/// The canonical form of a field's kind. Every variant gets its own shape; a kind this build
/// of `spot-app-api` does not know falls back to its `Debug` rendering, so a new kind is
/// hashed distinctly rather than silently colliding.
fn kind_json(kind: &FieldKind) -> serde_json::Value {
    use FieldKind as K;
    match kind {
        K::Bool => serde_json::json!({"bool": null}),
        K::Int { format } => serde_json::json!({"int": {"format": int_format_json(format)}}),
        K::Decimal {
            scale,
            precision,
            format,
        } => serde_json::json!({
            "decimal": {
                "scale": scale,
                "precision": precision,
                "format": decimal_format_json(format)
            }
        }),
        K::Text { max_chars, format } => serde_json::json!({
            "text": {"max_chars": max_chars, "format": text_format_json(format)}
        }),
        K::LongText { format } => {
            serde_json::json!({"long_text": {"format": long_text_format_json(format)}})
        }
        // The values themselves are hashed as the sorted `options` set.
        K::Select { .. } => serde_json::json!({"select": null}),
        K::Date => serde_json::json!({"date": null}),
        K::Time => serde_json::json!({"time": null}),
        K::DateTime => serde_json::json!({"datetime": null}),
        K::Link { doctype } => serde_json::json!({"link": doctype.as_str()}),
        K::DynamicLink { type_field } => serde_json::json!({"dynamic_link": type_field.as_str()}),
        K::Json { format } => serde_json::json!({"json": {"format": json_format_json(format)}}),
        K::Bytes => serde_json::json!({"bytes": null}),
        K::Attach { format } => {
            serde_json::json!({"attach": {"format": attach_format_json(format)}})
        }
        K::Secret => serde_json::json!({"secret": null}),
        K::Table { doctype } => serde_json::json!({"table": doctype.as_str()}),
        K::TableMultiselect {
            doctype,
            link_field,
        } => serde_json::json!({
            "table_multiselect": {
                "doctype": doctype.as_str(),
                "link_field": link_field.as_str()
            }
        }),
        _ => serde_json::json!({"other": format!("{kind:?}")}),
    }
}

/// The canonical form of a `text` format (ADR-068 §2).
fn text_format_json(format: &TextFormat) -> serde_json::Value {
    match format {
        TextFormat::Plain => serde_json::json!("plain"),
        TextFormat::Email => serde_json::json!("email"),
        TextFormat::Phone => serde_json::json!("phone"),
        TextFormat::Url => serde_json::json!("url"),
        TextFormat::Color => serde_json::json!("color"),
        TextFormat::Barcode { symbology } => serde_json::json!({"barcode": symbology}),
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of a `long_text` format.
fn long_text_format_json(format: &LongTextFormat) -> serde_json::Value {
    match format {
        LongTextFormat::Plain => serde_json::json!("plain"),
        LongTextFormat::Markdown => serde_json::json!("markdown"),
        LongTextFormat::Html => serde_json::json!("html"),
        LongTextFormat::Code { syntax } => serde_json::json!({"code": syntax}),
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of an `int` format.
fn int_format_json(format: &IntFormat) -> serde_json::Value {
    match format {
        IntFormat::Plain => serde_json::json!("plain"),
        IntFormat::Duration => serde_json::json!("duration"),
        IntFormat::Rating { max } => serde_json::json!({"rating": max}),
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of a `decimal` format.
fn decimal_format_json(format: &DecimalFormat) -> serde_json::Value {
    match format {
        DecimalFormat::Plain => serde_json::json!("plain"),
        DecimalFormat::Percent => serde_json::json!("percent"),
        DecimalFormat::Currency { currency_field } => {
            serde_json::json!({"currency": currency_field.as_str()})
        }
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of a `json` format.
fn json_format_json(format: &JsonFormat) -> serde_json::Value {
    match format {
        JsonFormat::Plain => serde_json::json!("plain"),
        JsonFormat::Geolocation => serde_json::json!("geolocation"),
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of an `attach` format.
fn attach_format_json(format: &AttachFormat) -> serde_json::Value {
    match format {
        AttachFormat::File => serde_json::json!("file"),
        AttachFormat::Image => serde_json::json!("image"),
        AttachFormat::Signature => serde_json::json!("signature"),
        _ => serde_json::json!(format!("{format:?}")),
    }
}

/// The canonical form of a value: a tagged, exactly round-tripping string for every type that
/// is not directly a JSON scalar. `Decimal` goes through its `Display` (which keeps its
/// scale), `Date`/`Time`/`DateTime` through theirs (the ISO forms), `Id` through the hyphenated
/// UUID, `Bytes` as lowercase hex and `Json` as the raw text (a default can never exist on a
/// `json` field, so no JSON re-parsing happens here — and no float is ever produced).
fn value_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number(serde_json::Number::from(*i)),
        Value::Decimal(d) => serde_json::json!({"decimal": d.to_string()}),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Date(d) => serde_json::json!({"date": d.to_string()}),
        Value::Time(t) => serde_json::json!({"time": t.to_string()}),
        Value::DateTime(t) => serde_json::json!({"instant": t.to_string()}),
        Value::Id(id) => serde_json::json!({"id": id.to_string()}),
        Value::Json(s) => serde_json::json!({"json": s}),
        Value::Bytes(b) => serde_json::json!({"bytes": crate::hash::hex(b)}),
        // ADR-087: no field kind can carry a default of these yet (WP-MET-05.T1); the exact
        // debug form keeps the plan hash deterministic until one is defined.
        Value::GeoPoint(_) | Value::Range(_) => {
            serde_json::json!({"constructed": format!("{value:?}")})
        }
    }
}
