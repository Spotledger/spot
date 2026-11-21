//! Compiles effective definitions to the two views: the storage `Catalog` and the app API's
//! `DocTypeDef`. Both are pure functions of their input, so equal input gives equal output.

use std::collections::BTreeMap;

use spot_app_api::lookup::{LOOKUP_VERSION, LookupSpec as AppLookupSpec};
use spot_app_api::meta::{
    AttachFormat, ControlHint, DecimalFormat, DocTypeDef, FieldDef as AppField, FieldHints,
    FieldKind, FieldSecurity, GroupName, IntFormat, JsonFormat, LongTextFormat, MaskFormat,
    ProtectionGroupDef, SyncClassHint, Tag, TextFormat,
};
use spot_app_api::prelude::Name;
use spot_storage_contract::Value;
use spot_storage_contract::schema::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, ExclusionDef, FieldDef,
    FieldType as Storage, IndexDef, LinkCheck, LinkTarget, RangeOf, SearchDef, SearchField,
    SearchWeight,
};
use spot_storage_contract::{Capabilities, GEO_SRID};

use crate::definition::{Definition, ExclusionSpec, FieldSpec, FieldType, LookupSpec, SearchSpec};
use crate::error::{MetaError, MetaErrorKind};
use crate::report::Warning;
use crate::validate::{DEFAULT_PRECISION, DEFAULT_RATING_MAX, FILE_DOCTYPE};

const DYNAMIC_REASON: &str = "dynamic links are engine-checked";
const FILE_REASON: &str = "file records are engine-checked";

fn name(s: &str) -> Result<Name, MetaError> {
    Name::new(s).map_err(|e| MetaError::new(MetaErrorKind::Compile, e.to_string()))
}

/// One reverse index of a traversable Link field (WP-STO-07.T3's `traversal::wanted`), as plain
/// data: `spot-migration-plan` depends on this crate, so this crate cannot call it and the
/// caller passes its output instead.
///
/// The index is on the **link column only** (`table.column`), a plain non-unique btree, named
/// as given. There is no `(link, id)` index until the nightly depth-3 measurement shows heap
/// fetches dominate (decision D3).
///
/// A Link field with `index: true` keeps its own `ix_` index and the entry adds a second one
/// under its own name: the two have different owners and lifetimes, so they are never shared.
/// Entries are de-duplicated by index name only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseIndex {
    /// The table: the DocType that holds the Link field.
    pub table: String,
    /// The indexed column: the Link field.
    pub column: String,
    /// The index name, exactly as the plan declares it.
    pub name: String,
}

/// The storage catalog for a set of effective definitions.
pub fn catalog(
    version: u64,
    defs: &[Definition],
    collation: Collation,
) -> Result<Catalog, MetaError> {
    catalog_with_reverse_indexes(version, defs, collation, &[])
}

/// The storage catalog for a set of effective definitions, with the reverse indexes of the
/// traversable Link fields in it, so the catalogue the backend planner diffs agrees with the
/// plan that builds them (no plan flip-flop).
///
/// Each [`ReverseIndex`] becomes one non-unique, single-column btree [`IndexDef`] on the table's
/// link column (the link column only, never `(link, id)`), named as given. An entry that names an
/// unknown DocType, a field that is not a Link, or a name an index of the definitions already
/// has (or another entry's name on another column) is a `Compile` error. An entry listed twice
/// is added once.
///
/// # Errors
/// `Compile` for an entry that cannot be applied, and for everything [`catalog`] refuses.
pub fn catalog_with_reverse_indexes(
    version: u64,
    defs: &[Definition],
    collation: Collation,
    reverse: &[ReverseIndex],
) -> Result<Catalog, MetaError> {
    let mut parent_of: BTreeMap<&str, &str> = BTreeMap::new();
    for d in defs {
        for f in d.fields.iter().filter(|f| f.ty.is_table()) {
            if let Some(child) = &f.options {
                parent_of.insert(child, &d.name);
            }
        }
    }
    let mut entities = defs
        .iter()
        .map(|d| entity(d, parent_of.get(d.name.as_str()).copied()))
        .collect::<Result<Vec<_>, _>>()?;
    add_reverse_indexes(defs, &mut entities, reverse)?;
    Catalog::new(version, collation, entities)
        .map_err(|e| MetaError::new(MetaErrorKind::Compile, e.to_string()))
}

/// The storage catalog for a set of effective definitions, compiled for a backend with the
/// given `capabilities` (ADR-087 section 4.7): a DocType that declares an exclusion or a search
/// the backend cannot serve is refused here, with `meta.backend_unsupported`, so the author
/// learns it when the DocType is compiled and not at the first query.
///
/// Storing a `geo` point is never refused (the point needs no spatial engine; only geo
/// filters do), and neither is a plain `range` field: the storage contract's own
/// `Capabilities::check_catalog` refuses that at `prepare`. Only a declared construct is
/// refused here, because only a declaration says the author is relying on it.
pub fn catalog_for(
    version: u64,
    defs: &[Definition],
    collation: Collation,
    capabilities: &Capabilities,
) -> Result<Catalog, MetaError> {
    catalog_for_with_reverse_indexes(version, defs, collation, capabilities, &[])
}

/// [`catalog_for`] with the reverse indexes of [`catalog_with_reverse_indexes`].
///
/// # Errors
/// As [`catalog_for`] and [`catalog_with_reverse_indexes`].
pub fn catalog_for_with_reverse_indexes(
    version: u64,
    defs: &[Definition],
    collation: Collation,
    capabilities: &Capabilities,
    reverse: &[ReverseIndex],
) -> Result<Catalog, MetaError> {
    let refuse = |d: &Definition, what: &str, flag: &str| {
        MetaError::new(
            MetaErrorKind::BackendUnsupported,
            format!(
                "invalid schema: `{}` declares {what}, which the `{}` backend cannot serve (it lacks `{flag}`)",
                d.name, capabilities.backend
            ),
        )
        .in_doctype(&d.name)
    };
    for d in defs {
        if !capabilities.range_exclusion && !d.exclusions.is_empty() {
            return Err(refuse(d, "an exclusion", "range_exclusion"));
        }
        if !capabilities.search && d.search.is_some() {
            return Err(refuse(d, "a search projection", "search"));
        }
    }
    catalog_with_reverse_indexes(version, defs, collation, reverse)
}

/// Adds the reverse indexes to the compiled entities, refusing an entry that cannot apply.
fn add_reverse_indexes(
    defs: &[Definition],
    entities: &mut [EntityDef],
    reverse: &[ReverseIndex],
) -> Result<(), MetaError> {
    let refuse = |r: &ReverseIndex, why: &str| {
        MetaError::new(MetaErrorKind::Compile, why)
            .in_doctype(&r.table)
            .in_field(&r.column)
    };
    // Names of the indexes the definitions produce, then those the entries add.
    let mut taken: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut from_defs = std::collections::BTreeSet::new();
    for e in entities.iter() {
        for i in &e.indexes {
            from_defs.insert(i.name.as_str().to_owned());
        }
    }
    for r in reverse {
        let Some(d) = defs.iter().find(|d| d.name == r.table) else {
            return Err(refuse(r, "a reverse index names an unknown DocType"));
        };
        if !d
            .fields
            .iter()
            .any(|f| f.name == r.column && f.ty == FieldType::Link)
        {
            return Err(refuse(r, "a reverse index needs a field that is a Link"));
        }
        if from_defs.contains(&r.name) {
            return Err(refuse(
                r,
                "a reverse index has the name of an index the definitions already produce",
            ));
        }
        let key = (r.table.clone(), r.column.clone());
        match taken.get(&r.name) {
            // The same entry again is the same index.
            Some(first) if *first == key => continue,
            Some(_) => {
                return Err(refuse(
                    r,
                    "two reverse indexes on different columns have the same name",
                ));
            }
            None => {
                taken.insert(r.name.clone(), key);
            }
        }
        let def = IndexDef {
            name: name(&r.name)?,
            fields: vec![name(&r.column)?],
            unique: false,
            method: spot_storage_contract::IndexMethod::BTree,
            opclass: None,
            include: vec![],
        };
        if let Some(e) = entities.iter_mut().find(|e| e.name.as_str() == r.table) {
            e.indexes.push(def);
        }
    }
    Ok(())
}

/// The storage name of the exclusion on `field`: `ex_<doctype>_<field>`, cut to fit.
fn exclusion_name(doctype: &str, field: &str) -> Result<Name, MetaError> {
    fit_name(format!("ex_{doctype}_{field}"))
}

fn exclusion(d: &Definition, x: &ExclusionSpec) -> Result<ExclusionDef, MetaError> {
    Ok(ExclusionDef {
        name: exclusion_name(&d.name, &x.field)?,
        field: name(&x.field)?,
        scope: x.scope.iter().map(|s| name(s)).collect::<Result<_, _>>()?,
        message: x.message.clone(),
    })
}

fn search(s: &SearchSpec) -> Result<SearchDef, MetaError> {
    let weight = |w: &str| match w {
        "A" => Ok(SearchWeight::A),
        "B" => Ok(SearchWeight::B),
        "C" => Ok(SearchWeight::C),
        "D" => Ok(SearchWeight::D),
        other => Err(MetaError::new(
            MetaErrorKind::Compile,
            format!("unknown search weight `{other}`"),
        )),
    };
    Ok(SearchDef {
        language: s.language.clone(),
        fields: s
            .fields
            .iter()
            .map(|f| {
                Ok(SearchField {
                    name: name(&f.name)?,
                    weight: weight(&f.weight)?,
                })
            })
            .collect::<Result<_, MetaError>>()?,
        facets: s.facets.iter().map(|f| name(f)).collect::<Result<_, _>>()?,
        typo_tolerance: s.typo_tolerance,
    })
}

fn entity(d: &Definition, parent: Option<&str>) -> Result<EntityDef, MetaError> {
    let kind = match (d.is_child, parent) {
        (true, Some(p)) => EntityKind::Child { parent: name(p)? },
        (true, None) => {
            return Err(MetaError::new(
                MetaErrorKind::Compile,
                "child DocType has no parent table",
            )
            .in_doctype(&d.name));
        }
        (false, _) => EntityKind::Document,
    };
    let (mut fields, mut children, mut indexes) = (Vec::new(), Vec::new(), Vec::new());
    for f in &d.fields {
        if f.ty.is_table() {
            children.push(ChildTableDef {
                field: name(&f.name)?,
                entity: name(f.options.as_deref().unwrap_or(""))?,
            });
            continue;
        }
        // A secret has no column of its own: its value is never part of the document. What it
        // does have is a storage path beside the row, in the same insert and transaction: the
        // engine-managed `<field>__sealed` column holds the sealed bytes, and only those
        // (ADR-068 §2, WP-SEC-03.T2).
        if f.ty == FieldType::Secret {
            fields.push(FieldDef {
                name: name(&sealed_column(&f.name))?,
                ty: Storage::Bytes,
                nullable: true,
            });
            // A lookup secret is found by value through its blind index: one more nullable
            // column, indexed so the lookup is a single equality on 33 bytes.
            if f.lookup_index() {
                fields.push(FieldDef {
                    name: name(&blind_index_column(&f.name))?,
                    ty: Storage::Bytes,
                    nullable: true,
                });
                indexes.push(blind_index_def(&d.name, &f.name, f.unique)?);
            }
            continue;
        }
        fields.push(FieldDef {
            name: name(&f.name)?,
            ty: storage_type(f)?,
            nullable: true,
        });
        if f.unique {
            indexes.push(index(&d.name, std::slice::from_ref(&f.name), true)?);
        } else if f.index {
            indexes.push(index(&d.name, std::slice::from_ref(&f.name), false)?);
        }
    }
    for ix in &d.indexes {
        indexes.push(index(&d.name, &ix.fields, ix.unique)?);
    }
    // The naming backstop (WP-SEC-12.T2): a document that is named by a `naming` pattern has
    // unique names, and the storage says so too. Otherwise the guarantee exists only in the
    // plan and in the engine's name lock, and a writer that bypasses the engine (a migration,
    // an import) could store a second document under a taken name.
    if let Some(title) = naming_title_column(d) {
        // A plain index over the same column would be a second index on one column: the
        // unique one replaces it (same column, different name prefix `ix_` versus `uq_`, so
        // the de-duplication below would not collapse the two). A unique field or a declared
        // unique single-column index already has the `uq_` name, and the de-duplication keeps
        // exactly one of them.
        let plain = index(&d.name, std::slice::from_ref(title), false)?;
        indexes.retain(|i| i.name != plain.name);
        indexes.push(index(&d.name, std::slice::from_ref(title), true)?);
    }
    let mut seen = std::collections::BTreeSet::new();
    indexes.retain(|i| seen.insert(i.name.clone()));
    Ok(EntityDef {
        name: name(&d.name)?,
        kind,
        fields,
        children,
        indexes,
        exclusions: d
            .exclusions
            .iter()
            .map(|x| exclusion(d, x))
            .collect::<Result<_, _>>()?,
        search: d.search.as_ref().map(search).transpose()?,
    })
}

fn storage_type(f: &FieldSpec) -> Result<Storage, MetaError> {
    let target = f.options.as_deref().unwrap_or("");
    Ok(match f.ty {
        FieldType::Bool => Storage::Bool,
        FieldType::Int => Storage::Int,
        FieldType::Decimal => Storage::Decimal {
            precision: f.precision.unwrap_or(DEFAULT_PRECISION),
            scale: f.scale.unwrap_or(0),
        },
        FieldType::Text => Storage::Text {
            max_chars: f.max_chars,
        },
        FieldType::LongText | FieldType::Select => Storage::Text { max_chars: None },
        FieldType::Date => Storage::Date,
        FieldType::Time => Storage::Time,
        FieldType::DateTime => Storage::DateTime,
        FieldType::Link => Storage::Link {
            target: LinkTarget::Entity(name(target)?),
            check: LinkCheck::Database,
        },
        FieldType::DynamicLink => Storage::Link {
            target: LinkTarget::Dynamic {
                entity_field: name(target)?,
            },
            check: LinkCheck::EngineOnly {
                reason: DYNAMIC_REASON.into(),
            },
        },
        FieldType::Json => Storage::Json,
        FieldType::Geo => Storage::Geo { srid: GEO_SRID },
        FieldType::Range => Storage::Range {
            of: match f.format.as_deref() {
                Some("date_range") => RangeOf::Date,
                Some("instant_range") => RangeOf::DateTime,
                _ => return Err(unknown_format(f)),
            },
        },
        FieldType::Bytes => Storage::Bytes,
        FieldType::Attach => Storage::Link {
            target: LinkTarget::Entity(name(FILE_DOCTYPE)?),
            check: LinkCheck::EngineOnly {
                reason: FILE_REASON.into(),
            },
        },
        // A table's rows are their own entity and a secret's value is sealed into its
        // `<field>__sealed` column; neither is a plain column of the row itself.
        FieldType::Table | FieldType::TableMultiselect | FieldType::Secret => {
            return Err(MetaError::new(
                MetaErrorKind::Compile,
                "table and secret fields are not stored as plain columns",
            )
            .in_field(&f.name));
        }
    })
}

/// The engine-managed infix of a secret's columns. A DocType field may not contain it
/// (`validate.rs` refuses one), so an author can never name a column the engine owns.
pub const RESERVED_INFIX: &str = "__";
/// Suffix of the column holding a secret field's `Sealed::to_bytes()`.
pub const SEALED_SUFFIX: &str = "__sealed";
/// Suffix of the column holding a lookup secret's blind index (33 bytes).
pub const BLIND_INDEX_SUFFIX: &str = "__bidx";

/// The storage column of a secret field: `<field>__sealed`, bytes, nullable.
pub fn sealed_column(field: &str) -> String {
    format!("{field}{SEALED_SUFFIX}")
}

/// The blind-index column of a lookup secret: `<field>__bidx`, bytes, nullable.
pub fn blind_index_column(field: &str) -> String {
    format!("{field}{BLIND_INDEX_SUFFIX}")
}

/// The index over a lookup secret's blind-index column: `idx_<doctype>_<field>_bidx`, unique
/// when the field is. It is a whole-value index: a blind index is only ever compared whole.
fn blind_index_def(doctype: &str, field: &str, unique: bool) -> Result<IndexDef, MetaError> {
    let full = format!("idx_{doctype}_{field}_bidx");
    Ok(IndexDef {
        name: fit_name(full)?,
        fields: vec![name(&blind_index_column(field))?],
        unique,
        method: spot_storage_contract::IndexMethod::BTree,
        opclass: None,
        include: vec![],
    })
}

/// An index named `ix_<doctype>_<fields>` (`uq_` when unique). A name over the 63-byte limit is
/// cut and given a hash of the full name, so it stays unique and stable.
fn index(doctype: &str, fields: &[String], unique: bool) -> Result<IndexDef, MetaError> {
    let prefix = if unique { "uq" } else { "ix" };
    let full = format!("{prefix}_{doctype}_{}", fields.join("_"));
    Ok(IndexDef {
        name: fit_name(full)?,
        fields: fields.iter().map(|f| name(f)).collect::<Result<_, _>>()?,
        unique,
        method: spot_storage_contract::IndexMethod::BTree,
        opclass: None,
        include: vec![],
    })
}

/// Fits an index name to the storage name limit, cutting it and appending a hash of the full
/// name when it is too long.
fn fit_name(full: String) -> Result<Name, MetaError> {
    let shown = if full.len() <= Name::MAX_LEN {
        full
    } else {
        let mut hash: u32 = 0x811c_9dc5;
        for b in full.bytes() {
            hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
        }
        // Names are ASCII, so cutting at a byte offset is safe.
        format!("{}_{hash:08x}", &full[..Name::MAX_LEN - 9])
    };
    name(&shown)
}

/// The title column whose value the storage must keep unique, when the DocType needs the naming
/// backstop: it is not a child table (rows of a parent's table carry no name of their own), it
/// declares a non-empty `naming` pattern, and its `title_field` names a column this compile
/// emits — a table is not a column, and a secret's value lives in its sealed bytes, not in a
/// column of its own name.
///
/// This is the same rule as `spot-plan`'s `indexes::naming_title_field`, kept in both crates on
/// purpose: making one depend on the other for four lines would invert the layering (the plan
/// is built from the metadata, not the other way round). `tests/naming_index.rs` pins the index
/// names against the literals the `spot-plan` unit test pins, so the two cannot drift apart.
fn naming_title_column(d: &Definition) -> Option<&String> {
    if d.is_child {
        return None;
    }
    if d.naming.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    let title = d.title_field.as_deref()?;
    d.fields
        .iter()
        .find(|f| f.name == title)
        .filter(|f| !f.ty.is_table() && f.ty != FieldType::Secret)
        .map(|f| &f.name)
}

/// The app API view of one effective DocType: the complete field description (ADR-068 §6),
/// with the warnings compiling it produced.
pub fn doctype(
    version: u64,
    d: &Definition,
    all: &[Definition],
    inheritance: &crate::derive::InheritanceMap,
) -> Result<(DocTypeDef, Vec<Warning>), MetaError> {
    let mut fields = Vec::new();
    let mut warnings = Vec::new();
    for f in &d.fields {
        let kind = match f.ty {
            FieldType::Bool => FieldKind::Bool,
            FieldType::Int => FieldKind::Int {
                format: int_format(f)?,
            },
            FieldType::Decimal => FieldKind::Decimal {
                scale: f.scale.unwrap_or(0),
                precision: f.precision.unwrap_or(DEFAULT_PRECISION),
                format: decimal_format(f)?,
            },
            FieldType::Text => FieldKind::Text {
                max_chars: f.max_chars,
                format: text_format(f)?,
            },
            FieldType::LongText => FieldKind::LongText {
                format: long_text_format(f)?,
            },
            FieldType::Select => FieldKind::Select {
                values: f.select_values(),
            },
            FieldType::Date => FieldKind::Date,
            FieldType::Time => FieldKind::Time,
            FieldType::DateTime => FieldKind::DateTime,
            FieldType::Link => FieldKind::Link {
                doctype: name(f.options.as_deref().unwrap_or(""))?,
            },
            FieldType::DynamicLink => FieldKind::DynamicLink {
                type_field: name(f.options.as_deref().unwrap_or(""))?,
            },
            FieldType::Json => FieldKind::Json {
                format: json_format(f)?,
            },
            // The app API's `FieldKind` has no `Geo` or `Range` yet (ADR-068 section 6 adds them
            // with the contract amendment). Until then the view carries both as JSON, which is
            // what they are on the wire (a GeoJSON point; a pair of bounds), and says so in a
            // warning, so a client never mistakes the approximation for the final kind.
            FieldType::Geo => {
                warnings.push(approximated(d, f));
                FieldKind::Json {
                    format: JsonFormat::Geolocation,
                }
            }
            FieldType::Range => {
                warnings.push(approximated(d, f));
                FieldKind::Json {
                    format: JsonFormat::Plain,
                }
            }
            FieldType::Bytes => FieldKind::Bytes,
            FieldType::Table => FieldKind::Table {
                doctype: name(f.options.as_deref().unwrap_or(""))?,
            },
            FieldType::Attach => FieldKind::Attach {
                format: attach_format(f)?,
            },
            FieldType::Secret => FieldKind::Secret,
            FieldType::TableMultiselect => {
                let child = f.options.as_deref().unwrap_or("");
                FieldKind::TableMultiselect {
                    doctype: name(child)?,
                    link_field: name(&child_link_field(d, f, all, child)?)?,
                }
            }
        };
        if f.ty == FieldType::Json && f.format.as_deref() == Some("geolocation") {
            warnings.push(Warning::new(
                "meta.geolocation_deprecated",
                &d.name,
                Some(&f.name),
                "the `geolocation` format of `json` is a deprecated alias: use a `geo` field with format `point`, which stores one point in a typed column the platform can index (a value that is not a single point cannot move to it)",
            ));
        }
        let mut field = AppField::new(name(&f.name)?, kind, f.required, f.allow_on_posted);
        field.label = f.label_or_name().to_owned();
        field.read_only = f.read_only;
        // The app value model has no array, so a multiple select's list default stays in the
        // definition for the engine to apply; the view carries no scalar default for it.
        if !f.multiple {
            field.default = f.default_literal().and_then(|v| default_value(f, v));
        }
        // Only the seam form reaches the app view: `lookup = true` on a secret
        // describes storage, not the field's API description, which is unchanged.
        field.lookup = f.lookup_spec().map(lookup_view);
        field.hints = hints(f)?;
        field.multiple = f.multiple;
        if let Some(control) = field.hints.control
            && !control_applies(control, f)
        {
            warnings.push(Warning::new(
                "meta.control_not_applicable",
                &d.name,
                Some(&f.name),
                format!(
                    "control `{}` cannot apply to a {:?} field{}; the hint is kept and the client falls back to its default control",
                    control.as_str(),
                    f.ty,
                    if f.multiple && f.ty == FieldType::Select {
                        " that is multiple"
                    } else {
                        ""
                    }
                ),
            ));
        }
        field.security = field_security(f, &f.name)?;
        if f.declassified.is_none()
            && let Some(groups) = inheritance.get(&crate::derive::FieldRef::new(&d.name, &f.name))
        {
            for g in groups {
                field
                    .security
                    .inherited_groups
                    .push(GroupName::new(g).map_err(|e| {
                        MetaError::new(crate::error::MetaErrorKind::BadProperty, e.to_string())
                            .in_doctype(&d.name)
                            .in_field(&f.name)
                    })?);
            }
        }
        fields.push(field);
    }
    let mut view = DocTypeDef::new(
        name(&d.name)?,
        d.label_or_name().to_owned(),
        d.is_posted,
        d.is_child,
        d.title_field.as_deref().map(name).transpose()?,
        fields,
    );
    view.version = version;

    // Compile protection groups
    for group_spec in &d.protection_groups {
        view.protection_groups.push(protection_group(group_spec)?);
    }

    Ok((view, warnings))
}

/// The warning that a `geo` or `range` field is shown to the app API as JSON until `FieldKind`
/// has the two kinds.
fn approximated(d: &Definition, f: &FieldSpec) -> Warning {
    Warning::new(
        "meta.app_kind_approximated",
        &d.name,
        Some(&f.name),
        format!(
            "the app API has no field kind for {:?} yet; the view describes it as JSON",
            f.ty
        ),
    )
}

/// The presentation hints of a field, compiled into the app view. An unknown control name is a
/// bug here: validation refuses it before a build, so a bad one is loud rather than dropped.
fn hints(f: &FieldSpec) -> Result<FieldHints, MetaError> {
    let mut h = FieldHints::default();
    h.description.clone_from(&f.description);
    h.hidden = f.hidden;
    h.in_list_view = f.in_list_view;
    h.in_standard_filter = f.in_standard_filter;
    h.no_copy = f.no_copy;
    h.control = match f.control.as_deref() {
        None => None,
        Some(name) => Some(ControlHint::parse(name).ok_or_else(|| {
            MetaError::new(
                MetaErrorKind::Compile,
                format!(
                    "unknown control `{name}`; use one of: {}",
                    crate::CONTROL_HINTS.join(", ")
                ),
            )
            .in_field(&f.name)
        })?),
    };
    // permlevel is deprecated (ADR-068 A1): always 0 in compiled output.
    h.permlevel = 0;
    Ok(h)
}

/// Whether a control hint can apply to a field's kind (ADR-068 amendment A2). A hint that
/// cannot is kept and reported as a warning: the client falls back to its own control.
fn control_applies(control: ControlHint, f: &FieldSpec) -> bool {
    match control {
        ControlHint::Switch | ControlHint::Checkbox => f.ty == FieldType::Bool,
        ControlHint::Dropdown => f.ty == FieldType::Select,
        ControlHint::Radio | ControlHint::Segmented => f.ty == FieldType::Select && !f.multiple,
        ControlHint::Autocomplete => {
            matches!(f.ty, FieldType::Link | FieldType::DynamicLink)
        }
        ControlHint::Chips => f.ty == FieldType::Select && f.multiple,
        ControlHint::Textarea => matches!(f.ty, FieldType::Text | FieldType::LongText),
        // The set grows additively; a hint this build does not know is never warned about.
        _ => true,
    }
}

/// The definition's default, parsed by the field's type into the ADR-055 value model, with the
/// grammar `validate.rs` checks by (`parse` below). The definition validator has already
/// checked every default before anything compiles, so an unparseable one can only be a
/// programming bug; it then compiles to no default rather than failing the rebuild (ADR-05).
fn default_value(f: &FieldSpec, v: &str) -> Option<Value> {
    match f.ty {
        FieldType::Bool => Some(Value::Bool(v == "true")),
        FieldType::Int => v.parse::<i64>().ok().map(Value::Int),
        FieldType::Decimal => parse::decimal(v).map(Value::Decimal),
        FieldType::Text | FieldType::LongText | FieldType::Select => {
            Some(Value::Text(v.to_owned()))
        }
        FieldType::Date => parse::date(v).map(Value::Date),
        FieldType::Time => parse::time(v).map(Value::Time),
        FieldType::DateTime => parse::datetime(v).map(Value::DateTime),
        // A secret's value, or any hint of it, is never part of the app view (ADR-068 §2).
        // A range default stays in the definition for the engine to apply (a validated
        // `[start,end)` text): the app view describes the field as JSON for now, and a scalar
        // default of another kind would contradict it.
        FieldType::Link
        | FieldType::DynamicLink
        | FieldType::Json
        | FieldType::Bytes
        | FieldType::Geo
        | FieldType::Range
        | FieldType::Table
        | FieldType::TableMultiselect
        | FieldType::Attach
        | FieldType::Secret => None,
    }
}

/// The error for a format the validator should have refused, so a bug is loud, not silent.
fn unknown_format(f: &FieldSpec) -> MetaError {
    MetaError::new(
        MetaErrorKind::Compile,
        format!("unknown format `{}`", f.format.as_deref().unwrap_or("")),
    )
    .in_field(&f.name)
}

fn text_format(f: &FieldSpec) -> Result<TextFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("plain") => TextFormat::Plain,
        Some("email") => TextFormat::Email,
        Some("phone") => TextFormat::Phone,
        Some("url") => TextFormat::Url,
        Some("color") => TextFormat::Color,
        Some("barcode") => TextFormat::Barcode {
            symbology: f.symbology.clone(),
        },
        Some(_) => return Err(unknown_format(f)),
    })
}

fn long_text_format(f: &FieldSpec) -> Result<LongTextFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("plain") => LongTextFormat::Plain,
        Some("markdown") => LongTextFormat::Markdown,
        Some("html") => LongTextFormat::Html,
        Some("code") => LongTextFormat::Code {
            syntax: f.syntax.clone().unwrap_or_default(),
        },
        Some(_) => return Err(unknown_format(f)),
    })
}

fn int_format(f: &FieldSpec) -> Result<IntFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("plain") => IntFormat::Plain,
        Some("duration") => IntFormat::Duration,
        Some("rating") => IntFormat::Rating {
            max: f.max.unwrap_or(DEFAULT_RATING_MAX),
        },
        Some(_) => return Err(unknown_format(f)),
    })
}

fn decimal_format(f: &FieldSpec) -> Result<DecimalFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("plain") => DecimalFormat::Plain,
        Some("currency") => DecimalFormat::Currency {
            currency_field: name(f.currency_field.as_deref().unwrap_or(""))?,
        },
        Some("percent") => DecimalFormat::Percent,
        Some(_) => return Err(unknown_format(f)),
    })
}

fn json_format(f: &FieldSpec) -> Result<JsonFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("plain") => JsonFormat::Plain,
        Some("geolocation") => JsonFormat::Geolocation,
        Some(_) => return Err(unknown_format(f)),
    })
}

fn attach_format(f: &FieldSpec) -> Result<AttachFormat, MetaError> {
    Ok(match f.format.as_deref() {
        None | Some("file") => AttachFormat::File,
        Some("image") => AttachFormat::Image,
        Some("signature") => AttachFormat::Signature,
        Some(_) => return Err(unknown_format(f)),
    })
}

/// The single link field of a `table_multiselect`'s child DocType (the validator has checked
/// there is exactly one).
fn child_link_field(
    parent: &Definition,
    f: &FieldSpec,
    all: &[Definition],
    child: &str,
) -> Result<String, MetaError> {
    all.iter()
        .find(|c| c.name == child)
        .and_then(|c| c.fields.iter().find(|g| g.ty == FieldType::Link))
        .map(|g| g.name.clone())
        .ok_or_else(|| {
            MetaError::new(
                MetaErrorKind::Compile,
                format!("table_multiselect child `{child}` has no link field"),
            )
            .in_doctype(&parent.name)
            .in_field(&f.name)
        })
}

/// The date and time grammar of a definition's default, the one `validate.rs` checks by:
/// `YYYY-MM-DD`, `HH:MM[:SS]` with an optional fraction, and an instant with `Z` or a numeric
/// offset. The validators agree by construction, so a default that survived validation parses
/// here too.
pub(crate) mod parse {
    use std::str::FromStr;
    use std::time::Duration;

    use spot_app_api::prelude::{Date, Decimal, Month, OffsetDateTime, PrimitiveDateTime, Time};

    /// Parses one decimal, with the same rejection rule as `validate.rs`.
    pub fn decimal(s: &str) -> Option<Decimal> {
        Decimal::from_str(s).ok()
    }

    fn digits(s: &str, len: usize) -> Option<u32> {
        (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    }

    /// `YYYY-MM-DD`.
    pub fn date(s: &str) -> Option<Date> {
        let mut p = s.split('-');
        let (Some(y), Some(m), Some(d), None) = (p.next(), p.next(), p.next(), p.next()) else {
            return None;
        };
        let (y, m, d) = (digits(y, 4)?, digits(m, 2)?, digits(d, 2)?);
        let month = Month::try_from(u8::try_from(m).ok()?).ok()?;
        Date::from_calendar_date(y.try_into().ok()?, month, u8::try_from(d).ok()?).ok()
    }

    /// `HH:MM[:SS]` with an optional fraction of up to nine digits.
    pub fn time(s: &str) -> Option<Time> {
        let (main, fraction) = match s.split_once('.') {
            Some((m, f)) => (m, Some(f)),
            None => (s, None),
        };
        if fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        let parts: Vec<&str> = main.split(':').collect();
        let (h, m, sec) = match parts.as_slice() {
            [h, m] => (*h, *m, "00"),
            [h, m, s] => (*h, *m, *s),
            _ => return None,
        };
        let (h, m, sec) = (digits(h, 2)?, digits(m, 2)?, digits(sec, 2)?);
        if !(h < 24 && m < 60 && sec < 60) {
            return None;
        }
        let nanos = match fraction {
            None => 0,
            Some(f) if f.len() <= 9 => format!("{f:0<9}").parse::<u32>().ok()?,
            Some(_) => return None,
        };
        Time::from_hms_nano(
            u8::try_from(h).ok()?,
            u8::try_from(m).ok()?,
            u8::try_from(sec).ok()?,
            nanos,
        )
        .ok()
    }

    /// `YYYY-MM-DDTHH:MM:SS` with `Z` or a numeric offset; the instant comes back in UTC.
    pub fn datetime(s: &str) -> Option<OffsetDateTime> {
        let (day_part, rest) = s.split_once('T')?;
        let day = date(day_part)?;
        if let Some(t) = rest.strip_suffix('Z') {
            return Some(PrimitiveDateTime::new(day, time(t)?).assume_utc());
        }
        let at = rest.rfind(['+', '-'])?;
        let (t, offset) = rest.split_at(at);
        let (h, m) = offset[1..].split_once(':')?;
        let (h, m) = (digits(h, 2)?, digits(m, 2)?);
        if !(h < 24 && m < 60) {
            return None;
        }
        let off = Duration::from_secs(u64::from(h) * 3600 + u64::from(m) * 60);
        let local = PrimitiveDateTime::new(day, time(t)?).assume_utc();
        Some(if offset.starts_with('+') {
            local - off
        } else {
            local + off
        })
    }
}

/// Compiles a MaskSpec to MaskFormat, or returns None if no masking is set.
fn mask_format(
    spec: &Option<crate::definition::MaskSpec>,
) -> Result<Option<MaskFormat>, MetaError> {
    match spec {
        None => Ok(None),
        Some(m) => {
            if m.placeholder {
                Ok(Some(MaskFormat::Placeholder))
            } else if let Some(n) = m.last_chars {
                Ok(Some(MaskFormat::LastChars(n)))
            } else {
                // Default to placeholder if neither is set
                Ok(Some(MaskFormat::Placeholder))
            }
        }
    }
}

/// Compiles security properties of a field.
fn field_security(f: &FieldSpec, field_name: &str) -> Result<FieldSecurity, MetaError> {
    // Start with the default, then set fields (FieldSecurity is non-exhaustive)
    let mut sec: FieldSecurity = Default::default();

    if let Some(group) = &f.protection_group {
        sec.protection_group = Some(GroupName::new(group).map_err(|e| {
            MetaError::new(crate::error::MetaErrorKind::BadProperty, e.to_string())
                .in_field(field_name)
        })?);
    }

    for tag_str in &f.classification {
        let tag = Tag::new(tag_str).map_err(|e| {
            MetaError::new(crate::error::MetaErrorKind::BadProperty, e.to_string())
                .in_field(field_name)
        })?;
        sec.classification.push(tag);
    }

    sec.masking = mask_format(&f.masking)?;
    sec.declassified = f.declassified.clone();

    Ok(sec)
}

/// Compiles a protection group from the definition to the app view.
fn protection_group(
    spec: &crate::definition::ProtectionGroupSpec,
) -> Result<ProtectionGroupDef, MetaError> {
    let name = GroupName::new(&spec.name)
        .map_err(|e| MetaError::new(crate::error::MetaErrorKind::BadProperty, e.to_string()))?;

    let sync_class_hint = match spec.sync_class_hint.as_deref() {
        None | Some("everywhere") => SyncClassHint::Everywhere,
        Some("encrypted") => SyncClassHint::Encrypted,
        Some("sealed") => SyncClassHint::Sealed,
        Some("home_only") => SyncClassHint::HomeOnly,
        Some(hint) => {
            return Err(MetaError::new(
                crate::error::MetaErrorKind::BadProperty,
                format!(
                    "unknown sync class hint `{hint}`; must be one of: everywhere, \
                     encrypted, sealed, home_only"
                ),
            ));
        }
    };

    let mut group = ProtectionGroupDef::new(name, spec.label_or_name().to_owned());
    group.trace = spec.trace;
    group.sync_class_hint = sync_class_hint;
    Ok(group)
}

/// The app API form of a field's resolved lookup (definition, then site override). Nothing is
/// evaluated: a rule (`when`) never reaches here because validation refuses it until the shared
/// expression language exists (WP-UI-11), so only the seam and the fixed parameters carry over.
fn lookup_view(spec: &LookupSpec) -> AppLookupSpec {
    // The contract type is `#[non_exhaustive]`: start from its default and set what is ours.
    let mut view = AppLookupSpec::default();
    view.seam.clone_from(&spec.seam);
    view.version = LOOKUP_VERSION;
    view.params.clone_from(&spec.params);
    view
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod reverse_index_tests {
    use super::*;

    fn defs() -> Vec<Definition> {
        let mut edge = Definition::new("edge");
        edge.fields = vec![
            link("owner", "node"),
            FieldSpec::new("note", FieldType::Text),
        ];
        let mut node = Definition::new("node");
        node.fields = vec![FieldSpec::new("title", FieldType::Text)];
        vec![edge, node]
    }

    fn en() -> Collation {
        Collation::new("en").expect("collation")
    }

    fn link(name: &str, target: &str) -> FieldSpec {
        let mut f = FieldSpec::new(name, FieldType::Link);
        f.options = Some(target.to_owned());
        f
    }

    fn ri(table: &str, column: &str, name: &str) -> ReverseIndex {
        ReverseIndex {
            table: table.into(),
            column: column.into(),
            name: name.into(),
        }
    }

    fn index_names(cat: &Catalog, table: &str) -> Vec<String> {
        cat.entity(&Name::new(table).expect("name"))
            .expect("entity")
            .indexes
            .iter()
            .map(|i| i.name.as_str().to_owned())
            .collect()
    }

    #[test]
    fn an_empty_list_is_byte_identical_to_catalog() {
        let plain = catalog(1, &defs(), en()).expect("plain");
        let with = catalog_with_reverse_indexes(1, &defs(), en(), &[]).expect("with");
        assert_eq!(plain, with);
        assert!(index_names(&plain, "edge").is_empty());
    }

    #[test]
    fn an_entry_becomes_one_plain_single_column_btree_index() {
        let cat = catalog_with_reverse_indexes(
            1,
            &defs(),
            en(),
            &[ri("edge", "owner", "gx_edge_owner_rev")],
        )
        .expect("catalog");
        let e = cat
            .entity(&Name::new("edge").expect("name"))
            .expect("entity");
        assert_eq!(e.indexes.len(), 1);
        let i = &e.indexes[0];
        assert_eq!(i.name.as_str(), "gx_edge_owner_rev");
        assert_eq!(i.fields.len(), 1);
        assert_eq!(i.fields[0].as_str(), "owner");
        assert!(!i.unique);
        assert_eq!(i.method, spot_storage_contract::IndexMethod::BTree);
        assert!(index_names(&cat, "node").is_empty());
    }

    #[test]
    fn the_same_entry_twice_is_added_once() {
        let r = ri("edge", "owner", "gx_edge_owner_rev");
        let cat = catalog_with_reverse_indexes(1, &defs(), en(), &[r.clone(), r]).expect("catalog");
        assert_eq!(index_names(&cat, "edge"), ["gx_edge_owner_rev"]);
    }

    #[test]
    fn an_index_true_field_keeps_its_own_index_and_the_entry_adds_no_third() {
        let mut d = defs();
        d[0].fields[0].index = true;
        let r = ri("edge", "owner", "gx_edge_owner_rev");
        let cat = catalog_with_reverse_indexes(1, &d, en(), &[r.clone(), r]).expect("catalog");
        assert_eq!(
            index_names(&cat, "edge"),
            ["ix_edge_owner", "gx_edge_owner_rev"]
        );
    }

    fn refused(reverse: &[ReverseIndex]) -> MetaError {
        catalog_with_reverse_indexes(1, &defs(), en(), reverse).expect_err("must refuse")
    }

    #[test]
    fn unknown_doctype_non_link_field_and_name_clashes_are_compile_errors() {
        for bad in [
            ri("ghost", "owner", "gx_a"),
            ri("edge", "nope", "gx_a"),
            ri("edge", "note", "gx_a"),
        ] {
            let e = refused(&[bad]);
            assert_eq!(e.kind, MetaErrorKind::Compile);
        }
        // A name an index of the definitions already produces.
        let mut d = defs();
        d[0].fields[0].index = true;
        let e = catalog_with_reverse_indexes(1, &d, en(), &[ri("edge", "owner", "ix_edge_owner")])
            .expect_err("name clash");
        assert_eq!(e.kind, MetaErrorKind::Compile);
        // One name on two columns.
        let mut d = defs();
        d[0].fields.push(link("other", "node"));
        let e = catalog_with_reverse_indexes(
            1,
            &d,
            en(),
            &[ri("edge", "owner", "gx_a"), ri("edge", "other", "gx_a")],
        )
        .expect_err("two columns");
        assert_eq!(e.kind, MetaErrorKind::Compile);
    }
}
