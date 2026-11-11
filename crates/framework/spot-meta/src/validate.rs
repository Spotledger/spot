//! Validation of a merged definition set. Reports every problem, each naming the DocType and
//! the field.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use spot_app_api::prelude::{Decimal, Name};
use spot_storage_contract::schema::system;

use crate::compile::RESERVED_INFIX;
use crate::definition::{
    Definition, FieldLookup, FieldSpec, FieldType, PLATFORM_LINK_SEARCH, check_control_name,
    split_options,
};
use crate::error::{MetaError, MetaErrorKind as K};
use crate::formats::{check_text_format, formats_of};
use spot_storage_contract::MAX_SEARCH_FIELDS;

/// The DocType that holds the platform's file records (WP-PLT-10); `attach` fields need it.
pub const FILE_DOCTYPE: &str = "file";

/// Field names the engine manages on every posted DocType.
pub const DOC_STATE: &str = "doc_state";
/// Field linking an amended document to the cancelled one it replaces.
pub const AMENDED_FROM: &str = "amended_from";
/// The engine-managed field names. Reserved on every DocType.
pub const ENGINE_FIELDS: [&str; 2] = [DOC_STATE, AMENDED_FROM];

/// Highest scale a decimal field may have.
pub const MAX_SCALE: u8 = 28;
/// Precision of a decimal field that does not set one.
pub const DEFAULT_PRECISION: u8 = 18;
/// Highest precision a decimal field may have.
pub const MAX_PRECISION: u8 = 38;
/// Highest value of a rating that does not set `max`.
pub const DEFAULT_RATING_MAX: u8 = 5;
/// Highest `max` a rating may have.
pub const MAX_RATING: u8 = 10;

/// Checks the whole set. `defs` must not yet contain the engine-managed fields. With
/// `known_seams` set (`Registry::build_with`), a lookup naming any other seam is an error;
/// `None` runs without the seam check, like `Registry::build`.
pub fn validate_with_seams(
    defs: &[Definition],
    known_seams: Option<&BTreeSet<String>>,
) -> Vec<MetaError> {
    let mut errors = Vec::new();
    let mut by_name: BTreeMap<&str, &Definition> = BTreeMap::new();
    for d in defs {
        if by_name.insert(&d.name, d).is_some() {
            errors.push(
                MetaError::new(K::Duplicate, "DocType is defined more than once")
                    .in_doctype(&d.name),
            );
        }
    }
    let mut table_users: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for d in defs {
        for f in d.fields.iter().filter(|f| f.ty.is_table()) {
            if let Some(target) = &f.options {
                table_users
                    .entry(target)
                    .or_default()
                    .push((&d.name, &f.name));
            }
        }
    }
    for d in defs {
        check_doctype(d, &by_name, &table_users, known_seams, &mut errors);
    }
    errors
}

fn check_doctype(
    d: &Definition,
    all: &BTreeMap<&str, &Definition>,
    table_users: &BTreeMap<&str, Vec<(&str, &str)>>,
    known_seams: Option<&BTreeSet<String>>,
    errors: &mut Vec<MetaError>,
) {
    let err = |kind, msg: String| MetaError::new(kind, msg).in_doctype(&d.name);
    if Name::new(&d.name).is_err() {
        errors.push(err(
            K::InvalidName,
            "DocType name must be a storage name: lowercase letters, digits and `_`, starting with a letter, at most 63 bytes".into(),
        ));
    }
    if d.is_child && d.is_posted {
        errors.push(err(K::BadTable, "a child DocType cannot be posted".into()));
    }
    if d.is_child {
        match table_users.get(d.name.as_str()).map_or(0, Vec::len) {
            0 => errors.push(err(
                K::BadTable,
                "child DocType is not used by any table field".into(),
            )),
            1 => {}
            _ => errors.push(err(
                K::BadTable,
                "a child DocType can be used by only one parent table field".into(),
            )),
        }
    }

    if let Some(naming) = &d.naming {
        if naming.trim().is_empty() {
            errors.push(err(K::BadProperty, "`naming` must not be empty".into()));
        } else if naming.len() > MAX_NAMING_BYTES {
            errors.push(err(
                K::BadProperty,
                format!(
                    "`naming` is {} bytes; the limit is {MAX_NAMING_BYTES}",
                    naming.len()
                ),
            ));
        }
    }

    // Validate protection groups
    let mut group_names = BTreeSet::new();
    for g in &d.protection_groups {
        if Name::new(&g.name).is_err() {
            errors.push(err(
                K::BadProperty,
                format!(
                    "protection group name `{}` must be a storage name: lowercase letters, digits and `_`, starting with a letter, at most 63 bytes",
                    g.name
                ),
            ));
        } else if !group_names.insert(g.name.as_str()) {
            errors.push(err(
                K::BadProperty,
                format!(
                    "protection group `{}` is defined more than once (meta.group_duplicate)",
                    g.name
                ),
            ));
        }
    }

    let mut seen = BTreeSet::new();
    for f in &d.fields {
        let ferr = |kind, msg: String| {
            MetaError::new(kind, msg)
                .in_doctype(&d.name)
                .in_field(&f.name)
        };
        if Name::new(&f.name).is_err() {
            errors.push(ferr(
                K::InvalidName,
                "field name must be a storage name: lowercase letters, digits and `_`, starting with a letter, at most 63 bytes".into(),
            ));
        } else if system::ALL.contains(&f.name.as_str()) {
            errors.push(ferr(
                K::ReservedName,
                "name is a reserved system column".into(),
            ));
        } else if ENGINE_FIELDS.contains(&f.name.as_str()) {
            errors.push(ferr(
                K::ReservedName,
                "name is reserved for an engine-managed field".into(),
            ));
        } else if f.name.contains(RESERVED_INFIX) {
            // A secret compiles to `<field>__sealed` (and `<field>__bidx`), so `__` is the
            // engine's: a field that contains it could name another field's column.
            errors.push(ferr(
                K::ReservedName,
                format!(
                    "name contains `{RESERVED_INFIX}`, which is reserved for engine-managed columns"
                ),
            ));
        }
        if !seen.insert(f.name.as_str()) {
            errors.push(ferr(K::Duplicate, "field is defined more than once".into()));
        }
        check_field(d, f, all, known_seams, errors);
    }

    if let Some(title) = &d.title_field {
        match d.fields.iter().find(|f| &f.name == title) {
            None => errors.push(err(
                K::BadTitleField,
                format!("title field `{title}` does not exist"),
            )),
            Some(f) if !f.ty.is_text_like() => errors.push(err(
                K::BadTitleField,
                format!("title field `{title}` must be text, long_text or select"),
            )),
            Some(_) => {}
        }
    }

    check_exclusions(d, errors);
    check_search(d, errors);

    for ix in &d.indexes {
        if ix.fields.is_empty() {
            errors.push(err(K::BadIndex, "an index needs at least one field".into()));
        }
        for name in &ix.fields {
            let known = d.fields.iter().find(|f| &f.name == name);
            let engine = d.is_posted && ENGINE_FIELDS.contains(&name.as_str());
            match known {
                Some(f) if f.ty.is_table() => {
                    errors.push(err(K::BadIndex, format!("index uses table field `{name}`")))
                }
                Some(f) if f.ty == FieldType::Secret => errors.push(err(
                    K::BadIndex,
                    format!("index uses secret field `{name}`, which has no column"),
                )),
                Some(f) if matches!(f.ty, FieldType::Geo | FieldType::Range) => {
                    errors.push(err(
                        K::BadIndex,
                        format!(
                            "index uses {:?} field `{name}`; a geo or range field is indexed by the platform, not by a B-tree index (use an exclusion for ranges)",
                            f.ty
                        ),
                    ))
                }
                None if !engine && !system::ALL.contains(&name.as_str()) => errors.push(err(
                    K::BadIndex,
                    format!("index uses unknown field `{name}`"),
                )),
                _ => {}
            }
        }
    }
}

fn check_field(
    d: &Definition,
    f: &FieldSpec,
    all: &BTreeMap<&str, &Definition>,
    known_seams: Option<&BTreeSet<String>>,
    errors: &mut Vec<MetaError>,
) {
    let ferr = |kind, msg: String| {
        MetaError::new(kind, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };
    let needs_options = matches!(
        f.ty,
        FieldType::Select
            | FieldType::Link
            | FieldType::DynamicLink
            | FieldType::Table
            | FieldType::TableMultiselect
    );
    // A select whose values come from a lookup seam may leave `options` out entirely.
    let values_from_seam = f.ty == FieldType::Select && f.lookup_spec().is_some();
    match (&f.options, needs_options) {
        (None, true) if values_from_seam => {}
        (None, true) => errors.push(ferr(K::BadProperty, format!("{:?} needs `options`", f.ty))),
        (Some(_), false) => errors.push(ferr(
            K::BadProperty,
            format!("`options` is not allowed on {:?}", f.ty),
        )),
        _ => {}
    }
    if f.ty == FieldType::Decimal {
        match f.scale {
            None => errors.push(ferr(K::BadProperty, "decimal needs `scale`".into())),
            Some(s) if s > MAX_SCALE => errors.push(ferr(
                K::BadProperty,
                format!("`scale` {s} is above the maximum of {MAX_SCALE}"),
            )),
            Some(s) => {
                let p = f.precision.unwrap_or(DEFAULT_PRECISION);
                if p == 0 || p > MAX_PRECISION || s > p {
                    errors.push(ferr(
                        K::BadProperty,
                        format!(
                            "`precision` {p} must be 1 to {MAX_PRECISION} and at least `scale` {s}"
                        ),
                    ));
                }
            }
        }
    } else {
        if f.scale.is_some() {
            errors.push(ferr(K::BadProperty, "`scale` is only for decimal".into()));
        }
        if f.precision.is_some() {
            errors.push(ferr(
                K::BadProperty,
                "`precision` is only for decimal".into(),
            ));
        }
    }
    match (f.ty, f.max_chars) {
        (FieldType::Text, Some(0)) => errors.push(ferr(
            K::BadProperty,
            "`max_chars` must be at least 1".into(),
        )),
        (FieldType::Text, _) | (_, None) => {}
        (_, Some(_)) => errors.push(ferr(K::BadProperty, "`max_chars` is only for text".into())),
    }
    if matches!(f.ty, FieldType::Geo | FieldType::Range) && (f.unique || f.index) {
        errors.push(ferr(
            K::BadProperty,
            format!(
                "a {:?} field cannot be `unique` or `index`: a B-tree index cannot hold it (use an exclusion for ranges)",
                f.ty
            ),
        ));
    }
    if f.ty.is_table() && (f.unique || f.index) {
        errors.push(ferr(
            K::BadProperty,
            "a table field cannot be unique or indexed".into(),
        ));
    }
    if f.ty == FieldType::Secret {
        // A secret is stored, sealed, in its own row: `unique` is allowed only through the
        // blind index, because sealed bytes are randomised and never equal for equal values.
        if f.unique && !f.lookup_index() {
            errors.push(ferr(
                K::BadProperty,
                "a secret field cannot be `unique` without `lookup = true`: sealed values are \
                 randomised, so uniqueness can only be checked on the blind index"
                    .into(),
            ));
        }
        if f.index {
            errors.push(ferr(
                K::BadProperty,
                "a secret field cannot be `index`: only a `lookup = true` secret is indexed, \
                 through its blind index"
                    .into(),
            ));
        }
        // Nothing that shows or filters on the value, and nothing that copies it in.
        for (set, what) in [
            (f.in_list_view, "in_list_view"),
            (f.in_standard_filter, "in_standard_filter"),
        ] {
            if set {
                errors.push(ferr(
                    K::BadProperty,
                    format!(
                        "a secret field cannot be `{what}`: its value is never shown or filtered"
                    ),
                ));
            }
        }
    }
    check_format(d, f, errors);
    check_lookup(d, f, known_seams, errors);
    check_multiple(d, f, errors);
    check_control(d, f, errors);

    let target = f.options.as_deref().unwrap_or("");
    match f.ty {
        FieldType::Select => {
            let values = f.select_values();
            let distinct: BTreeSet<_> = values.iter().collect();
            // With a lookup the values come from the seam, so `options` may be empty
            // (ADR-068 §3, WP-MET-01.T5 rule 2); without one it still needs at least one.
            if values.is_empty() && f.lookup_spec().is_none() {
                errors.push(ferr(
                    K::BadOptions,
                    "select needs at least one option".into(),
                ));
            } else if distinct.len() != values.len() {
                errors.push(ferr(
                    K::BadOptions,
                    "select options must be distinct".into(),
                ));
            }
        }
        FieldType::Link => match all.get(target) {
            None => errors.push(ferr(
                K::UnknownTarget,
                format!("links to unknown DocType `{target}`"),
            )),
            Some(t) if t.is_child => errors.push(ferr(
                K::UnknownTarget,
                format!("links to child DocType `{target}`; link to its parent instead"),
            )),
            Some(_) => {}
        },
        FieldType::Table | FieldType::TableMultiselect => {
            if d.is_child {
                errors.push(ferr(
                    K::BadTable,
                    "a child DocType cannot have table fields".into(),
                ));
            }
            match all.get(target) {
                None => errors.push(ferr(
                    K::UnknownTarget,
                    format!("table uses unknown DocType `{target}`"),
                )),
                Some(t) if !t.is_child => errors.push(ferr(
                    K::BadTable,
                    format!("table `options` must be a child DocType, but `{target}` is not one"),
                )),
                Some(t) => {
                    let links = t.fields.iter().filter(|g| g.ty == FieldType::Link).count();
                    if f.ty == FieldType::TableMultiselect && links != 1 {
                        errors.push(ferr(
                            K::BadTable,
                            format!(
                                "table_multiselect child `{target}` must have exactly one link field, but has {links}"
                            ),
                        ));
                    }
                }
            }
        }
        FieldType::Attach if !all.contains_key(FILE_DOCTYPE) => errors.push(ferr(
            K::UnknownTarget,
            format!("attach needs the platform DocType `{FILE_DOCTYPE}` (the file record), which is not in the set"),
        )),
        FieldType::DynamicLink => match d.fields.iter().find(|g| g.name == target) {
            Some(g) if g.ty.is_text_like() => {}
            Some(_) => errors.push(ferr(
                K::BadProperty,
                format!("dynamic link type field `{target}` must be text-like"),
            )),
            None => errors.push(ferr(
                K::UnknownTarget,
                format!("dynamic link type field `{target}` does not exist"),
            )),
        },
        _ => {}
    }

    // Field-level security validation
    check_field_security(d, f, errors);

    if let Some(expr) = f.default_expression() {
        // An expression default is kept as text; only its type and shape are checked.
        if matches!(f.ty, FieldType::Secret) || !default_allowed(f.ty) {
            errors.push(ferr(
                K::BadDefault,
                format!("{:?} fields cannot have a default", f.ty),
            ));
        } else if f.multiple && f.ty == FieldType::Select {
            errors.push(ferr(
                K::BadDefault,
                "a multiple select needs a list of its options as `default`, not an expression"
                    .into(),
            ));
        } else if let Err(why) = check_expression_text("default expression", expr) {
            errors.push(ferr(K::BadDefault, why));
        }
    } else if let Some(default) = f.default_literal() {
        let checked = if f.multiple && f.ty == FieldType::Select {
            check_multiple_default(f, default)
        } else {
            check_default(f, default)
        };
        if let Err(why) = checked {
            errors.push(ferr(K::BadDefault, why));
        }
    }
    check_rules(d, f, errors);
}

/// Whether a field type may carry a default (the same list as the descriptor's).
fn default_allowed(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Bool
            | FieldType::Int
            | FieldType::Decimal
            | FieldType::Text
            | FieldType::LongText
            | FieldType::Select
            | FieldType::Date
            | FieldType::Time
            | FieldType::DateTime
            | FieldType::Range
    )
}

/// Longest expression text a property may hold, in bytes.
pub const MAX_EXPRESSION_BYTES: usize = 1000;
/// Longest `naming` text, in bytes.
pub const MAX_NAMING_BYTES: usize = 200;

/// Shape check of expression text: not empty, not only whitespace, within the cap. The text
/// is never parsed here; `spot-plan` does that with the shared language.
pub(crate) fn check_expression_text(what: &str, text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        Err(format!("{what} must not be empty"))
    } else if text.len() > MAX_EXPRESSION_BYTES {
        Err(format!(
            "{what} is {} bytes; the limit is {MAX_EXPRESSION_BYTES}",
            text.len()
        ))
    } else {
        Ok(())
    }
}

/// Whether a type may carry `fetch_from`: a value that is neither a link, a table nor a secret.
fn accepts_fetch(ty: FieldType) -> bool {
    !matches!(
        ty,
        FieldType::Link
            | FieldType::DynamicLink
            | FieldType::Table
            | FieldType::TableMultiselect
            | FieldType::Secret
    )
}

/// Checks the rule properties of one field (ADR-068 §1): the expression texts by shape and
/// `fetch_from` by shape and link field. The three `*_depends_on` properties are valid on
/// every field type (every type holds a value or child rows).
fn check_rules(d: &Definition, f: &FieldSpec, errors: &mut Vec<MetaError>) {
    let ferr = |msg: String| {
        MetaError::new(K::BadProperty, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };
    for (key, value) in [
        ("depends_on", &f.depends_on),
        ("mandatory_depends_on", &f.mandatory_depends_on),
        ("read_only_depends_on", &f.read_only_depends_on),
    ] {
        if let Some(text) = value
            && let Err(why) = check_expression_text(&format!("`{key}`"), text)
        {
            errors.push(ferr(why));
        }
    }
    let Some(from) = &f.fetch_from else {
        return;
    };
    if !accepts_fetch(f.ty) {
        errors.push(ferr(format!(
            "`fetch_from` is not allowed on {:?}: it is for value fields, not links, tables or secrets",
            f.ty
        )));
        return;
    }
    let shape = "`fetch_from` must be `<link_field>.<target_field>` in storage-name characters";
    let Some((link, target)) = from.split_once('.') else {
        errors.push(ferr(format!("{shape}, not `{from}`")));
        return;
    };
    if Name::new(link).is_err() || Name::new(target).is_err() {
        errors.push(ferr(format!("{shape}, not `{from}`")));
        return;
    }
    match d.fields.iter().find(|g| g.name == link) {
        None => errors.push(ferr(format!(
            "`fetch_from` link field `{link}` does not exist on this DocType"
        ))),
        Some(g) if g.ty != FieldType::Link => errors.push(ferr(format!(
            "`fetch_from` link field `{link}` must be a link field, not {:?}",
            g.ty
        ))),
        Some(_) => {}
    }
}

/// Where a field type may carry a `lookup` (ADR-068 §3.1); `descriptor.rs` keeps the same
/// list. Inlined as a match so the check needs no descriptor import.
fn accepts_lookup(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Link
            | FieldType::DynamicLink
            | FieldType::Select
            | FieldType::TableMultiselect
            | FieldType::Text
    )
}

/// Checks one field's `lookup` (ADR-068 §3, WP-MET-01.T5): the field type, the seam's shape
/// and the reserved `platform:` prefix, the not-yet rule for `when` and, when the build names
/// the known seams, that the seam is one of them.
fn check_lookup(
    d: &Definition,
    f: &FieldSpec,
    known_seams: Option<&BTreeSet<String>>,
    errors: &mut Vec<MetaError>,
) {
    let Some(lookup) = &f.lookup else {
        return;
    };
    let ferr = |msg: String| {
        MetaError::new(K::BadProperty, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };
    // `lookup = true` (or `false`) declares a secret's blind index and carries no seam to
    // check; on any other type it is refused.
    let FieldLookup::Spec(lookup) = lookup else {
        if f.ty != FieldType::Secret {
            errors.push(ferr(format!(
                "`lookup = true` is only for secret fields; a {:?} field takes the lookup \
                 table (`[fields.lookup]`) instead",
                f.ty
            )));
        }
        return;
    };
    if f.ty == FieldType::Secret {
        errors.push(ferr(
            "a secret field takes `lookup = true` (a blind index), not a lookup table".into(),
        ));
        return;
    }
    if !accepts_lookup(f.ty) {
        errors.push(ferr(format!(
            "`lookup` is not allowed on {:?}: it is for link, dynamic_link, select, \
             table_multiselect and text fields",
            f.ty
        )));
        return;
    }
    if !lookup.when.is_empty() {
        errors.push(
            MetaError::new(
                K::NotYetSupported,
                "`when` rules wait for the shared expression language (WP-UI-11, ADR-068 §3.9); \
                 the format is kept, but nothing is accepted yet",
            )
            .in_doctype(&d.name)
            .in_field(&f.name),
        );
    }
    for rule in &lookup.when {
        check_seam(d, &f.name, &rule.seam, known_seams, errors);
    }
    if let Some(seam) = &lookup.seam {
        check_seam(d, &f.name, seam, known_seams, errors);
    }
}

/// Checks one seam name: `<app>:<name>` in storage-name characters, the reserved `platform:`
/// prefix (only `platform:link_search`) and the known-seam set.
fn check_seam(
    d: &Definition,
    field: &str,
    seam: &str,
    known_seams: Option<&BTreeSet<String>>,
    errors: &mut Vec<MetaError>,
) {
    let err = |kind, msg: String| {
        MetaError::new(kind, msg)
            .in_doctype(&d.name)
            .in_field(field)
    };
    let storage_name = |s: &str| Name::new(s).is_ok();
    // The standard seam always exists, whatever set the build was given.
    if seam == PLATFORM_LINK_SEARCH {
        return;
    }
    match seam.split_once(':') {
        None | Some((_, "")) => errors.push(err(
            K::BadProperty,
            format!("lookup seam `{seam}` must be `<app>:<name>` in storage-name characters"),
        )),
        Some(("platform", _)) => errors.push(err(
            K::BadProperty,
            format!(
                "the `platform:` prefix is reserved: only `{PLATFORM_LINK_SEARCH}` is accepted"
            ),
        )),
        Some((app, name)) if !storage_name(app) || !storage_name(name) => errors.push(err(
            K::BadProperty,
            format!("lookup seam `{seam}` must be `<app>:<name>` in storage-name characters"),
        )),
        Some((_, _)) => {
            if let Some(known) = known_seams
                && !known.contains(seam)
            {
                errors.push(err(
                    K::UnknownSeam,
                    format!("lookup seam `{seam}` is not a seam this build knows"),
                ));
            }
        }
    }
}

/// Checks the `multiple` property (ADR-068 amendment A2). It changes the shape of the stored
/// value, so it is valid on a `select` only and cannot also be `unique`.
fn check_multiple(d: &Definition, f: &FieldSpec, errors: &mut Vec<MetaError>) {
    if !f.multiple {
        return;
    }
    let ferr = |msg: String| {
        MetaError::new(K::BadProperty, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };
    if f.ty != FieldType::Select {
        errors.push(ferr(format!(
            "`multiple` is only for select fields, not {:?}: it would change what is stored",
            f.ty
        )));
    }
    if f.unique {
        errors.push(ferr(
            "a multiple select cannot be `unique`: it holds several values".into(),
        ));
    }
}

/// Checks a field's `control` hint (ADR-068 amendment A2). An unknown name is refused with the
/// list of valid ones; a name that cannot apply to the field's kind is **not** refused here: it
/// is kept and the compile report warns (`compile.rs`), so the client falls back.
fn check_control(d: &Definition, f: &FieldSpec, errors: &mut Vec<MetaError>) {
    let Some(name) = f.control.as_deref() else {
        return;
    };
    if let Err(why) = check_control_name(name) {
        errors.push(
            MetaError::new(K::BadProperty, why)
                .in_doctype(&d.name)
                .in_field(&f.name),
        );
    }
}

/// Checks a multiple select's default: a list of the field's options, one per line (the same
/// convention `options` uses). A scalar default is refused, because the stored value is an
/// array. A list of one value is written with a trailing line break so it stays a list.
fn check_multiple_default(f: &FieldSpec, v: &str) -> Result<(), String> {
    if !v.contains('\n') {
        return Err(format!(
            "default `{v}` is scalar; a multiple select needs a list of its options, one per line"
        ));
    }
    let values = split_options(v);
    if values.is_empty() {
        return Err("a multiple select's default needs at least one value".into());
    }
    for (i, value) in values.iter().enumerate() {
        if values[..i].contains(value) {
            return Err(format!("default `{value}` is listed twice"));
        }
    }
    let options = f.select_values();
    match values.iter().find(|value| !options.contains(value)) {
        Some(value) => Err(format!("default `{value}` is not one of the options")),
        None => Ok(()),
    }
}

fn check_default(f: &FieldSpec, v: &str) -> Result<(), String> {
    let bad = |what: &str| format!("default `{v}` is not {what}");
    match f.ty {
        FieldType::Bool => (v == "true" || v == "false")
            .then_some(())
            .ok_or_else(|| bad("`true` or `false`")),
        FieldType::Int => {
            let n = v.parse::<i64>().map_err(|_| bad("a 64-bit integer"))?;
            match (f.format.as_deref(), f.max) {
                (Some("rating"), max)
                    if !(0..=i64::from(max.unwrap_or(DEFAULT_RATING_MAX))).contains(&n) =>
                {
                    Err(format!("default `{v}` is outside the rating range"))
                }
                _ => Ok(()),
            }
        }
        FieldType::Decimal => {
            let d = Decimal::from_str(v).map_err(|_| bad("a decimal"))?;
            let scale = u32::from(f.scale.unwrap_or(0));
            if d.normalize().scale() > scale {
                return Err(format!(
                    "default `{v}` has more than {scale} decimal places"
                ));
            }
            Ok(())
        }
        FieldType::Text => {
            if let Some(max) = f.max_chars
                && v.chars().count() > max as usize
            {
                return Err(format!("default is longer than {max} characters"));
            }
            match f.format.as_deref() {
                Some(format) if formats_of(FieldType::Text).contains(&format) => {
                    check_text_format(format, v).map_err(|why| format!("default: {why}"))
                }
                _ => Ok(()),
            }
        }
        FieldType::LongText => Ok(()),
        FieldType::Select => f
            .select_values()
            .iter()
            .any(|o| o == v)
            .then_some(())
            .ok_or_else(|| format!("default `{v}` is not one of the options")),
        FieldType::Date => parse_date(v)
            .then_some(())
            .ok_or_else(|| bad("a date (YYYY-MM-DD)")),
        FieldType::Time => parse_time(v)
            .then_some(())
            .ok_or_else(|| bad("a time (HH:MM[:SS])")),
        FieldType::DateTime => parse_datetime(v)
            .then_some(())
            .ok_or_else(|| bad("a datetime (YYYY-MM-DDTHH:MM:SS with Z or an offset)")),
        FieldType::Range => check_range_default(f, v),
        FieldType::Link
        | FieldType::DynamicLink
        | FieldType::Json
        | FieldType::Bytes
        | FieldType::Geo
        | FieldType::Table
        | FieldType::TableMultiselect
        | FieldType::Attach => Err(format!("{:?} fields cannot have a default", f.ty)),
        FieldType::Secret => Err(
            "a secret field cannot have a default: the value never travels in a document".into(),
        ),
    }
}

fn digits(s: &str, len: usize) -> Option<u32> {
    (s.len() == len && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn parse_date(s: &str) -> bool {
    let mut p = s.split('-');
    let (Some(y), Some(m), Some(d), None) = (p.next(), p.next(), p.next(), p.next()) else {
        return false;
    };
    let (Some(y), Some(m), Some(d)) = (digits(y, 4), digits(m, 2), digits(d, 2)) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

fn parse_time(s: &str) -> bool {
    let (main, fraction) = match s.split_once('.') {
        Some((m, f)) => (m, Some(f)),
        None => (s, None),
    };
    if fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let parts: Vec<&str> = main.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [h, m] if fraction.is_none() => (*h, *m, "00"),
        [h, m, s] => (*h, *m, *s),
        _ => return false,
    };
    matches!(
        (digits(h, 2), digits(m, 2), digits(sec, 2)),
        (Some(h), Some(m), Some(s)) if h < 24 && m < 60 && s < 60
    )
}

fn parse_datetime(s: &str) -> bool {
    let Some((date, rest)) = s.split_once('T') else {
        return false;
    };
    let time = if let Some(t) = rest.strip_suffix('Z') {
        t
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, offset) = rest.split_at(i);
        let off = &offset[1..];
        let ok = off.split_once(':').is_some_and(
            |(h, m)| matches!((digits(h, 2), digits(m, 2)), (Some(h), Some(m)) if h < 24 && m < 60),
        );
        if !ok {
            return false;
        }
        t
    } else {
        return false;
    };
    parse_date(date) && parse_time(time)
}

/// Checks the format and its parameters against the base type (ADR-068 §2).
fn check_format(d: &Definition, f: &FieldSpec, errors: &mut Vec<MetaError>) {
    let ferr = |msg: String| {
        MetaError::new(K::BadProperty, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };
    let allowed = formats_of(f.ty);
    let format = f.format.as_deref();
    let known = match format {
        None => None,
        Some(x) if allowed.contains(&x) => Some(x),
        Some(x) if allowed.is_empty() => {
            errors.push(ferr(format!(
                "{:?} has no formats, but format `{x}` is set",
                f.ty
            )));
            None
        }
        Some(x) => {
            errors.push(ferr(format!(
                "format `{x}` is not allowed on {:?}; use one of: {}",
                f.ty,
                allowed.join(", ")
            )));
            None
        }
    };
    // Parameters belong to one format each; a parameter without it is refused, as is a format
    // missing its required parameter. An unknown or misplaced format is already reported, so
    // its parameters are not judged again.
    let params = [
        ("syntax", f.syntax.is_some(), "code"),
        ("symbology", f.symbology.is_some(), "barcode"),
        ("max", f.max.is_some(), "rating"),
        ("currency_field", f.currency_field.is_some(), "currency"),
    ];
    if format.is_none() || known.is_some() {
        for (param, set, owner) in params {
            if set && known != Some(owner) {
                errors.push(ferr(format!("`{param}` is only for the `{owner}` format")));
            }
        }
    }
    if f.ty == FieldType::Range && format.is_none() {
        errors.push(ferr(format!(
            "a range needs a `format`, which says what it ranges over: {}",
            allowed.join(" or ")
        )));
    }
    if known == Some("code") && f.syntax.as_deref().is_none_or(str::is_empty) {
        errors.push(ferr("the `code` format needs `syntax`".into()));
    }
    if known == Some("currency") {
        match f.currency_field.as_deref() {
            None => errors.push(ferr("the `currency` format needs `currency_field`".into())),
            Some(c) if c == f.name => {
                errors.push(ferr("`currency_field` must name another field".into()))
            }
            Some(c) => match d.fields.iter().find(|g| g.name == c) {
                None => errors.push(ferr(format!("`currency_field` `{c}` does not exist"))),
                Some(g) if g.ty != FieldType::Link => {
                    errors.push(ferr(format!("`currency_field` `{c}` must be a link field")))
                }
                Some(_) => {}
            },
        }
    }
    if let Some(max) = f.max
        && known == Some("rating")
        && !(1..=MAX_RATING).contains(&max)
    {
        errors.push(ferr(format!("`max` {max} must be 1 to {MAX_RATING}")));
    }
}

/// Field-level security validation (ADR-068 amendment A1, ADR-070 section 12).
fn check_field_security(d: &Definition, f: &FieldSpec, errors: &mut Vec<MetaError>) {
    let ferr = |msg: String| {
        MetaError::new(K::BadProperty, msg)
            .in_doctype(&d.name)
            .in_field(&f.name)
    };

    // Check that protection_group names a declared group (meta.unknown_group)
    if let Some(group) = &f.protection_group
        && !d.protection_groups.iter().any(|g| &g.name == group)
    {
        errors.push(ferr(format!(
            "protection group `{group}` is not declared on this DocType (meta.unknown_group)"
        )));
    }

    // Check that inherited_groups is never authored (meta.inherited_authored) - this will be checked
    // in compile phase since inherited_groups is computed, not parsed from TOML

    // Check that masking is only on allowed types (meta.mask_not_applicable)
    if f.masking.is_some() {
        match f.ty {
            FieldType::Text | FieldType::LongText | FieldType::Int | FieldType::Decimal => {}
            _ => errors.push(ferr(format!(
                "masking is not applicable to {:?} (meta.mask_not_applicable)",
                f.ty
            ))),
        }
    }

    // Check that masking format is valid (meta.mask_invalid)
    if let Some(mask) = &f.masking
        && !mask.well_formed()
    {
        errors.push(ferr(
            "masking format is invalid: use placeholder=true or last_chars=n with n from 1 to 8 (meta.mask_invalid)"
                .into(),
        ));
    }

    // Check that declassified has a non-empty reason (meta.declassify_reason)
    if let Some(reason) = &f.declassified
        && reason.is_empty()
    {
        errors.push(ferr(
            "declassified reason cannot be empty (meta.declassify_reason)".into(),
        ));
    }

    // Check that if classified as secret/pii, field has protection or is declassified (meta.tag_unprotected)
    // Exception: Secret kind fields are exempt
    let is_secret_kind = f.ty == FieldType::Secret;
    if !is_secret_kind
        && f.classification
            .iter()
            .any(|tag| tag == "secret" || tag == "pii")
        && f.protection_group.is_none()
        && f.declassified.is_none()
    {
        errors.push(ferr(
            "field tagged secret or pii must have protection_group or declassified (meta.tag_unprotected)".into(),
        ));
    }

    // Check that permlevel and protection_group are not both set (meta.permlevel_conflict)
    // The generated `level_N` group beside `permlevel = N` is the migrated form (see
    // `crate::registry::is_migrated_permlevel`), which is valid input: an effective snapshot's
    // own definitions rebuild.
    if f.permlevel > 0
        && f.protection_group.is_some()
        && !crate::registry::is_migrated_permlevel(f.permlevel, f.protection_group.as_deref())
    {
        errors.push(ferr(
            "field cannot have both permlevel >= 1 and protection_group set (meta.permlevel_conflict)".into(),
        ));
    }
}

/// Checks a range field's default (ADR-087 section 5.1): the half-open text `[start,end)` over
/// the format's payload (`date_range`: `YYYY-MM-DD`; `instant_range`: an instant with `Z` or an
/// offset). An empty range (`start >= end`), an unbounded one (a missing bound or `infinity`)
/// and any other bracket shape are refused, as the contract refuses them on write
/// (`RangeValue::validate`). Without a format the payload is unknown and the format error is
/// the one reported.
fn check_range_default(f: &FieldSpec, v: &str) -> Result<(), String> {
    use std::cmp::Ordering;

    let shape = "a half-open range written `[start,end)`";
    let inner = v
        .trim()
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(|| format!("default `{v}` is not {shape}; other brackets are refused in v1"))?;
    let Some((start, end)) = inner.split_once(',') else {
        return Err(format!("default `{v}` is not {shape}"));
    };
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() || end.is_empty() || [start, end].iter().any(|b| b.contains("infinity")) {
        return Err(format!(
            "default `{v}` is unbounded; both bounds must be finite or the resource is blocked forever"
        ));
    }
    let order = match f.format.as_deref() {
        Some("date_range") => {
            let (a, b) = (
                crate::compile::parse::date(start),
                crate::compile::parse::date(end),
            );
            match (a, b) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => return Err(format!("default `{v}` bounds must be dates (YYYY-MM-DD)")),
            }
        }
        Some("instant_range") => {
            let (a, b) = (
                crate::compile::parse::datetime(start),
                crate::compile::parse::datetime(end),
            );
            match (a, b) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => {
                    return Err(format!(
                        "default `{v}` bounds must be instants (YYYY-MM-DDTHH:MM:SS with Z or an offset)"
                    ));
                }
            }
        }
        _ => return Ok(()),
    };
    match order {
        Ordering::Less => Ok(()),
        _ => Err(format!(
            "default `{v}` is an empty range: the start must be before the end (an empty range conflicts with nothing and would switch an exclusion off)"
        )),
    }
}

/// Whether a field is protected on its own: in a protection group, or at a permission level
/// (the legacy form of one). Inheritance from an input is checked after it is computed
/// (`check_search_inheritance`).
fn is_protected(f: &FieldSpec) -> bool {
    f.protection_group.is_some() || f.permlevel > 0
}

/// Checks a DocType's `exclusions` (ADR-087 section 5.1). Each is `meta.bad_exclusion`.
fn check_exclusions(d: &Definition, errors: &mut Vec<MetaError>) {
    let err = |msg: String| MetaError::new(K::BadExclusion, msg).in_doctype(&d.name);
    let mut ranges = BTreeSet::new();
    for x in &d.exclusions {
        let ferr = |msg: String| err(msg).in_field(&x.field);
        match d.fields.iter().find(|f| f.name == x.field) {
            None => errors.push(ferr(format!(
                "exclusion names field `{}`, which does not exist",
                x.field
            ))),
            Some(f) if f.ty != FieldType::Range => errors.push(ferr(format!(
                "exclusion field `{}` is {:?}; it must be a range",
                x.field, f.ty
            ))),
            Some(_) => {}
        }
        if !ranges.insert(x.field.as_str()) {
            errors.push(ferr(format!(
                "`{}` has more than one exclusion; declare one with the whole scope",
                x.field
            )));
        }
        let mut scope = BTreeSet::new();
        for s in &x.scope {
            if s == &x.field || !scope.insert(s.as_str()) {
                errors.push(ferr(format!(
                    "scope field `{s}` is listed twice or is the range itself"
                )));
                continue;
            }
            match d.fields.iter().find(|f| &f.name == s) {
                None => errors.push(ferr(format!(
                    "scope field `{s}` does not exist"
                ))),
                Some(f) if !scope_scalar(f.ty) => errors.push(ferr(format!(
                    "scope field `{s}` is {:?}; a scope field must be a scalar (bool, int, decimal, text, select, date, time, datetime or link)",
                    f.ty
                ))),
                // A null scope never conflicts, so a record that leaves one out would sit
                // outside the exclusion without anyone choosing that.
                Some(f) if !f.required => errors.push(ferr(format!(
                    "scope field `{s}` is not `required`; a record with no value there is never in conflict, which would switch the exclusion off for it"
                ))),
                Some(_) => {}
            }
        }
        if x.message.trim().is_empty() {
            errors.push(ferr("an exclusion needs a plain-language `message`".into()));
        }
    }
}

/// The field types an exclusion may be scoped by: the contract's scalar list. A dynamic link
/// is a link whose target varies, so it is no equality scope.
fn scope_scalar(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Bool
            | FieldType::Int
            | FieldType::Decimal
            | FieldType::Text
            | FieldType::Select
            | FieldType::Date
            | FieldType::Time
            | FieldType::DateTime
            | FieldType::Link
    )
}

/// Checks a DocType's `search` declaration (ADR-087 section 7.2).
fn check_search(d: &Definition, errors: &mut Vec<MetaError>) {
    let Some(sd) = &d.search else {
        return;
    };
    let err = |msg: String| MetaError::new(K::BadSearch, msg).in_doctype(&d.name);
    if sd.fields.is_empty() || sd.fields.len() > MAX_SEARCH_FIELDS {
        errors.push(err(format!(
            "`search` must name between 1 and {MAX_SEARCH_FIELDS} fields, not {}",
            sd.fields.len()
        )));
    }
    if let Some(language) = &sd.language
        && (language.is_empty()
            || language.len() > 32
            || !language
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
        errors.push(err(format!(
            "`search.language` `{language}` must be a short language name such as `en`"
        )));
    }
    let mut seen = BTreeSet::new();
    for sf in &sd.fields {
        let ferr = |kind, msg: String| {
            MetaError::new(kind, msg)
                .in_doctype(&d.name)
                .in_field(&sf.name)
        };
        if !matches!(sf.weight.as_str(), "A" | "B" | "C" | "D") {
            errors.push(ferr(
                K::BadSearch,
                format!("weight `{}` must be A, B, C or D", sf.weight),
            ));
        }
        if !seen.insert(sf.name.as_str()) {
            errors.push(ferr(
                K::BadSearch,
                "field is listed twice in `search`".into(),
            ));
        }
        match d.fields.iter().find(|f| f.name == sf.name) {
            None => errors.push(ferr(
                K::BadSearch,
                "searchable field does not exist".into(),
            )),
            Some(f) if !matches!(f.ty, FieldType::Text | FieldType::LongText) => {
                errors.push(ferr(
                    K::BadSearch,
                    format!("searchable field is {:?}; it must be text or long_text", f.ty),
                ));
            }
            // A protected field never enters a projection: a projection has no field guard of
            // its own, so a search hit or a rank would tell a reader what the guard hides.
            Some(f) if is_protected(f) => errors.push(ferr(
                K::SearchFieldProtected,
                "a field in a protection group can never be searched: protected fields do not enter a projection".into(),
            )),
            Some(_) => {}
        }
    }
    for facet in &sd.facets {
        let ferr = |kind, msg: String| {
            MetaError::new(kind, msg)
                .in_doctype(&d.name)
                .in_field(facet)
        };
        match d.fields.iter().find(|f| &f.name == facet) {
            None => errors.push(ferr(K::BadSearch, "facet field does not exist".into())),
            Some(f) if !scope_scalar(f.ty) => errors.push(ferr(
                K::BadSearch,
                format!("facet field is {:?}; a facet must be a scalar field", f.ty),
            )),
            // A count by a protected field is a leak of that field.
            Some(f) if is_protected(f) => errors.push(ferr(
                K::SearchFieldProtected,
                "a field in a protection group can never be a facet: a count reveals it".into(),
            )),
            Some(_) => {}
        }
    }
}

/// The inheritance half of the protected-search rule: a search field or facet that is copied
/// or derived from a protected input is protected too (ADR-070 section 12, rule 5), so it may
/// not enter a projection either. Runs after the inheritance map is computed.
pub(crate) fn check_search_inheritance(
    defs: &[Definition],
    inheritance: &crate::derive::InheritanceMap,
) -> Vec<MetaError> {
    let mut errors = Vec::new();
    for d in defs {
        let Some(sd) = &d.search else {
            continue;
        };
        let named = sd
            .fields
            .iter()
            .map(|f| &f.name)
            .chain(&sd.facets)
            .collect::<BTreeSet<_>>();
        for name in named {
            let declassified = d
                .fields
                .iter()
                .find(|f| &f.name == name)
                .is_some_and(|f| f.declassified.is_some());
            if !declassified
                && inheritance
                    .get(&crate::derive::FieldRef::new(&d.name, name))
                    .is_some_and(|g| !g.is_empty())
            {
                errors.push(
                    MetaError::new(
                        K::SearchFieldProtected,
                        "this field inherits a protection group from its inputs, so it can never enter a projection or be a facet",
                    )
                    .in_doctype(&d.name)
                    .in_field(name),
                );
            }
        }
    }
    errors
}
