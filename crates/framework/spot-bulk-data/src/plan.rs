//! The export plan: the checked, stable description of one export.
//!
//! [`ExportPlan::build`] is where every safety decision is made, once, before a single row
//! moves:
//!
//! 1. the caller holds the **export** right on the DocType ([`ExportAuthority`], the caller's
//!    `PermissionEngine` behind a trait);
//! 2. every field the filter or the sort uses is one the caller may read **unmasked** and report
//!    (`FieldGuard::check_query_fields_for` with `Channel::Export`, the no-inference rule);
//! 3. every requested column is classified for [`Channel::Export`](spot_field_guard::Channel)
//!    (`FieldGuard::columns`): a column the channel hides is an **error** naming the path, never
//!    silently dropped, and a column that may leave only masked is exported in its masked form;
//! 4. the principal's read rights become one filter (`PermissionFilter::for_read`) ANDed with
//!    the caller's own filter.
//!
//! The plan carries a stable `digest` over the DocType, columns, order, format and filter, and
//! a `filter_digest` that names the filter without recording its values.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use spot_field_guard::{Channel, FieldGuard, ProtectionSource, QueryField, QueryUse};
use spot_perm_query::{PermissionFilter, ReadFilter, ReadTarget};
use spot_permissions::{DocFacts, FieldFacts, FieldLevel, MaskFormat, Principal};
use spot_storage_contract::{
    CmpOp, Expr, FieldRef, Join, JoinKind, Name, Order, Query, QueryPath, Selection, Source,
};

use crate::error::{BulkError, BulkResult};
use crate::format::Format;
use crate::spec::{EXPORT_ALIAS, FieldSet, OrderKey, Part};

/// The right to export a DocType, supplied by the caller's `PermissionEngine`.
///
/// The engine has no `Action::Export` (its `Action` is a lifecycle operation or `Read`), so the
/// host answers the DocType-level `export` right here, wrapping its `PermissionEngine`. The
/// plan decides nothing itself: it only refuses when this says no.
pub trait ExportAuthority: Send + Sync {
    /// Whether `principal` holds the `export` right on `doctype`.
    fn may_export(&self, principal: &Principal, doctype: &str) -> bool;
}

/// An authority that allows everyone; a fake for tests and for a host that checks elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllExport;

impl ExportAuthority for AllowAllExport {
    fn may_export(&self, _principal: &Principal, _doctype: &str) -> bool {
        true
    }
}

/// An authority that allows nobody; a fake for tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllExport;

impl ExportAuthority for DenyAllExport {
    fn may_export(&self, _principal: &Principal, _doctype: &str) -> bool {
        false
    }
}

/// One planned column: the field path and, when the channel may only show it masked, the mask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanColumn {
    /// The field path (`field`, or `field` of the child DocType for a child part).
    pub path: String,
    /// The mask to apply after the fetch, or `None` for the real value.
    pub mask: Option<MaskFormat>,
}

/// One child table's planned part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildPart {
    /// The parent's table field name.
    pub table: String,
    /// The child DocType's storage name.
    pub doctype: String,
    /// The planned child columns.
    pub columns: Vec<PlanColumn>,
}

/// A checked export.
#[derive(Clone, Debug)]
pub struct ExportPlan {
    /// The DocType.
    pub doctype: String,
    /// The parent columns, in output order.
    pub columns: Vec<PlanColumn>,
    /// The child parts (empty unless children were included).
    pub children: Vec<ChildPart>,
    /// The sort keys.
    pub order: Vec<OrderKey>,
    /// The format.
    pub format: Format,
    /// The row cap.
    pub max_rows: Option<u64>,
    /// Whether CSV formula hardening is on.
    pub hardening: bool,
    /// The stable digest over the whole plan.
    pub digest: [u8; 32],
    /// A digest of the filter that names it without recording its values.
    pub filter_digest: String,
    /// Whether the read filter leaves nothing (the stream is empty without querying).
    pub read_none: bool,
    filter: Option<Expr>,
    alias: Name,
    child_alias: Name,
    owner_field: Name,
    parent_select: Vec<Selection>,
    parent_index: BTreeMap<String, usize>,
    child_select: BTreeMap<String, ChildSelect>,
}

/// The select list and index of one child part.
#[derive(Clone, Debug)]
struct ChildSelect {
    select: Vec<Selection>,
    index: BTreeMap<String, usize>,
}

impl ExportPlan {
    /// Builds and checks a plan. See the module docs for the four checks.
    ///
    /// # Errors
    ///
    /// `bulk.export_denied` (no export right), `permission.field_denied` (a refused column or a
    /// query field), `bulk.spec_invalid` (an unusable specification) or `fieldguard.unknown_doctype`.
    pub fn build(
        spec: &crate::spec::ExportSpec,
        principal: &Principal,
        guard: &FieldGuard,
        authority: &dyn ExportAuthority,
        fields: &FieldSet,
        prot: &dyn ProtectionSource,
    ) -> BulkResult<ExportPlan> {
        if spec.doctype != fields.doctype {
            return Err(BulkError::spec_invalid(format!(
                "the spec exports `{}` but the field set is `{}`",
                spec.doctype, fields.doctype
            )));
        }
        if !authority.may_export(principal, &spec.doctype) {
            return Err(BulkError::export_denied(&spec.doctype));
        }
        let requested: Vec<String> = if spec.columns.is_empty() {
            fields.fields.clone()
        } else {
            spec.columns.clone()
        };
        for path in &requested {
            if path.contains('.') {
                return Err(BulkError::spec_invalid(format!(
                    "`{path}` is a child-table column; child tables are exported as their own parts with `include_children`"
                )));
            }
        }
        let columns = plan_columns(guard, prot, principal, &spec.doctype, &requested)?;

        let order = effective_order(spec, fields);
        for key in &order {
            if key.field.contains('.') {
                return Err(BulkError::spec_invalid(format!(
                    "the order key `{}` is not a plain field of `{}`",
                    key.field, spec.doctype
                )));
            }
        }
        // Every name the plan will use must be a valid storage identifier up front, so the
        // query builder never has to guess.
        for n in requested
            .iter()
            .map(String::as_str)
            .chain(order.iter().map(|k| k.field.as_str()))
        {
            name(n)?;
        }
        for child in &fields.children {
            name(&child.table)?;
            name(&child.doctype)?;
            for c in &child.columns {
                name(c)?;
            }
        }
        name(&fields.owner_field)?;

        // The no-inference rule: a field the filter or the sort uses must be readable unmasked
        // and reportable (the `report` right, not `read`).
        let mut used: Vec<QueryField> = Vec::new();
        if let Some(filter) = &spec.filter {
            collect_fields(filter, QueryUse::Filter, &mut used);
        }
        for key in &order {
            used.push(QueryField::new(&key.field, QueryUse::Sort));
        }
        guard.check_query_fields_for(Channel::Export, principal, &spec.doctype, &used)?;

        let alias = name(EXPORT_ALIAS)?;
        let child_alias = name(crate::spec::CHILD_ALIAS)?;
        let owner_field = name(&fields.owner_field)?;
        let target = ReadTarget {
            alias: alias.clone(),
            owner_field: owner_field.clone(),
        };
        let read = PermissionFilter::for_read(guard.engine(), principal, &spec.doctype, &target)
            .map_err(|e| BulkError::spec_invalid(e.to_string()))?;
        let (read_none, permission_filter) = match read {
            ReadFilter::None => (true, None),
            ReadFilter::All => (false, None),
            ReadFilter::Filter(p) => (false, Some(p)),
        };
        let filter = and(spec.filter.clone(), permission_filter);

        let parent_select = select_list(&alias, parent_select_names(&columns, &order));
        let parent_index = index_of(&parent_select);

        let mut children = Vec::new();
        let mut child_select = BTreeMap::new();
        if spec.include_children {
            for child in &fields.children {
                let cols = plan_columns(guard, prot, principal, &child.doctype, &child.columns)?;
                let mut part_columns = vec![
                    PlanColumn {
                        path: crate::spec::PARENT_COLUMN.to_owned(),
                        mask: None,
                    },
                    PlanColumn {
                        path: crate::spec::IDX_COLUMN.to_owned(),
                        mask: None,
                    },
                ];
                part_columns.extend(cols);
                let names: Vec<String> = part_columns.iter().map(|c| c.path.clone()).collect();
                let select = select_list(&child_alias, names);
                child_select.insert(
                    child.table.clone(),
                    ChildSelect {
                        index: index_of(&select),
                        select,
                    },
                );
                children.push(ChildPart {
                    table: child.table.clone(),
                    doctype: child.doctype.clone(),
                    columns: part_columns,
                });
            }
        }

        let digest = plan_digest(spec, &columns, &children, &order, read_none);
        let filter_digest = filter_digest(spec.filter.as_ref());
        Ok(ExportPlan {
            doctype: spec.doctype.clone(),
            columns,
            children,
            order,
            format: spec.format,
            max_rows: spec.max_rows,
            hardening: spec.harden_csv,
            digest,
            filter_digest,
            read_none,
            filter,
            alias,
            child_alias,
            owner_field,
            parent_select,
            parent_index,
            child_select,
        })
    }

    /// The parts a stream yields, in order.
    #[must_use]
    pub fn parts(&self) -> Vec<Part> {
        let mut parts = vec![Part::Parent];
        parts.extend(self.children.iter().map(|c| Part::Child(c.table.clone())));
        parts
    }

    /// The export column names of `part`, in output order.
    #[must_use]
    pub fn column_names(&self, part: &Part) -> Vec<String> {
        match part {
            Part::Parent => self.columns.iter().map(|c| c.path.clone()).collect(),
            Part::Child(table) => self
                .children
                .iter()
                .find(|c| &c.table == table)
                .map(|c| c.columns.iter().map(|c| c.path.clone()).collect())
                .unwrap_or_default(),
        }
    }

    /// The position of `name` in the parent part's select list.
    #[must_use]
    pub fn parent_index(&self, name: &str) -> Option<usize> {
        self.parent_index.get(name).copied()
    }

    /// The position of `name` in a child part's select list.
    #[must_use]
    pub fn child_index(&self, table: &str, name: &str) -> Option<usize> {
        self.child_select
            .get(table)
            .and_then(|s| s.index.get(name).copied())
    }

    /// The alias the parent query uses.
    #[must_use]
    pub fn alias(&self) -> &Name {
        &self.alias
    }

    /// The owner field the read filter compares with the user.
    #[must_use]
    pub fn owner_field(&self) -> &Name {
        &self.owner_field
    }

    /// The parent page query, with an optional keyset predicate and a row limit.
    #[must_use]
    pub fn parent_query(&self, keyset: Option<Expr>, limit: u32) -> Query {
        let mut q = Query::from(
            &name_or(&self.doctype),
            &self.alias,
            QueryPath::Transactional,
        );
        q.select = self.parent_select.clone();
        q.filter = and(self.filter.clone(), keyset);
        q.order_by = self
            .order
            .iter()
            .map(|k| Order {
                expr: Expr::field(&self.alias, &name_or(&k.field)),
                dir: k.dir,
                nulls: k.nulls,
            })
            .collect();
        q.limit = Some(limit);
        q
    }

    /// A child part's page query, with an optional keyset predicate and a row limit.
    ///
    /// The query joins the parent entity so the principal's read filter still applies: a child
    /// row is exported only when its parent is one the caller may read **and** one the export's
    /// own row filter selects (the parent's whole filter, permission and caller filter alike).
    #[must_use]
    pub fn child_query(&self, table: &str, keyset: Option<Expr>, limit: u32) -> Option<Query> {
        let child = self.children.iter().find(|c| c.table == table)?;
        let select = self.child_select.get(table)?.select.clone();
        let mut q = Query::from(
            &name_or(&child.doctype),
            &self.child_alias,
            QueryPath::Transactional,
        );
        if self.filter.is_some() {
            let parent = Source {
                entity: name_or(&self.doctype),
                alias: self.alias.clone(),
            };
            q.joins.push(Join {
                kind: JoinKind::Inner,
                source: parent,
                on: Expr::Cmp(
                    Box::new(Expr::field(
                        &self.child_alias,
                        &name_or(crate::spec::PARENT_COLUMN),
                    )),
                    CmpOp::Eq,
                    Box::new(Expr::field(&self.alias, &name_or("id"))),
                ),
            });
        }
        q.select = select;
        q.filter = and(self.filter.clone(), keyset);
        q.order_by = vec![
            Order {
                expr: Expr::field(&self.child_alias, &name_or(crate::spec::PARENT_COLUMN)),
                dir: spot_storage_contract::OrderDir::Asc,
                nulls: spot_storage_contract::Nulls::Last,
            },
            Order {
                expr: Expr::field(&self.child_alias, &name_or(crate::spec::IDX_COLUMN)),
                dir: spot_storage_contract::OrderDir::Asc,
                nulls: spot_storage_contract::Nulls::Last,
            },
        ];
        q.limit = Some(limit);
        Some(q)
    }

    /// The parent keyset predicate from the last delivered row's key values (order keys then
    /// the record id).
    #[must_use]
    pub fn parent_keyset(&self, keys: &[spot_storage_contract::Value]) -> Option<Expr> {
        if keys.is_empty() {
            return None;
        }
        Some(crate::page::after_predicate(&self.alias, &self.order, keys))
    }

    /// The child keyset predicate from a parent id and a child row position.
    #[must_use]
    pub fn child_keyset(&self, parent: spot_storage_contract::RecordId, idx: i64) -> Expr {
        let keys = [
            spot_storage_contract::Value::Id(parent),
            spot_storage_contract::Value::Int(idx),
        ];
        crate::page::after_predicate_exact(
            &self.child_alias,
            &[
                OrderKey {
                    field: crate::spec::PARENT_COLUMN.to_owned(),
                    dir: spot_storage_contract::OrderDir::Asc,
                    nulls: spot_storage_contract::Nulls::Last,
                },
                OrderKey {
                    field: crate::spec::IDX_COLUMN.to_owned(),
                    dir: spot_storage_contract::OrderDir::Asc,
                    nulls: spot_storage_contract::Nulls::Last,
                },
            ],
            &keys,
        )
    }

    /// The parent query for a set of ids: the plan's columns and filter, restricted to `ids`.
    /// Used by incremental extraction to load the current state of changed documents.
    #[must_use]
    pub fn parent_query_by_ids(&self, ids: &[spot_storage_contract::RecordId]) -> Query {
        let mut q = Query::from(
            &name_or(&self.doctype),
            &self.alias,
            QueryPath::Transactional,
        );
        q.select = self.parent_select.clone();
        if ids.is_empty() {
            q.filter = Some(Expr::Or(Vec::new()));
        } else {
            let id_in = Expr::In(
                Box::new(Expr::field(&self.alias, &name_or("id"))),
                ids.iter()
                    .copied()
                    .map(spot_storage_contract::Value::Id)
                    .collect(),
            );
            q.filter = and(self.filter.clone(), Some(id_in));
        }
        q.limit = Some(u32::try_from(ids.len()).unwrap_or(u32::MAX).max(1));
        q
    }

    /// The values of the export columns of one parent row (masked where planned). The row must
    /// come from this plan's parent query.
    #[must_use]
    pub fn parent_export_values(
        &self,
        row: &[spot_storage_contract::Value],
    ) -> Vec<spot_storage_contract::Value> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, column)| {
                let value = row
                    .get(i)
                    .cloned()
                    .unwrap_or(spot_storage_contract::Value::Null);
                match &column.mask {
                    None => value,
                    Some(format) => match value {
                        spot_storage_contract::Value::Text(text) => {
                            spot_storage_contract::Value::Text(spot_permissions::mask(
                                &text, format,
                            ))
                        }
                        _ => spot_storage_contract::Value::Null,
                    },
                }
            })
            .collect()
    }
}

/// The child part named `table`, if any.
#[must_use]
pub fn child_part<'a>(plan: &'a ExportPlan, table: &str) -> Option<&'a ChildPart> {
    plan.children.iter().find(|c| c.table == table)
}

fn name(s: &str) -> BulkResult<Name> {
    Name::new(s).map_err(|_| BulkError::spec_invalid(format!("`{s}` is not a valid storage name")))
}

/// A name built from text `build` already validated. The fallback is a valid storage name, so
/// this never panics; an invalid field set is refused as `bulk.spec_invalid` before it is used.
fn name_or(s: &str) -> Name {
    match Name::new(s) {
        Ok(n) => n,
        Err(_) => match Name::new("x") {
            Ok(n) => n,
            Err(_) => unreachable!("`x` is a valid storage name"),
        },
    }
}

fn effective_order(spec: &crate::spec::ExportSpec, fields: &FieldSet) -> Vec<OrderKey> {
    if !spec.order.is_empty() {
        return spec.order.clone();
    }
    if fields.has_field("modified") {
        vec![OrderKey::asc("modified")]
    } else {
        Vec::new()
    }
}

fn parent_select_names(columns: &[PlanColumn], order: &[OrderKey]) -> Vec<String> {
    let mut names: Vec<String> = columns.iter().map(|c| c.path.clone()).collect();
    for key in order {
        names.push(key.field.clone());
    }
    names.push("id".to_owned());
    dedup(names)
}

fn dedup(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for n in names {
        if seen.insert(n.clone()) {
            out.push(n);
        }
    }
    out
}

fn select_list(alias: &Name, names: Vec<String>) -> Vec<Selection> {
    names
        .into_iter()
        .map(|n| Selection {
            expr: Expr::field(alias, &name_or(&n)),
            alias: name_or(&n),
        })
        .collect()
}

fn index_of(select: &[Selection]) -> BTreeMap<String, usize> {
    select
        .iter()
        .enumerate()
        .map(|(i, s)| (s.alias.as_str().to_owned(), i))
        .collect()
}

/// Classifies `requested` for the export channel and resolves each masked column's mask.
fn plan_columns(
    guard: &FieldGuard,
    prot: &dyn ProtectionSource,
    principal: &Principal,
    doctype: &str,
    requested: &[String],
) -> BulkResult<Vec<PlanColumn>> {
    let columns = guard.columns(Channel::Export, principal, doctype, requested)?;
    let mut out = Vec::new();
    for path in &columns.allowed {
        out.push(PlanColumn {
            path: path.clone(),
            mask: None,
        });
    }
    for path in &columns.masked {
        out.push(PlanColumn {
            path: path.clone(),
            mask: Some(mask_format(guard, prot, principal, doctype, path)),
        });
    }
    Ok(out)
}

fn mask_format(
    guard: &FieldGuard,
    prot: &dyn ProtectionSource,
    principal: &Principal,
    doctype: &str,
    path: &str,
) -> MaskFormat {
    let facts = prot
        .protection(doctype)
        .map(|p| p.field(path))
        .unwrap_or_else(|| FieldFacts::plain(path));
    let doc = DocFacts {
        doctype: doctype.to_owned(),
        ..DocFacts::default()
    };
    match guard
        .engine()
        .field_access(principal, doctype, &facts, &doc)
        .level
    {
        FieldLevel::Masked(format) => format,
        _ => MaskFormat::Placeholder,
    }
}

/// Collects every field reference in `expr` as a query field.
fn collect_fields(expr: &Expr, usage: QueryUse, out: &mut Vec<QueryField>) {
    match expr {
        Expr::Field(FieldRef { field, .. }) => out.push(QueryField::new(field.as_str(), usage)),
        Expr::Value(_) => {}
        Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => {
            collect_fields(a, usage, out);
            collect_fields(b, usage, out);
        }
        Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => {
            for e in v {
                collect_fields(e, usage, out);
            }
        }
        Expr::Not(e)
        | Expr::IsNull(e)
        | Expr::In(e, _)
        | Expr::Text(e, _)
        | Expr::Geo(e, _)
        | Expr::Range(e, _) => {
            collect_fields(e, usage, out);
        }
        // ADR-087: a search term names fields of the `from` entity. No backend serves search
        // yet; its permission rules (ADR-087 section 7.7) must land before one does.
        Expr::Search(_) | Expr::SearchRank(_) => {}
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg {
                collect_fields(a, usage, out);
            }
        }
    }
}

fn and(a: Option<Expr>, b: Option<Expr>) -> Option<Expr> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(Expr::And(vec![a, b])),
    }
}

fn plan_digest(
    spec: &crate::spec::ExportSpec,
    columns: &[PlanColumn],
    children: &[ChildPart],
    order: &[OrderKey],
    read_none: bool,
) -> [u8; 32] {
    let mut s = String::new();
    s.push_str(&spec.doctype);
    s.push('|');
    s.push_str(spec.format.as_str());
    s.push('|');
    s.push_str(if read_none { "none" } else { "some" });
    s.push('|');
    s.push_str(&format!("{:?}", spec.max_rows));
    s.push('|');
    s.push_str(if spec.include_children {
        "children"
    } else {
        "flat"
    });
    s.push('|');
    for c in columns {
        s.push_str(&c.path);
        s.push_str(&format!(":{:?},", c.mask));
    }
    for child in children {
        s.push_str(&child.table);
        s.push('(');
        for c in &child.columns {
            s.push_str(&c.path);
            s.push_str(&format!(":{:?},", c.mask));
        }
        s.push_str(");");
    }
    s.push('|');
    for k in order {
        s.push_str(&format!("{}:{:?}:{:?},", k.field, k.dir, k.nulls));
    }
    s.push('|');
    s.push_str(&format!("{:?}", spec.filter));
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hasher.finalize().into()
}

fn filter_digest(filter: Option<&Expr>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{filter:?}").as_bytes());
    let out: [u8; 32] = hasher.finalize().into();
    out[..8].iter().map(|b| format!("{b:02x}")).collect()
}
