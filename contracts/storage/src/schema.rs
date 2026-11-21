//! The catalog: what entities exist and how their records are shaped.
//!
//! The metadata layer (WP-MET-01) compiles DocType definitions into a [`Catalog`]; storage only
//! ever sees this compiled form. Each transaction runs against one catalog snapshot, so a
//! metadata change never reaches a transaction that is already running (ADR-05).

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{ErrorKind, StorageError, StorageResult};
use crate::id::Name;
use crate::limits::{GEO_SRID, MAX_SEARCH_FIELDS};
use crate::value::Value;

/// System columns every backend maintains. They cannot be declared as fields, but queries may
/// read them by these names.
pub mod system {
    /// Record id (all entities).
    pub const ID: &str = "id";
    /// Document version (document entities).
    pub const VERSION: &str = "version";
    /// Id of the parent document (child entities).
    pub const PARENT: &str = "parent";
    /// Name of the parent's child-table field (child entities).
    pub const PARENT_FIELD: &str = "parent_field";
    /// 1-based position within the parent's child table (child entities).
    pub const IDX: &str = "idx";

    /// All reserved names.
    pub const ALL: [&str; 5] = [ID, VERSION, PARENT, PARENT_FIELD, IDX];
}

/// A consistent set of entity definitions, stamped with the metadata version it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    /// Metadata version this snapshot was compiled from.
    pub version: u64,
    /// The tenant's text sort order.
    pub collation: Collation,
    entities: BTreeMap<Name, EntityDef>,
}

impl Catalog {
    /// Builds and validates a catalog.
    pub fn new(
        version: u64,
        collation: Collation,
        entities: Vec<EntityDef>,
    ) -> StorageResult<Self> {
        let mut map = BTreeMap::new();
        for e in entities {
            let name = e.name.clone();
            if map.insert(name.clone(), e).is_some() {
                return Err(schema_err(format!("entity `{name}` is defined twice")));
            }
        }
        let catalog = Self {
            version,
            collation,
            entities: map,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    /// The entity called `name`.
    pub fn entity(&self, name: &Name) -> StorageResult<&EntityDef> {
        self.entities.get(name).ok_or_else(|| {
            StorageError::new(
                ErrorKind::UnknownEntity,
                format!("entity `{name}` is not in the catalog"),
            )
            .with_entity(name)
        })
    }

    /// All entities in name order.
    pub fn entities(&self) -> impl Iterator<Item = &EntityDef> {
        self.entities.values()
    }

    fn validate(&self) -> StorageResult<()> {
        for e in self.entities.values() {
            let mut seen = BTreeSet::new();
            for f in &e.fields {
                if system::ALL.contains(&f.name.as_str()) {
                    return Err(schema_err(format!(
                        "`{}.{}` uses a reserved system column name",
                        e.name, f.name
                    )));
                }
                if !seen.insert(f.name.clone()) {
                    return Err(schema_err(format!(
                        "field `{}.{}` is defined twice",
                        e.name, f.name
                    )));
                }
                if let FieldType::Link { check, .. } = &f.ty {
                    match check {
                        LinkCheck::EngineOnly { reason } if reason.trim().is_empty() => {
                            return Err(schema_err(format!(
                                "`{}.{}` opts out of database link checks without a reason",
                                e.name, f.name
                            )));
                        }
                        LinkCheck::Database
                            if matches!(
                                f.ty,
                                FieldType::Link {
                                    target: LinkTarget::Dynamic { .. },
                                    ..
                                }
                            ) =>
                        {
                            return Err(schema_err(format!(
                                "dynamic link `{}.{}` cannot be checked by the database; use EngineOnly",
                                e.name, f.name
                            )));
                        }
                        _ => {}
                    }
                }
                if let FieldType::Link {
                    target: LinkTarget::Entity(t),
                    ..
                } = &f.ty
                {
                    self.entity(t).map_err(|_| {
                        schema_err(format!(
                            "`{}.{}` links to unknown entity `{t}`",
                            e.name, f.name
                        ))
                    })?;
                }
                if let FieldType::Link {
                    target: LinkTarget::Dynamic { entity_field },
                    ..
                } = &f.ty
                {
                    let ok = e
                        .fields
                        .iter()
                        .any(|g| &g.name == entity_field && matches!(g.ty, FieldType::Text { .. }));
                    if !ok {
                        return Err(schema_err(format!(
                            "dynamic link `{}.{}` needs a text field `{entity_field}` naming the target entity",
                            e.name, f.name
                        )));
                    }
                }
                if let FieldType::Geo { srid } = f.ty
                    && srid != GEO_SRID
                {
                    return Err(schema_err(format!(
                        "`{}.{}` uses SRID {srid}; only {GEO_SRID} (WGS-84) is accepted",
                        e.name, f.name
                    )));
                }
                if let FieldType::Decimal { precision, scale } = f.ty
                    && (precision == 0 || precision > 38 || scale > precision)
                {
                    return Err(schema_err(format!(
                        "`{}.{}` has invalid decimal precision {precision} / scale {scale}",
                        e.name, f.name
                    )));
                }
            }
            for c in &e.children {
                if seen.contains(&c.field) || !seen.insert(c.field.clone()) {
                    return Err(schema_err(format!(
                        "child table `{}.{}` clashes with another field",
                        e.name, c.field
                    )));
                }
                let child = self.entity(&c.entity).map_err(|_| {
                    schema_err(format!(
                        "`{}.{}` uses unknown child entity `{}`",
                        e.name, c.field, c.entity
                    ))
                })?;
                if child.kind
                    != (EntityKind::Child {
                        parent: e.name.clone(),
                    })
                {
                    return Err(schema_err(format!(
                        "`{}` must be declared as a child of `{}`",
                        c.entity, e.name
                    )));
                }
            }
            if matches!(e.kind, EntityKind::Child { .. }) && !e.children.is_empty() {
                return Err(schema_err(format!(
                    "child entity `{}` cannot have child tables",
                    e.name
                )));
            }
            if let EntityKind::Child { parent } = &e.kind {
                let p = self.entity(parent)?;
                if !p.children.iter().any(|c| c.entity == e.name) {
                    return Err(schema_err(format!(
                        "`{parent}` has no child table using `{}`",
                        e.name
                    )));
                }
            }
            for ix in &e.indexes {
                if ix.fields.is_empty() {
                    return Err(schema_err(format!("index `{}` has no fields", ix.name)));
                }
                for f in ix.fields.iter().chain(&ix.include) {
                    if e.field(f).is_err() && !system::ALL.contains(&f.as_str()) {
                        return Err(schema_err(format!(
                            "index `{}` uses unknown field `{f}`",
                            ix.name
                        )));
                    }
                }
                validate_index_method(e, ix)?;
            }
            validate_exclusions(e)?;
            validate_search(e)?;
        }
        Ok(())
    }
}

/// Whether an entity holds documents or the child rows of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityKind {
    /// A document: loaded, saved, versioned and locked as a unit with its child rows.
    Document,
    /// Rows of a child table; they exist only inside a document of the `parent` entity.
    Child {
        /// The parent document entity.
        parent: Name,
    },
}

/// One entity: a table's worth of records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityDef {
    /// Storage name.
    pub name: Name,
    /// Document or child.
    pub kind: EntityKind,
    /// Declared fields (system columns are implicit).
    pub fields: Vec<FieldDef>,
    /// Child tables of a document entity.
    pub children: Vec<ChildTableDef>,
    /// Indexes, including unique ones.
    pub indexes: Vec<IndexDef>,
    /// Overlap exclusions over range fields (ADR-087 section 5).
    pub exclusions: Vec<ExclusionDef>,
    /// The search projection this entity opts into (ADR-087 section 7); `None` for no search.
    pub search: Option<SearchDef>,
}

impl EntityDef {
    /// The declared field called `name`.
    pub fn field(&self, name: &Name) -> StorageResult<&FieldDef> {
        self.fields.iter().find(|f| &f.name == name).ok_or_else(|| {
            StorageError::new(
                ErrorKind::UnknownField,
                format!("`{}` has no field `{name}`", self.name),
            )
            .with_entity(&self.name)
            .with_field(name)
        })
    }

    /// The child table stored under `field`.
    pub fn child_table(&self, field: &Name) -> StorageResult<&ChildTableDef> {
        self.children
            .iter()
            .find(|c| &c.field == field)
            .ok_or_else(|| {
                StorageError::new(
                    ErrorKind::UnknownField,
                    format!("`{}` has no child table `{field}`", self.name),
                )
                .with_entity(&self.name)
                .with_field(field)
            })
    }
}

/// A declared field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    /// Storage name.
    pub name: Name,
    /// Storage type.
    pub ty: FieldType,
    /// Whether `Value::Null` is allowed.
    pub nullable: bool,
}

/// Storage types. Backends map each to one native column type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldType {
    /// Boolean.
    Bool,
    /// 64-bit integer.
    Int,
    /// Exact decimal with at most `precision` digits, `scale` of them after the point.
    Decimal {
        /// Total digits, 1 to 38.
        precision: u8,
        /// Digits after the point.
        scale: u8,
    },
    /// Text, optionally limited to `max_chars` characters.
    Text {
        /// Maximum length in characters.
        max_chars: Option<u32>,
    },
    /// Calendar date.
    Date,
    /// Time of day.
    Time,
    /// Instant (UTC).
    DateTime,
    /// A link: holds a [`RecordId`](crate::RecordId); a graph edge (ADR-50).
    Link {
        /// Where the link points.
        target: LinkTarget,
        /// Who guarantees the target exists.
        check: LinkCheck,
    },
    /// JSON document.
    Json,
    /// Raw bytes.
    Bytes,
    /// A WGS-84 point (ADR-087 section 4). `srid` is `4326`, the only value accepted in v1.
    Geo {
        /// The spatial reference system.
        srid: u32,
    },
    /// A half-open range of `of` values (ADR-087 section 5).
    Range {
        /// What the range is a range of.
        of: RangeOf,
    },
}

/// What a [`FieldType::Range`] is a range of: integer, decimal, date or instant payloads only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOf {
    /// 64-bit integers.
    Int,
    /// Exact decimals.
    Decimal,
    /// Calendar dates (`daterange`).
    Date,
    /// Instants (`tstzrange`).
    DateTime,
}

impl FieldType {
    /// Checks that `value` can be stored in a field of this type with this nullability.
    pub fn check(&self, value: &Value, nullable: bool) -> Result<(), String> {
        if value.is_null() {
            return if nullable {
                Ok(())
            } else {
                Err("a value is required".into())
            };
        }
        if !value.fits(self) {
            return Err(format!("expected {self:?}, got {value:?}"));
        }
        match (self, value) {
            (FieldType::Decimal { precision, scale }, Value::Decimal(d)) => {
                if d.scale() > u32::from(*scale) && d.normalize().scale() > u32::from(*scale) {
                    return Err(format!(
                        "{d} has more than {scale} decimal places; round it before storing (ADR-12)"
                    ));
                }
                let int_digits = d.trunc().abs().to_string().trim_start_matches('0').len();
                if int_digits > usize::from(precision - scale) {
                    return Err(format!(
                        "{d} does not fit precision {precision}, scale {scale}"
                    ));
                }
            }
            (
                FieldType::Text {
                    max_chars: Some(max),
                },
                Value::Text(s),
            ) if s.chars().count() > *max as usize => {
                return Err(format!("text is longer than {max} characters"));
            }
            (FieldType::Geo { .. }, Value::GeoPoint(p)) => p.validate()?,
            (FieldType::Range { .. }, Value::Range(r)) => r.validate()?,
            _ => {}
        }
        Ok(())
    }
}

/// The target of a link field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// Always a record of this entity.
    Entity(Name),
    /// The entity is named per record by the text field `entity_field` of the same entity.
    Dynamic {
        /// The field holding the target entity's storage name.
        entity_field: Name,
    },
}

/// Who guarantees that a link points at an existing record (ADR-053).
///
/// The document engine always owns what a link *means* (the target is active, posted, allowed
/// for this company). This setting only decides whether storage also guarantees that the
/// target *exists*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkCheck {
    /// Storage guarantees it (a deferred foreign key on PostgreSQL): a link to a missing record,
    /// or deleting a record that is still linked, fails with `ReferenceViolation` by commit at
    /// the latest. The default for every link with a fixed target entity.
    Database,
    /// Only the document engine checks it. Required for dynamic links; allowed for fixed links
    /// only with a stated reason, for example a hot link whose targets are never hard-deleted.
    EngineOnly {
        /// Why storage does not check this link.
        reason: String,
    },
}

/// The tenant's text sort order: a Unicode collation for a locale, such as `en`, `ur` or
/// `und` (the language-neutral root order).
///
/// Sorting, range comparisons (`<`, `>`) and `Min`/`Max` on text follow this collation, with
/// ties broken by Unicode code point, so the order is total. Equality, grouping, `In`, text
/// matching and unique indexes stay exact: "Ali" and "ali" are different keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Collation(String);

impl Collation {
    /// Validates a locale: a 2–3 letter lowercase language code or `und`, optionally followed
    /// by `-` subtags of 2–8 letters or digits (for example `en`, `ur-PK`, `ar`).
    pub fn new(locale: &str) -> StorageResult<Self> {
        let mut parts = locale.split('-');
        let lang_ok = parts.next().is_some_and(|l| {
            (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_lowercase())
        });
        let rest_ok = parts
            .all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()));
        if !(lang_ok && rest_ok) {
            return Err(schema_err(format!(
                "`{locale}` is not a valid collation locale"
            )));
        }
        Ok(Self(locale.to_owned()))
    }

    /// The locale as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A child table of a document entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTableDef {
    /// The field name the table appears under in the document.
    pub field: Name,
    /// The child entity holding its rows.
    pub entity: Name,
}

/// An index. Every filter on a transactional path must be covered by one (ADR-45).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDef {
    /// Storage name, unique per tenant.
    pub name: Name,
    /// Indexed fields in order; system columns allowed.
    pub fields: Vec<Name>,
    /// Whether keys must be unique. Rows with a null in any key field never conflict.
    pub unique: bool,
    /// The index method. `BTree` is the ordinary index.
    pub method: IndexMethod,
    /// An optional operator class from the closed set in [`OpClass`].
    pub opclass: Option<OpClass>,
    /// Extra fields stored in the index so queries can be answered from it; not part of the
    /// key and not allowed on a unique index of another method.
    pub include: Vec<Name>,
}

impl IndexDef {
    /// A plain B-tree index over `fields`.
    pub fn btree(name: Name, fields: Vec<Name>, unique: bool) -> Self {
        Self {
            name,
            fields,
            unique,
            method: IndexMethod::BTree,
            opclass: None,
            include: vec![],
        }
    }
}

/// The index methods (ADR-087 section 8). The set is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMethod {
    /// The ordinary B-tree.
    BTree,
    /// GiST: spatial and range indexes, and the exclusion constraint's own index.
    Gist,
    /// GIN: the search projection and trigram indexes.
    Gin,
}

/// The operator classes an index may name (ADR-087 sections 5 and 7). The set is closed: an app
/// cannot name an arbitrary class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpClass {
    /// `gist_trgm_ops` (`pg_trgm`), on a GiST index over text.
    GistTrgm,
    /// `gin_trgm_ops` (`pg_trgm`), on a GIN index over text.
    GinTrgm,
    /// The `btree_gist` equality classes, for scalar scope columns in a GiST index.
    GistScalar,
}

/// An overlap exclusion (ADR-087 section 5): no two records may hold overlapping `field`
/// ranges within the same `scope`. A null range, or a null scope field, never conflicts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionDef {
    /// Storage name of the constraint, unique within the entity's indexes and exclusions.
    pub name: Name,
    /// The range field of this entity.
    pub field: Name,
    /// Equality columns that scope the exclusion (for example the property and unit).
    pub scope: Vec<Name>,
    /// Plain-language explanation shown when a write conflicts.
    pub message: String,
}

/// What an entity declares searchable (ADR-087 section 7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchDef {
    /// The default language of the content; `None` is the tenant collation's language.
    pub language: Option<String>,
    /// The searchable fields, each text, with its weight. At most `MAX_SEARCH_FIELDS`.
    pub fields: Vec<SearchField>,
    /// Fields the caller may count by (facets).
    pub facets: Vec<Name>,
    /// Typo tolerance (trigram similarity); a backend without it refuses at `prepare`.
    pub typo_tolerance: bool,
}

/// One searchable field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchField {
    /// The text field.
    pub name: Name,
    /// Its relevance weight.
    pub weight: SearchWeight,
}

/// The four relevance weights (the `setweight` labels); nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchWeight {
    /// Highest.
    A,
    /// High.
    B,
    /// Normal.
    C,
    /// Lowest.
    D,
}

/// The type of a declared field (system columns are plain and never special).
fn declared_type<'e>(e: &'e EntityDef, name: &Name) -> Option<&'e FieldType> {
    e.fields.iter().find(|f| &f.name == name).map(|f| &f.ty)
}

fn validate_index_method(e: &EntityDef, ix: &IndexDef) -> StorageResult<()> {
    let ty = |n: &Name| declared_type(e, n);
    let is_text = |n: &Name| matches!(ty(n), Some(FieldType::Text { .. }));
    if ix.unique && ix.method != IndexMethod::BTree {
        return Err(schema_err(format!(
            "index `{}` is unique, which only a B-tree index can be; use an exclusion for ranges",
            ix.name
        )));
    }
    if !ix.include.is_empty() && ix.method == IndexMethod::Gin {
        return Err(schema_err(format!(
            "index `{}` is a GIN index, which cannot include extra columns",
            ix.name
        )));
    }
    let has_spatial = ix
        .fields
        .iter()
        .any(|f| matches!(ty(f), Some(FieldType::Geo { .. } | FieldType::Range { .. })));
    if has_spatial && ix.method != IndexMethod::Gist {
        return Err(schema_err(format!(
            "index `{}` covers a geo or range field, which needs a GiST index",
            ix.name
        )));
    }
    match ix.opclass {
        None => {}
        Some(OpClass::GistTrgm) | Some(OpClass::GinTrgm) => {
            let want = if ix.opclass == Some(OpClass::GistTrgm) {
                IndexMethod::Gist
            } else {
                IndexMethod::Gin
            };
            if ix.method != want || ix.fields.len() != 1 || !is_text(&ix.fields[0]) {
                return Err(schema_err(format!(
                    "index `{}`: a trigram operator class needs a {want:?} index over one text field",
                    ix.name
                )));
            }
        }
        Some(OpClass::GistScalar) if ix.method != IndexMethod::Gist => {
            return Err(schema_err(format!(
                "index `{}`: the scalar GiST operator class needs a GiST index",
                ix.name
            )));
        }
        Some(OpClass::GistScalar) => {}
    }
    Ok(())
}

fn validate_exclusions(e: &EntityDef) -> StorageResult<()> {
    let mut names = BTreeSet::new();
    for x in &e.exclusions {
        if e.indexes.iter().any(|i| i.name == x.name) || !names.insert(&x.name) {
            return Err(schema_err(format!(
                "exclusion `{}` on `{}` reuses a name",
                x.name, e.name
            )));
        }
        if !matches!(declared_type(e, &x.field), Some(FieldType::Range { .. })) {
            return Err(schema_err(format!(
                "exclusion `{}` on `{}` needs a range field; `{}` is not one",
                x.name, e.name, x.field
            )));
        }
        let mut scope = BTreeSet::new();
        for s in &x.scope {
            let scalar = matches!(
                declared_type(e, s),
                Some(
                    FieldType::Bool
                        | FieldType::Int
                        | FieldType::Decimal { .. }
                        | FieldType::Text { .. }
                        | FieldType::Date
                        | FieldType::Time
                        | FieldType::DateTime
                        | FieldType::Link { .. }
                )
            );
            if !scalar || s == &x.field || !scope.insert(s) {
                return Err(schema_err(format!(
                    "exclusion `{}` on `{}`: scope field `{s}` must be a distinct, declared scalar field",
                    x.name, e.name
                )));
            }
        }
        if x.message.trim().is_empty() {
            return Err(schema_err(format!(
                "exclusion `{}` on `{}` needs a message",
                x.name, e.name
            )));
        }
    }
    Ok(())
}

fn validate_search(e: &EntityDef) -> StorageResult<()> {
    let Some(sd) = &e.search else {
        return Ok(());
    };
    if sd.fields.is_empty() || sd.fields.len() > MAX_SEARCH_FIELDS {
        return Err(schema_err(format!(
            "`{}` must declare between 1 and {MAX_SEARCH_FIELDS} searchable fields",
            e.name
        )));
    }
    let mut seen = BTreeSet::new();
    for f in &sd.fields {
        if !matches!(declared_type(e, &f.name), Some(FieldType::Text { .. }))
            || !seen.insert(&f.name)
        {
            return Err(schema_err(format!(
                "searchable field `{}.{}` must be a declared text field, listed once",
                e.name, f.name
            )));
        }
    }
    for f in &sd.facets {
        if declared_type(e, f).is_none() {
            return Err(schema_err(format!(
                "facet `{}.{f}` is not a declared field",
                e.name
            )));
        }
    }
    Ok(())
}

fn schema_err(msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidSchema, msg)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;

    fn en() -> Collation {
        Collation::new("en").unwrap_or_else(|e| panic!("{e}"))
    }

    fn n(s: &str) -> Name {
        Name::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn decimals_are_never_rounded() {
        let ty = FieldType::Decimal {
            precision: 18,
            scale: 2,
        };
        assert!(
            ty.check(&Value::Decimal(Decimal::new(12345, 2)), false)
                .is_ok()
        );
        assert!(
            ty.check(&Value::Decimal(Decimal::new(123450, 3)), false)
                .is_ok()
        );
        assert!(
            ty.check(&Value::Decimal(Decimal::new(12345, 3)), false)
                .is_err()
        );
        let small = FieldType::Decimal {
            precision: 4,
            scale: 2,
        };
        assert!(
            small
                .check(&Value::Decimal(Decimal::new(9999, 2)), false)
                .is_ok()
        );
        assert!(
            small
                .check(&Value::Decimal(Decimal::new(10000, 2)), false)
                .is_err()
        );
    }

    #[test]
    fn null_needs_nullable() {
        assert!(FieldType::Int.check(&Value::Null, true).is_ok());
        assert!(FieldType::Int.check(&Value::Null, false).is_err());
        assert!(
            FieldType::Int
                .check(&Value::Text(String::new()), false)
                .is_err()
        );
    }

    #[test]
    fn child_must_point_back_to_parent() {
        let parent = EntityDef {
            name: n("invoice"),
            kind: EntityKind::Document,
            fields: vec![],
            children: vec![ChildTableDef {
                field: n("lines"),
                entity: n("invoice_line"),
            }],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        let wrong_child = EntityDef {
            name: n("invoice_line"),
            kind: EntityKind::Document,
            fields: vec![],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        let err = Catalog::new(1, en(), vec![parent, wrong_child])
            .err()
            .map(|e| e.kind);
        assert_eq!(err, Some(ErrorKind::InvalidSchema));
    }

    #[test]
    fn collations_and_link_checks_are_validated() -> StorageResult<()> {
        for ok in ["en", "ur", "und", "ur-PK", "zh-Hant"] {
            assert!(Collation::new(ok).is_ok(), "{ok}");
        }
        for bad in ["", "EN", "english", "en_US", "en-", "en-x"] {
            assert!(Collation::new(bad).is_err(), "{bad}");
        }
        let with_link = |target, check| -> StorageResult<Catalog> {
            let e = EntityDef {
                name: n("note"),
                kind: EntityKind::Document,
                fields: vec![
                    FieldDef {
                        name: n("kind"),
                        ty: FieldType::Text { max_chars: None },
                        nullable: true,
                    },
                    FieldDef {
                        name: n("about"),
                        ty: FieldType::Link { target, check },
                        nullable: true,
                    },
                ],
                children: vec![],
                indexes: vec![],
                exclusions: vec![],
                search: None,
            };
            Catalog::new(1, Collation::new("en")?, vec![e])
        };
        let dynamic = || LinkTarget::Dynamic {
            entity_field: n("kind"),
        };
        let engine = |r: &str| LinkCheck::EngineOnly { reason: r.into() };
        assert!(with_link(dynamic(), engine("polymorphic")).is_ok());
        assert!(with_link(dynamic(), LinkCheck::Database).is_err());
        assert!(with_link(LinkTarget::Entity(n("note")), engine("  ")).is_err());
        assert!(with_link(LinkTarget::Entity(n("note")), engine("hot, never deleted")).is_ok());
        Ok(())
    }

    #[test]
    fn reserved_names_are_rejected() {
        let e = EntityDef {
            name: n("party"),
            kind: EntityKind::Document,
            fields: vec![FieldDef {
                name: n("version"),
                ty: FieldType::Int,
                nullable: false,
            }],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        assert_eq!(
            Catalog::new(1, en(), vec![e]).err().map(|e| e.kind),
            Some(ErrorKind::InvalidSchema)
        );
    }
}
