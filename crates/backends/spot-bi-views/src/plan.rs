//! `generate`: the catalogue, the policy and the protection model become a [`BiPlan`]. Pure: no
//! database is touched, so the same inputs always give the same SQL.
//!
//! For each exposed document `D` the plan has `v<n>_<D>`, and for each child table `C` of `D`
//! (when the policy exposes children) `v<n>_<D>__<C>`. A document view reads the document table
//! and, when `posted_only` is set, keeps only rows with `docstatus = 1`. A child view reads the
//! child table and joins its parent so that the parent's posted filter applies to the children
//! too. A link whose target is exposed and has a title field gets a `<field>_name` column from a
//! `LEFT JOIN` to the target's base table, with the same posted filter, so a name shows only when
//! the linked record is itself visible. The joins read base tables and never other views, so the
//! views can be created in any order.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, EntityDef, EntityKind, FieldDef, FieldType, LinkTarget, Name,
};

use crate::error::{
    BiErrors, NAME_COLLISION, NAME_TOO_LONG, POLICY_INVALID, POSTED_MARKER, PROTECTED_COLUMN,
    RESERVED_NAME, UNKNOWN_COLUMN, UNKNOWN_ENTITY, UNSUPPORTED_TYPE,
};
use crate::policy::{BiPolicy, EntityPolicy};
use crate::protect::{Protection, is_system_internal};
use crate::sql::{
    BI_SCHEMA, MAX_IDENT_BYTES, POSTED, POSTED_FIELD, RESERVED_PREFIX, child_view_name,
    fingerprint, pg_type, quote_ident, quote_literal, view_name,
};

/// The plan for one policy and catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BiPlan {
    /// The catalogue version the plan was generated from.
    pub catalog_version: u64,
    /// Every document view, each followed by its child views.
    pub views: Vec<ViewPlan>,
    /// The `catalogue` view: one row per view column, for tool discovery.
    pub catalogue: ViewPlan,
    /// Every field the plan leaves out, and why. Administrators read this; it is data, not an error.
    pub excluded: Vec<Exclusion>,
}

/// One view of the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewPlan {
    /// The view's name inside the `bi` schema.
    pub name: String,
    /// The entity the view shows.
    pub entity: String,
    /// The parent document, for a child view.
    pub parent: Option<String>,
    /// The policy version the name carries.
    pub version: u32,
    /// The columns, in order.
    pub columns: Vec<PlannedColumn>,
    /// The `SELECT` the view is defined by.
    pub body: String,
    /// A fingerprint of `body`, stored as the view's comment so apply can tell it is unchanged.
    pub hash: String,
}

impl ViewPlan {
    /// `CREATE [OR REPLACE] VIEW` for this view.
    #[must_use]
    pub fn create_sql(&self, replace: bool) -> String {
        format!(
            "CREATE {}VIEW {}.{} WITH (security_barrier = true) AS {}",
            if replace { "OR REPLACE " } else { "" },
            quote_ident(BI_SCHEMA),
            quote_ident(&self.name),
            self.body
        )
    }

    /// The comment that records this view's fingerprint.
    #[must_use]
    pub fn comment_sql(&self) -> String {
        format!(
            "COMMENT ON VIEW {}.{} IS {}",
            quote_ident(BI_SCHEMA),
            quote_ident(&self.name),
            quote_literal(&self.comment_text())
        )
    }

    /// The text of the view's comment.
    #[must_use]
    pub fn comment_text(&self) -> String {
        format!("spot-bi {}", self.hash)
    }
}

/// One column of a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedColumn {
    /// The column name.
    pub name: String,
    /// The PostgreSQL type, as `format_type` reports it.
    pub pg_type: String,
    /// The description shown in `bi.catalogue`.
    pub description: String,
}

/// A field the plan leaves out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    /// The entity the field belongs to.
    pub entity: String,
    /// The field name.
    pub column: String,
    /// Why it is left out.
    pub reason: ExclusionReason,
}

/// Why a field is left out of a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The policy's `include` list does not name it.
    NotIncluded,
    /// The policy's `exclude` list names it.
    PolicyExcluded,
    /// It is a member of this protection group.
    ProtectionGroup(String),
    /// It is declared secret or password.
    Secret,
    /// It is a system audit internal (the child table's `parent_field`).
    SystemInternal,
    /// Its storage type has no BI representation.
    UnsupportedType(String),
}

/// Generates the plan.
///
/// # Errors
///
/// Every problem found, in one pass: unknown entities or fields, protected or unsupported fields
/// asked for by name, a posted-only filter with no usable `docstatus`, names that are too long
/// or collide, and title fields that are not visible.
pub fn generate(
    catalog: &Catalog,
    policy: &BiPolicy,
    protection: &Protection,
) -> Result<BiPlan, BiErrors> {
    let mut errors = BiErrors::default();
    let base = policy.base_schema().to_owned();

    // 1. The exposed documents, and the children of those that expose them.
    let mut docs: BTreeMap<Name, &EntityDef> = BTreeMap::new();
    for name in policy.exposed() {
        if name.as_str().starts_with(RESERVED_PREFIX) {
            errors.push(
                RESERVED_NAME,
                format!("`{name}` is in the reserved `{RESERVED_PREFIX}` namespace"),
            );
            continue;
        }
        match catalog.entity(name) {
            Ok(def) if def.kind == EntityKind::Document => {
                docs.insert(name.clone(), def);
            }
            Ok(_) => errors.push(
                POLICY_INVALID,
                format!("`{name}` is a child table; expose its parent with children = true"),
            ),
            Err(_) => errors.push(
                UNKNOWN_ENTITY,
                format!("`{name}` is exposed but the catalogue has no such entity"),
            ),
        }
    }
    let mut children: Vec<(Name, &EntityDef)> = Vec::new();
    for (parent_name, parent) in &docs {
        let effective = policy.entity(parent_name).cloned().unwrap_or_default();
        if !effective.children() {
            continue;
        }
        for table in &parent.children {
            if let Ok(child) = catalog.entity(&table.entity) {
                children.push((parent_name.clone(), child));
            }
        }
    }
    let child_parent: BTreeMap<Name, Name> = children
        .iter()
        .map(|(p, c)| (c.name.clone(), p.clone()))
        .collect();

    // 2. Policy tables must name an exposed document or a child of one.
    for (name, entity) in policy.configured() {
        if docs.contains_key(name) {
            continue;
        }
        if child_parent.contains_key(name) {
            let keys = entity.parent_only_keys_set();
            if !keys.is_empty() {
                errors.push(
                    POLICY_INVALID,
                    format!(
                        "[entity.{name}] sets {} but `{name}` is a child table; these belong to its parent",
                        keys.join(", ")
                    ),
                );
            }
        } else if catalog.entity(name).is_err() {
            errors.push(
                UNKNOWN_ENTITY,
                format!("[entity.{name}] names an entity the catalogue does not have"),
            );
        } else {
            errors.push(
                POLICY_INVALID,
                format!("[entity.{name}] is configured but `{name}` is not exposed"),
            );
        }
    }

    // 3. Field selection per source: documents, then their child tables.
    let mut sources: Vec<Source<'_>> = Vec::new();
    let mut excluded: Vec<Exclusion> = Vec::new();
    for (name, def) in &docs {
        let cfg = policy.entity(name).cloned().unwrap_or_default();
        if cfg.posted_only() {
            check_posted_marker(def, protection, &mut errors);
        }
        let fields = select_fields(def, &cfg, protection, &mut errors, &mut excluded);
        sources.push(Source {
            entity: def,
            parent: None,
            version: cfg.version(),
            posted: cfg.posted_only(),
            fields,
            labels: cfg.labels.clone(),
            links: Vec::new(),
        });
        for (parent, child) in children.iter().filter(|(p, _)| p == name) {
            let child_cfg = policy.entity(&child.name).cloned().unwrap_or_default();
            let fields = select_fields(child, &child_cfg, protection, &mut errors, &mut excluded);
            let parent_cfg = policy.entity(parent).cloned().unwrap_or_default();
            sources.push(Source {
                entity: child,
                parent: Some(docs[parent]),
                version: parent_cfg.version(),
                posted: parent_cfg.posted_only(),
                fields,
                labels: child_cfg.labels.clone(),
                links: Vec::new(),
            });
            excluded.push(Exclusion {
                entity: child.name.as_str().to_owned(),
                column: system::PARENT_FIELD.to_owned(),
                reason: ExclusionReason::SystemInternal,
            });
        }
    }

    // 4. Link names: a link to an exposed target with a title field shows the title.
    let visible: BTreeMap<Name, BTreeSet<Name>> = docs
        .keys()
        .map(|name| {
            let fields = sources
                .iter()
                .filter(|s| s.parent.is_none() && &s.entity.name == name)
                .flat_map(|s| s.fields.iter().map(|f| f.name.clone()))
                .collect();
            (name.clone(), fields)
        })
        .collect();
    let posted_target: BTreeMap<Name, bool> = docs
        .keys()
        .map(|name| {
            let posted = policy
                .entity(name)
                .cloned()
                .unwrap_or_default()
                .posted_only();
            (name.clone(), posted)
        })
        .collect();
    for source in &mut sources {
        let fields = source.fields.clone();
        for field in fields {
            let FieldType::Link {
                target: LinkTarget::Entity(target),
                ..
            } = &field.ty
            else {
                continue;
            };
            let Some(target_def) = docs.get(target) else {
                continue;
            };
            let Some(title) = policy.entity(target).and_then(|t| t.title_field.clone()) else {
                continue;
            };
            let Ok(title_def) = target_def.field(&title) else {
                errors.push(
                    UNKNOWN_COLUMN,
                    format!("title_field `{title}` of `{target}` is not a field of `{target}`"),
                );
                continue;
            };
            if !visible.get(target).is_some_and(|set| set.contains(&title)) {
                errors.push(
                    POLICY_INVALID,
                    format!(
                        "title_field `{title}` of `{target}` is not exposed, so `{}.{}` cannot show names",
                        source.entity.name, field.name
                    ),
                );
                continue;
            }
            let Ok(title_type) = pg_type(&title_def.ty) else {
                continue;
            };
            let column = format!("{}_name", field.name);
            if source
                .entity
                .fields
                .iter()
                .any(|f| f.name.as_str() == column)
                || source.fields.iter().any(|f| f.name.as_str() == column)
                || source.links.iter().any(|l| l.column == column)
            {
                errors.push(
                    NAME_COLLISION,
                    format!(
                        "the name column `{column}` of `{}` would collide with another column",
                        source.entity.name
                    ),
                );
                continue;
            }
            source.links.push(LinkName {
                field: field.name.as_str().to_owned(),
                target: target.as_str().to_owned(),
                title: title.as_str().to_owned(),
                title_type,
                column,
                posted: posted_target.get(target).copied().unwrap_or(false),
            });
        }
    }

    // 5. Names: the length limit and collisions between views.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut names = Vec::new();
    for source in &sources {
        let name = match &source.parent {
            None => view_name(source.version, source.entity.name.as_str()),
            Some(parent) => child_view_name(
                source.version,
                parent.name.as_str(),
                source.entity.name.as_str(),
            ),
        };
        if name.len() > MAX_IDENT_BYTES {
            errors.push(
                NAME_TOO_LONG,
                format!("the view name `{name}` is longer than {MAX_IDENT_BYTES} bytes"),
            );
        }
        if let Some(previous) = seen.insert(name.clone(), source.entity.name.as_str().to_owned()) {
            errors.push(
                NAME_COLLISION,
                format!(
                    "`{previous}` and `{}` would both become the view `{name}`",
                    source.entity.name
                ),
            );
        }
        names.push(name);
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // 6. SQL.
    let views: Vec<ViewPlan> = sources
        .iter()
        .zip(names)
        .map(|(source, name)| build_view(source, name, &base))
        .collect();
    let catalogue = build_catalogue(&views);
    Ok(BiPlan {
        catalog_version: catalog.version,
        views,
        catalogue,
        excluded,
    })
}

/// A table a view reads, with the fields it shows.
struct Source<'a> {
    entity: &'a EntityDef,
    parent: Option<&'a EntityDef>,
    version: u32,
    posted: bool,
    fields: Vec<&'a FieldDef>,
    labels: BTreeMap<Name, String>,
    links: Vec<LinkName>,
}

/// A link's name column.
#[derive(Clone)]
struct LinkName {
    field: String,
    target: String,
    title: String,
    title_type: String,
    column: String,
    posted: bool,
}

fn check_posted_marker(def: &EntityDef, protection: &Protection, errors: &mut BiErrors) {
    let marker = def.fields.iter().find(|f| f.name.as_str() == POSTED_FIELD);
    match marker {
        None => errors.push(
            POSTED_MARKER,
            format!(
                "`{}` has no `{POSTED_FIELD}` field, so its posted rows cannot be selected; add an integer `{POSTED_FIELD}` or set posted_only = false",
                def.name
            ),
        ),
        Some(field) if field.ty != FieldType::Int => errors.push(
            POSTED_MARKER,
            format!("`{}.{POSTED_FIELD}` must be an integer field", def.name),
        ),
        Some(field) if protection.exclusion(&def.name, &field.name).is_some() => errors.push(
            POSTED_MARKER,
            format!(
                "`{}.{POSTED_FIELD}` is protected; filtering on it would reveal hidden rows through counts",
                def.name
            ),
        ),
        Some(_) => {}
    }
}

/// The fields a view of `entity` shows, in catalogue order. Everything it leaves out goes to
/// `excluded` with its reason, and every name the policy asks for is checked against the catalogue
/// and the protection model.
fn select_fields<'a>(
    entity: &'a EntityDef,
    cfg: &EntityPolicy,
    protection: &Protection,
    errors: &mut BiErrors,
    excluded: &mut Vec<Exclusion>,
) -> Vec<&'a FieldDef> {
    let entity_name = entity.name.as_str();
    if let Some(include) = &cfg.include {
        for name in include {
            let text = name.as_str();
            if is_system_internal(text) {
                errors.push(
                    PROTECTED_COLUMN,
                    format!("`{entity_name}.{text}` is a system audit internal and is never shown"),
                );
                continue;
            }
            if system::ALL.contains(&text) {
                continue;
            }
            match entity.fields.iter().find(|f| &f.name == name) {
                None => errors.push(
                    UNKNOWN_COLUMN,
                    format!("`{entity_name}` has no field `{text}` to include"),
                ),
                Some(field) => {
                    if let Some(reason) = protection.exclusion(&entity.name, &field.name) {
                        errors.push(
                            PROTECTED_COLUMN,
                            format!(
                                "`{entity_name}.{text}` is protected ({}) and cannot be included",
                                describe(&reason)
                            ),
                        );
                    } else if let Err(kind) = pg_type(&field.ty) {
                        errors.push(
                            UNSUPPORTED_TYPE,
                            format!("`{entity_name}.{text}` is {kind} and has no BI type"),
                        );
                    }
                }
            }
        }
    }
    for name in &cfg.exclude {
        let text = name.as_str();
        if system::ALL.contains(&text) {
            errors.push(
                POLICY_INVALID,
                format!("`{entity_name}.{text}` is a system column and cannot be excluded"),
            );
        } else if !entity.fields.iter().any(|f| &f.name == name) {
            errors.push(
                UNKNOWN_COLUMN,
                format!("`{entity_name}` has no field `{text}` to exclude"),
            );
        }
    }
    for name in cfg.labels.keys() {
        if !entity.fields.iter().any(|f| &f.name == name) {
            errors.push(
                UNKNOWN_COLUMN,
                format!("`{entity_name}` has no field `{name}` to label"),
            );
        }
    }
    let mut out = Vec::new();
    for field in &entity.fields {
        let reason = if let Some(reason) = protection.exclusion(&entity.name, &field.name) {
            Some(reason)
        } else if let Err(kind) = pg_type(&field.ty) {
            Some(ExclusionReason::UnsupportedType(kind.to_owned()))
        } else if cfg
            .include
            .as_ref()
            .is_some_and(|inc| !inc.contains(&field.name))
        {
            Some(ExclusionReason::NotIncluded)
        } else if cfg.exclude.contains(&field.name) {
            Some(ExclusionReason::PolicyExcluded)
        } else {
            None
        };
        match reason {
            None => out.push(field),
            Some(reason) => excluded.push(Exclusion {
                entity: entity_name.to_owned(),
                column: field.name.as_str().to_owned(),
                reason,
            }),
        }
    }
    out
}

fn describe(reason: &ExclusionReason) -> String {
    match reason {
        ExclusionReason::ProtectionGroup(group) => format!("group `{group}`"),
        ExclusionReason::Secret => "secret".to_owned(),
        _ => "policy".to_owned(),
    }
}

/// The view's SQL and columns.
fn build_view(source: &Source<'_>, name: String, base: &str) -> ViewPlan {
    let table = |t: &str| format!("{}.{}", quote_ident(base), quote_ident(t));
    let mut select = Vec::new();
    let mut columns = Vec::new();
    match source.parent {
        None => {
            select.push("\"t\".\"id\"".to_owned());
            select.push("\"t\".\"version\"".to_owned());
            columns.push(column("id", "uuid", "record id"));
            columns.push(column(
                "version",
                "bigint",
                "document version, rises with every change",
            ));
        }
        Some(_) => {
            select.push("\"t\".\"id\"".to_owned());
            select.push("\"t\".\"parent\"".to_owned());
            select.push("\"t\".\"idx\"".to_owned());
            columns.push(column("id", "uuid", "record id"));
            columns.push(column("parent", "uuid", "id of the parent document"));
            columns.push(column(
                "idx",
                "integer",
                "row position within the parent's table",
            ));
        }
    }
    for field in &source.fields {
        let text = field.name.as_str();
        let Ok(ty) = pg_type(&field.ty) else { continue };
        let description = source
            .labels
            .get(&field.name)
            .cloned()
            .unwrap_or_else(|| text.to_owned());
        select.push(format!("\"t\".{}", quote_ident(text)));
        columns.push(column(text, &ty, &description));
        for link in source.links.iter().filter(|l| l.field == text) {
            let alias = link_alias(&source.links, link);
            select.push(format!(
                "{}.{} AS {}",
                quote_ident(&alias),
                quote_ident(&link.title),
                quote_ident(&link.column)
            ));
            columns.push(column(
                &link.column,
                &link.title_type,
                &format!("name of the linked {}", link.target),
            ));
        }
    }
    let mut from = format!(" FROM {} AS \"t\"", table(source.entity.name.as_str()));
    let mut filter = String::new();
    match source.parent {
        None => {
            if source.posted {
                filter = format!(" WHERE \"t\".{} = {POSTED}", quote_ident(POSTED_FIELD));
            }
        }
        Some(parent) => {
            if source.posted {
                from.push_str(&format!(
                    " JOIN {} AS \"p\" ON \"p\".\"id\" = \"t\".\"parent\"",
                    table(parent.name.as_str())
                ));
                filter = format!(" WHERE \"p\".{} = {POSTED}", quote_ident(POSTED_FIELD));
            }
        }
    }
    for link in &source.links {
        let alias = link_alias(&source.links, link);
        let mut join = format!(
            " LEFT JOIN {} AS {} ON {}.\"id\" = \"t\".{}",
            table(&link.target),
            quote_ident(&alias),
            quote_ident(&alias),
            quote_ident(&link.field)
        );
        if link.posted {
            join.push_str(&format!(
                " AND {}.{} = {POSTED}",
                quote_ident(&alias),
                quote_ident(POSTED_FIELD)
            ));
        }
        from.push_str(&join);
    }
    let body = format!("SELECT {}{from}{filter}", select.join(", "));
    let hash = fingerprint(&body);
    ViewPlan {
        name,
        entity: source.entity.name.as_str().to_owned(),
        parent: source.parent.map(|p| p.name.as_str().to_owned()),
        version: source.version,
        columns,
        body,
        hash,
    }
}

/// The alias of a link's join: `l<position of the link>`, unique within the view.
fn link_alias(links: &[LinkName], link: &LinkName) -> String {
    let position = links
        .iter()
        .position(|l| l.column == link.column)
        .unwrap_or(0);
    format!("l{position}")
}

fn column(name: &str, pg_type: &str, description: &str) -> PlannedColumn {
    PlannedColumn {
        name: name.to_owned(),
        pg_type: pg_type.to_owned(),
        description: description.to_owned(),
    }
}

/// `bi.catalogue`: one row per column of every view.
fn build_catalogue(views: &[ViewPlan]) -> ViewPlan {
    let header = [
        "view_name",
        "entity",
        "parent_entity",
        "position",
        "column_name",
        "column_type",
        "description",
    ];
    let mut rows = Vec::new();
    for view in views {
        for (index, column) in view.columns.iter().enumerate() {
            let parent = match &view.parent {
                Some(parent) => quote_literal(parent),
                None => "NULL::text".to_owned(),
            };
            rows.push(format!(
                "({}, {}, {parent}, {}::integer, {}, {}, {})",
                quote_literal(&format!("{BI_SCHEMA}.{}", view.name)),
                quote_literal(&view.entity),
                index + 1,
                quote_literal(&column.name),
                quote_literal(&column.pg_type),
                quote_literal(&column.description),
            ));
        }
    }
    let quoted: Vec<String> = header.iter().map(|h| quote_ident(h)).collect();
    let body = if rows.is_empty() {
        let typed = [
            "NULL::text",
            "NULL::text",
            "NULL::text",
            "NULL::integer",
            "NULL::text",
            "NULL::text",
            "NULL::text",
        ];
        let select: Vec<String> = typed
            .iter()
            .zip(&quoted)
            .map(|(ty, name)| format!("{ty} AS {name}"))
            .collect();
        format!("SELECT {} WHERE false", select.join(", "))
    } else {
        format!(
            "SELECT * FROM (VALUES {}) AS {} ({})",
            rows.join(", "),
            quote_ident("c"),
            quoted.join(", ")
        )
    };
    let hash = fingerprint(&body);
    ViewPlan {
        name: "catalogue".to_owned(),
        entity: String::new(),
        parent: None,
        version: 0,
        columns: header
            .iter()
            .zip(["text", "text", "text", "integer", "text", "text", "text"])
            .map(|(name, ty)| column(name, ty, name))
            .collect(),
        body,
        hash,
    }
}
