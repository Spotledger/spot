//! The parsed model: every DocType mapped onto Rust names, checked, ready to emit.
//!
//! Doc-comment wording lives here, next to the data it words. Every sentence starts with a
//! capital; a label mid-sentence is lowered with [`lower_first`], which handles the ASCII
//! labels definitions carry.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use spot_meta::{Definition, FieldSpec, FieldType};

use crate::error::{GenError, GenErrorKind};
use crate::map::{self, Namespace};

/// The whole generated module: one view per DocType and one row view per used child DocType.
#[derive(Debug, Default)]
pub struct Model {
    /// One per DocType, in definition order.
    pub types: Vec<DocType>,
    /// One row view per child DocType a parent's table uses, keyed by the child's name.
    pub rows: BTreeMap<String, RowType>,
}

/// One DocType's view.
#[derive(Debug)]
pub struct DocType {
    /// The storage name (`DOCTYPE`).
    pub name: String,
    /// The label or name, for the doc comment.
    pub label: String,
    /// The PascalCase type name.
    pub type_name: String,
    /// Whether the DocType is posted (draft / post / cancel lifecycle, ADR-42).
    pub is_posted: bool,
    /// Every member, in definition order.
    pub members: Vec<Member>,
}

impl DocType {
    /// The struct declaration's doc comment.
    #[must_use]
    pub fn doc_comment(&self) -> String {
        with_name(&self.label, &self.name)
    }
}

/// One member of a view: a scalar field or a child table.
#[derive(Debug)]
pub enum Member {
    /// A scalar field with a typed getter and setter.
    Field(Scalar),
    /// A child table with a typed row iterator, `rows_mut` and a row builder.
    Table(Table),
}

/// A scalar field.
#[derive(Debug)]
pub struct Scalar {
    /// The storage name the accessors address.
    pub name: String,
    /// The method identifier (`posting_date` -> `posting_date`, `type` -> `r#type`).
    pub ident: String,
    /// The label or name, for the doc comment.
    pub label: String,
    /// The Rust view of the field's type.
    pub kind: Kind,
    /// A select field's enum; `None` for every other kind.
    pub select: Option<Select>,
}

impl Scalar {
    /// The getter's doc comment.
    #[must_use]
    pub fn doc_comment(&self) -> String {
        with_name(&self.label, &self.name)
    }

    /// The setter's doc comment: the lowered label reads mid-sentence, and the storage
    /// name follows, so the comment spells the field the setter addresses.
    #[must_use]
    pub fn setter_comment(&self) -> String {
        match self.kind {
            Kind::Select => with_name("Sets the field from the option", &self.name),
            _ => with_name(&format!("Sets {}", lower_first(&self.label)), &self.name),
        }
    }

    /// The select enum, or `None`.
    #[must_use]
    pub fn select_of(&self) -> Option<&Select> {
        self.select.as_ref()
    }
}

/// A child-table field.
#[derive(Debug)]
pub struct Table {
    /// The storage name the accessors address.
    pub name: String,
    /// The method identifier.
    pub ident: String,
    /// The label or name, for the doc comment.
    pub label: String,
    /// The child DocType's storage name.
    pub child: String,
    /// The row view's type name.
    pub row_name: String,
}

impl Table {
    /// The table field's label for the row accessors' doc comments.
    #[must_use]
    pub fn doc_comment(&self) -> String {
        with_name(&self.label, &self.name)
    }
}

/// A select field's enum.
#[derive(Debug)]
pub struct Select {
    /// The enum's type name.
    pub enum_name: String,
    /// Every option: the stored text and the PascalCase variant, in display order.
    pub variants: Vec<(String, String)>,
}

/// The Rust view of a scalar field's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `bool`.
    Bool,
    /// `i64`.
    Int,
    /// An exact decimal (money and quantities; never a float, ADR-12).
    Decimal,
    /// Text and long text; the getter borrows `&str`.
    Text,
    /// One of a fixed set of options; the getter parses into the enum.
    Select,
    /// A link to a record.
    Link,
    /// A dynamic link: the target DocType is named by another field; typed as a link.
    DynamicLink,
    /// JSON text; the getter borrows `&str`.
    Json,
    /// Raw bytes; the getter borrows the slice.
    Bytes,
    /// A calendar date.
    Date,
    /// A wall-clock time.
    Time,
    /// An instant, stored and returned in UTC.
    DateTime,
}

/// One child DocType's typed row view.
#[derive(Debug)]
pub struct RowType {
    /// The child DocType's storage name.
    pub name: String,
    /// The row view's type name.
    pub row_name: String,
    /// The label or name, for the doc comment.
    pub label: String,
    /// Every scalar member, in definition order.
    pub fields: Vec<Scalar>,
}

impl RowType {
    /// The row struct's doc comment: one row of the child table, storage name named.
    #[must_use]
    pub fn doc_comment(&self) -> String {
        with_name(
            &format!("One row of the {} child table", lower_first(&self.label)),
            &self.name,
        )
    }
}

/// The label first, the storage name as a hint after it: `The status (status).`
fn with_name(label: &str, name: &str) -> String {
    format!("{} (`{name}`).", label.trim_end())
}

/// Lowers the first character, so a label reads mid-sentence.
pub(crate) fn lower_first(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    if let Some(c) = chars.next() {
        let _ = write!(out, "{}", c.to_ascii_lowercase());
        out.push_str(chars.as_str());
    }
    out
}

/// Parses a definition set into the model, reporting every error at once.
///
/// # Errors
/// Every invalid name, select without options, table without a child, or collision.
pub fn parse(definitions: &[Definition]) -> Result<Model, Vec<GenError>> {
    let mut errors: Vec<GenError> = Vec::new();
    let mut types = Vec::new();
    for raw in definitions {
        match type_of(raw) {
            Ok(doc_type) => types.push(doc_type),
            Err(doctypes) => errors.extend(doctypes),
        }
    }
    let mut rows: BTreeMap<String, RowType> = BTreeMap::new();
    if errors.is_empty() {
        for raw in definitions {
            for f in raw.fields.iter().filter(|f| f.ty.is_table()) {
                let Some(child) = f.options.as_deref().filter(|c| !c.is_empty()) else {
                    continue;
                };
                if rows.contains_key(child) {
                    continue;
                }
                let target = definitions.iter().find(|d| d.name == child);
                match row_type_of(child, target) {
                    Ok(row) => {
                        let _ = rows.insert(child.to_owned(), row);
                    }
                    Err(e) => errors.extend(
                        e.into_iter()
                            .map(|ge| ge.in_doctype(&raw.name).in_field(&f.name)),
                    ),
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(Model { types, rows })
    } else {
        errors.sort();
        Err(errors)
    }
}

/// Maps one definition.
///
/// Every field's errors are reported at once, so a build log names all of a DocType's bad
/// fields in one run. The DocType's own name short-circuits: a DocType no Rust name can
/// spell has no place to attribute its fields' errors to.
///
/// # Errors
/// The DocType's name is invalid or collides (short-circuiting), or one or more fields have
/// an invalid, unspellable or colliding name, or lack the options their type needs.
pub fn type_of(raw: &Definition) -> Result<DocType, Vec<GenError>> {
    let in_doc = |e: GenError| e.in_doctype(&raw.name);
    let type_name = map::pascal(&raw.name)
        .map_err(in_doc)
        .and_then(|name| ns_reserve_doc_name(&raw.name, name))
        .map_err(|e| vec![e])?;
    let mut ns = Namespace::new();
    let mut members = Vec::new();
    let mut errors: Vec<GenError> = Vec::new();
    for f in raw.fields.iter().filter(|f| f.ty != FieldType::Secret) {
        match member_of(f, &mut ns) {
            Ok(member) => members.push(member),
            Err(e) => errors.push(in_doc(e)),
        }
    }
    if errors.is_empty() {
        Ok(DocType {
            name: raw.name.clone(),
            label: raw.label_or_name().to_owned(),
            type_name,
            is_posted: raw.is_posted,
            members,
        })
    } else {
        Err(errors)
    }
}

/// Reserves the DocType's own name in a fresh namespace, before any field does.
///
/// # Errors
/// Another source already took the identifier — impossible in a fresh namespace, so the
/// error is unreachable here, but [`Namespace::reserve`] reports it if that ever changes.
fn ns_reserve_doc_name(source: &str, name: String) -> Result<String, GenError> {
    Namespace::new().reserve(source, "DocType", name)
}

/// Maps one field of a DocType.
///
/// # Errors
/// The field's name is invalid or collides, or the field lacks options.
pub fn member_of(f: &FieldSpec, ns: &mut Namespace) -> Result<Member, GenError> {
    let in_field = |e: GenError| e.in_field(&f.name);
    let ident = map::field_ident(&f.name).map_err(in_field)?;
    let ident = ns.reserve(&f.name, "field", ident).map_err(in_field)?;
    let label = f.label_or_name().to_owned();
    let scalar = |kind| {
        Member::Field(Scalar {
            name: f.name.clone(),
            ident: ident.clone(),
            label: label.clone(),
            kind,
            select: None,
        })
    };
    match f.ty {
        FieldType::Bool => Ok(scalar(Kind::Bool)),
        FieldType::Int => Ok(scalar(Kind::Int)),
        FieldType::Decimal => Ok(scalar(Kind::Decimal)),
        FieldType::Text | FieldType::LongText => Ok(scalar(Kind::Text)),
        FieldType::Link | FieldType::Attach => Ok(scalar(Kind::Link)),
        FieldType::DynamicLink => Ok(scalar(Kind::DynamicLink)),
        FieldType::Json => Ok(scalar(Kind::Json)),
        FieldType::Bytes => Ok(scalar(Kind::Bytes)),
        FieldType::Date => Ok(scalar(Kind::Date)),
        FieldType::Time => Ok(scalar(Kind::Time)),
        FieldType::DateTime => Ok(scalar(Kind::DateTime)),
        FieldType::Select => {
            let values = f.select_values();
            if values.is_empty() {
                return Err(
                    GenError::new(GenErrorKind::BadField, "select field has no options")
                        .in_field(&f.name),
                );
            }
            let base = map::pascal(&f.name).map_err(in_field)?;
            let enum_name = ns
                .reserve(&f.name, "select enum", format!("{base}Option"))
                .map_err(in_field)?;
            let mut variants = Vec::new();
            for v in &values {
                let variant = map::option_variant(v).map_err(in_field)?;
                let variant = ns.reserve(v, "select option", variant).map_err(in_field)?;
                variants.push((v.clone(), variant));
            }
            Ok(Member::Field(Scalar {
                name: f.name.clone(),
                ident: ident.clone(),
                label,
                kind: Kind::Select,
                select: Some(Select {
                    enum_name,
                    variants,
                }),
            }))
        }
        FieldType::Geo | FieldType::Range => Err(GenError::new(
            GenErrorKind::BadField,
            "this field type is not supported by the typed view generator yet",
        )
        .in_field(&f.name)),
        FieldType::Secret => Err(GenError::new(
            GenErrorKind::BadField,
            "a secret field has no typed member: the secrets service holds its value",
        )
        .in_field(&f.name)),
        FieldType::Table | FieldType::TableMultiselect => {
            let child = f
                .options
                .as_deref()
                .filter(|c| !c.is_empty())
                .ok_or_else(|| {
                    GenError::new(
                        GenErrorKind::BadField,
                        "table field has no child DocType in `options`",
                    )
                    .in_field(&f.name)
                })?;
            let row_name = format!("{}Row", map::pascal(child).map_err(in_field)?);
            Ok(Member::Table(Table {
                name: f.name.clone(),
                ident,
                label,
                child: child.to_owned(),
                row_name,
            }))
        }
    }
}

/// Maps one child DocType to its row view, reporting every field's errors at once.
///
/// # Errors
/// The child DocType does not exist in the set, or one or more of its fields are invalid —
/// in the same shape [`type_of`] reports a parent's fields.
fn row_type_of(child: &str, target: Option<&Definition>) -> Result<RowType, Vec<GenError>> {
    let Some(target) = target else {
        return Err(vec![GenError::new(
            GenErrorKind::BadField,
            format!("table uses unknown DocType `{child}`"),
        )]);
    };
    let Ok(row_name) = map::pascal(child) else {
        return Err(vec![GenError::new(
            GenErrorKind::InvalidName,
            format!("`{child}` is not a snake_case name"),
        )]);
    };
    let row_name = format!("{row_name}Row");
    let mut ns = Namespace::new();
    let mut fields = Vec::new();
    let mut errors: Vec<GenError> = Vec::new();
    for f in target.fields.iter().filter(|f| f.ty != FieldType::Secret) {
        match member_of(f, &mut ns) {
            Ok(Member::Field(field)) => fields.push(field),
            Ok(Member::Table(_)) => {
                errors.push(GenError::new(
                    GenErrorKind::BadField,
                    format!(
                        "child DocType `{child}` has its own table field `{}`",
                        f.name
                    ),
                ));
            }
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(RowType {
            name: child.to_owned(),
            row_name,
            label: target.label_or_name().to_owned(),
            fields,
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod geo_range_tests {
    use super::*;

    #[test]
    fn geo_and_range_fields_are_refused_not_invented() {
        for ty in [FieldType::Geo, FieldType::Range] {
            let f = FieldSpec::new("place", ty);
            let e = member_of(&f, &mut Namespace::new()).err();
            assert_eq!(e.map(|e| e.kind), Some(GenErrorKind::BadField));
        }
    }
}
