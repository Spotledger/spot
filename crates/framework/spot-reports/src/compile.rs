//! The compiler: a [`ReportDef`](crate::definition::ReportDef) and the effective metadata to a
//! [`CompiledReport`].
//!
//! Every field is resolved against the effective metadata (custom fields included), every
//! parameter is type-checked against the field it filters, and the IR [`Query`] is built and
//! validated under ADR-069's limits. Every problem is reported at once. A report whose base
//! DocType the loading site does not have is [`codes::BASE_MISSING`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::meta::FieldKind;
use spot_expr::Expr as LangExpr;
use spot_storage_contract::{Name, Nulls, OrderDir, Value};

use crate::compiled::{
    ColumnOrigin, CompiledColumn, CompiledComputed, CompiledFilter, CompiledJoin, CompiledOrder,
    CompiledParameter, CompiledReport, FieldPath, ParamOptions, Rhs,
};
use crate::definition::{
    AggKind, BASE_ALIAS, CompareOp, ComputedDef, FilterNode, ParamType, ReportDef,
};
use crate::encode::FieldType;
use crate::error::{ReportError, codes};
use crate::meta::EffectiveMetaSet;
use crate::walk;

/// The compiler: metadata and the storage catalog, held for one build.
pub struct Compiler<'a> {
    meta: &'a EffectiveMetaSet,
    catalog: &'a spot_storage_contract::Catalog,
}

impl<'a> Compiler<'a> {
    /// A compiler over one metadata set and the catalog compiled from it.
    #[must_use]
    pub fn new(meta: &'a EffectiveMetaSet, catalog: &'a spot_storage_contract::Catalog) -> Self {
        Compiler { meta, catalog }
    }

    /// The metadata a report is compiled against.
    ///
    /// A cache keys a compiled report by this set's [`EffectiveMetaSet::identity`], never by its
    /// version alone (see [`crate::ReportCache`]).
    #[must_use]
    pub fn meta(&self) -> &'a EffectiveMetaSet {
        self.meta
    }

    /// Compiles one definition, reporting every problem at once.
    ///
    /// # Errors
    ///
    /// Every [`ReportError`] the definition and its fields produce.
    pub fn compile(&self, def: &ReportDef) -> Result<CompiledReport, Vec<ReportError>> {
        let mut errors = def.validate();
        if !errors.is_empty() {
            return Err(errors);
        }
        let Ok(base) = Name::new(&def.base) else {
            return Err(vec![ReportError::at(
                codes::INVALID_DEFINITION,
                "base",
                "the base is not a valid DocType name",
            )]);
        };
        let Some(base_def) = self.meta.doctype(&def.base) else {
            return Err(vec![ReportError::at(
                codes::BASE_MISSING,
                "base",
                format!("the site has no DocType `{}`", def.base),
            )]);
        };
        let base_alias = Name::new(BASE_ALIAS).unwrap_or_else(|_| base.clone());

        // Joins.
        let mut joins: Vec<CompiledJoin> = Vec::new();
        let mut alias_entities: BTreeMap<String, String> = BTreeMap::new();
        alias_entities.insert(BASE_ALIAS.to_owned(), def.base.clone());
        for (i, join) in def.joins.iter().enumerate() {
            let at = format!("joins[{i}]");
            let Some(join_def) = self.meta.doctype(&join.doctype) else {
                errors.push(ReportError::at(
                    codes::BASE_MISSING,
                    &at,
                    format!("the site has no DocType `{}`", join.doctype),
                ));
                continue;
            };
            let (from_alias, link_field) = split_path(&join.on);
            let from_entity = alias_entities.get(&from_alias).cloned();
            let Some(from_entity) = from_entity else {
                errors.push(ReportError::at(
                    codes::UNKNOWN_JOIN,
                    &at,
                    format!("`{from_alias}` is not the base or an earlier join"),
                ));
                continue;
            };
            let link_ok = self
                .meta
                .doctype(&from_entity)
                .and_then(|d| d.field(&link_field).map(|f| f.kind.clone()));
            let link_ok = match link_ok {
                Some(FieldKind::Link { doctype }) => doctype.as_str() == join.doctype,
                Some(FieldKind::DynamicLink { .. }) => true,
                _ => false,
            };
            if !link_ok {
                errors.push(ReportError::at(
                    codes::BAD_TYPE,
                    &at,
                    format!(
                        "`{from_entity}.{link_field}` is not a link to `{}`",
                        join.doctype
                    ),
                ));
                continue;
            }
            let _ = join_def;
            let Ok(alias) = Name::new(&join.alias) else {
                continue;
            };
            let Ok(doctype) = Name::new(&join.doctype) else {
                continue;
            };
            let Ok(field) = Name::new(&link_field) else {
                continue;
            };
            alias_entities.insert(join.alias.clone(), join.doctype.clone());
            joins.push(CompiledJoin {
                alias,
                doctype,
                from_alias: Name::new(&from_alias).unwrap_or_else(|_| base_alias.clone()),
                link_field: field,
            });
        }

        // Parameters.
        let mut parameters: Vec<CompiledParameter> = Vec::new();
        for (i, p) in def.parameters.iter().enumerate() {
            let at = format!("parameters[{i}]");
            let Some(ty) = ParamType::parse(&p.ty) else {
                continue;
            };
            let default = match &p.default {
                Some(value) => match crate::definition::literal_to_value(value) {
                    Ok(v) => Some(v),
                    Err(_) => continue,
                },
                None => None,
            };
            let options = match ty {
                ParamType::Select => ParamOptions::Values(p.options.clone().unwrap_or_default()),
                ParamType::Link => ParamOptions::Link(p.doctype.clone().unwrap_or_default()),
                _ => ParamOptions::None,
            };
            let _ = at;
            parameters.push(CompiledParameter {
                name: p.name.clone(),
                label: p.label.clone(),
                ty,
                required: p.required,
                default,
                options,
                binding: p.for_filter.clone().unwrap_or_else(|| p.name.clone()),
            });
        }
        let params_by_binding: BTreeMap<String, &CompiledParameter> =
            parameters.iter().map(|p| (p.binding.clone(), p)).collect();

        // Columns.
        let mut columns: Vec<CompiledColumn> = Vec::new();
        for (i, column) in def.columns.iter().enumerate() {
            let at = format!("columns[{i}]");
            let path =
                match self.resolve(&column.field, &def.base, &alias_entities, &at, &mut errors) {
                    Some(p) => p,
                    None => continue,
                };
            let info = self.field_info(&path, &mut errors, &at);
            let Some(info) = info else { continue };
            let label = column.label.clone().unwrap_or_else(|| column.field.clone());
            match column.aggregate.as_deref() {
                None => columns.push(CompiledColumn {
                    name: crate::definition::default_column_name(&column.field),
                    label,
                    field_type: info.contract,
                    format: column.format.clone(),
                    origin: ColumnOrigin::Field(path),
                }),
                Some(agg) => {
                    let Some(kind) = AggKind::parse(agg) else {
                        continue;
                    };
                    if let Err(message) = aggregate_arg_ok(kind, &info) {
                        errors.push(ReportError::at(codes::BAD_TYPE, &at, message));
                        continue;
                    }
                    let field_type = if kind == AggKind::Avg {
                        FieldType::Json
                    } else if kind == AggKind::Count {
                        FieldType::Int
                    } else {
                        info.contract.clone()
                    };
                    columns.push(CompiledColumn {
                        name: crate::definition::default_column_name(&column.field),
                        label,
                        field_type,
                        format: column.format.clone(),
                        origin: match kind {
                            AggKind::Avg => ColumnOrigin::Avg { path },
                            _ => ColumnOrigin::Aggregate { path, func: kind },
                        },
                    });
                }
            }
        }

        // Computed columns.
        let mut computed: Vec<CompiledComputed> = Vec::new();
        let mut output_names: BTreeSet<String> = columns.iter().map(|c| c.name.clone()).collect();
        for (i, c) in def.computed.iter().enumerate() {
            let at = format!("computed[{i}]");
            let Some(cc) = self.compile_computed(c, &at, &output_names, &mut errors) else {
                continue;
            };
            output_names.insert(cc.name.clone());
            computed.push(cc);
        }
        for (index, c) in computed.iter().enumerate() {
            columns.push(CompiledColumn {
                name: c.name.clone(),
                label: c.label.clone(),
                field_type: c.field_type.clone(),
                format: c.format.clone(),
                origin: ColumnOrigin::Computed(index),
            });
        }

        // Filter tree.
        let filter = def.filters.as_ref().and_then(|node| {
            self.compile_filter(
                node,
                &def.base,
                &alias_entities,
                &params_by_binding,
                "filters",
                &mut errors,
            )
        });

        // Grouping and ordering.
        let mut group_by: Vec<FieldPath> = Vec::new();
        for (i, g) in def.group_by.iter().enumerate() {
            if let Some(path) = self.resolve(
                g,
                &def.base,
                &alias_entities,
                &format!("group_by[{i}]"),
                &mut errors,
            ) && self
                .field_info(&path, &mut errors, &format!("group_by[{i}]"))
                .is_some()
            {
                group_by.push(path);
            }
        }
        let mut order: Vec<CompiledOrder> = Vec::new();
        for (i, o) in def.order_by.iter().enumerate() {
            let at = format!("order_by[{i}]");
            let dir = match o.dir.as_deref() {
                Some("desc") => OrderDir::Desc,
                _ => OrderDir::Asc,
            };
            let nulls = match o.nulls.as_deref() {
                Some("first") => Nulls::First,
                Some("last") => Nulls::Last,
                _ if dir == OrderDir::Desc => Nulls::Last,
                _ => Nulls::First,
            };
            // An aggregate column is sorted by its aggregate; a computed column cannot be
            // sorted in storage.
            if let Some(column) = columns.iter().find(|c| c.name == o.field) {
                match &column.origin {
                    ColumnOrigin::Computed(_) => {
                        errors.push(ReportError::at(
                            codes::INVALID_DEFINITION,
                            &at,
                            "a computed column cannot be a sort key (it is evaluated after the query)",
                        ));
                        continue;
                    }
                    ColumnOrigin::Aggregate { path, func } => {
                        order.push(CompiledOrder {
                            path: path.clone(),
                            dir,
                            nulls,
                            agg: Some(*func),
                        });
                        continue;
                    }
                    ColumnOrigin::Avg { path } => {
                        order.push(CompiledOrder {
                            path: path.clone(),
                            dir,
                            nulls,
                            agg: Some(AggKind::Sum),
                        });
                        continue;
                    }
                    ColumnOrigin::Field(path) => {
                        order.push(CompiledOrder {
                            path: path.clone(),
                            dir,
                            nulls,
                            agg: None,
                        });
                        continue;
                    }
                }
            }
            if let Some(path) = self.resolve(&o.field, &def.base, &alias_entities, &at, &mut errors)
                && self.field_info(&path, &mut errors, &at).is_some()
            {
                order.push(CompiledOrder {
                    path,
                    dir,
                    nulls,
                    agg: None,
                });
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let report = CompiledReport {
            name: def.name.clone(),
            label: def.label.clone(),
            base,
            roles: def.roles.clone(),
            default_limit: def.default_limit(),
            max_rows: def.max_rows(),
            parameters,
            columns,
            computed,
            joins,
            group_by,
            order,
            filter,
            meta_version: self.meta.version(),
            meta_digest: self.meta.digest().to_owned(),
        };

        // Build the representative query and validate it under ADR-069's limits.
        let placeholders = report.placeholders();
        let resolve = |binding: &str| placeholders.get(binding).cloned();
        let probe = match report.build_query(&resolve) {
            Ok(q) => q,
            Err(e) => return Err(vec![e]),
        };
        if let Err(e) = walk::check_limits(&probe) {
            return Err(vec![e]);
        }
        if let Err(e) = probe.validate(self.catalog) {
            return Err(vec![ReportError::new(
                codes::INVALID_DEFINITION,
                e.to_string(),
            )]);
        }
        let _ = base_def;
        Ok(report)
    }

    fn compile_computed(
        &self,
        c: &ComputedDef,
        at: &str,
        output_names: &BTreeSet<String>,
        errors: &mut Vec<ReportError>,
    ) -> Option<CompiledComputed> {
        let Some(ty) = c.ty.as_deref().and_then(FieldType::parse) else {
            errors.push(ReportError::at(
                codes::BAD_TYPE,
                at,
                "a computed column needs its `type`",
            ));
            return None;
        };
        let expr = match spot_expr::parse(&c.expr) {
            Ok(e) => e,
            Err(e) => {
                errors.push(ReportError::at(codes::EXPRESSION, at, e.to_string()));
                return None;
            }
        };
        let mut inputs = Vec::new();
        let mut bad = false;
        for path in lang_paths(&expr) {
            let Some((root, parts)) = path else {
                errors.push(ReportError::at(
                    codes::EXPRESSION,
                    at,
                    "a computed expression reads fields as `doc.<column>`",
                ));
                bad = true;
                continue;
            };
            if root != spot_expr::Root::Doc || parts.len() != 1 {
                errors.push(ReportError::at(
                    codes::EXPRESSION,
                    at,
                    "a computed expression reads a column as `doc.<column>`",
                ));
                bad = true;
                continue;
            }
            let name = parts[0].clone();
            if !output_names.contains(&name) {
                errors.push(ReportError::at(
                    codes::UNKNOWN_FIELD,
                    at,
                    format!("`{name}` is not a column of this report"),
                ));
                bad = true;
                continue;
            }
            if !inputs.contains(&name) {
                inputs.push(name);
            }
        }
        if bad {
            return None;
        }
        Some(CompiledComputed {
            name: c.name.clone(),
            label: c.label.clone().unwrap_or_else(|| c.name.clone()),
            field_type: ty,
            format: c.format.clone(),
            expr: Arc::new(expr),
            inputs,
        })
    }

    fn compile_filter(
        &self,
        node: &FilterNode,
        base: &str,
        aliases: &BTreeMap<String, String>,
        params: &BTreeMap<String, &CompiledParameter>,
        at: &str,
        errors: &mut Vec<ReportError>,
    ) -> Option<CompiledFilter> {
        match node {
            FilterNode::All(group) => {
                let mut children = Vec::new();
                for (i, child) in group.all.iter().enumerate() {
                    if let Some(c) = self.compile_filter(
                        child,
                        base,
                        aliases,
                        params,
                        &format!("{at}.all[{i}]"),
                        errors,
                    ) {
                        children.push(c);
                    }
                }
                Some(CompiledFilter::All(children))
            }
            FilterNode::Any(group) => {
                let mut children = Vec::new();
                for (i, child) in group.any.iter().enumerate() {
                    if let Some(c) = self.compile_filter(
                        child,
                        base,
                        aliases,
                        params,
                        &format!("{at}.any[{i}]"),
                        errors,
                    ) {
                        children.push(c);
                    }
                }
                Some(CompiledFilter::Any(children))
            }
            FilterNode::Not(group) => self
                .compile_filter(
                    &group.not,
                    base,
                    aliases,
                    params,
                    &format!("{at}.not"),
                    errors,
                )
                .map(|c| CompiledFilter::Not(Box::new(c))),
            FilterNode::Cmp(cmp) => {
                let op = CompareOp::parse(&cmp.op)?;
                let path = self.resolve(&cmp.field, base, aliases, at, errors)?;
                let info = self.field_info(&path, errors, at)?;
                let rhs = if let Some(param) = &cmp.param {
                    let Some(p) = params.get(param) else {
                        errors.push(ReportError::at(
                            codes::UNKNOWN_PARAMETER,
                            at,
                            format!("`{param}` is not a parameter of this report"),
                        ));
                        return None;
                    };
                    if let Err(message) = param_fits_field(p, &info) {
                        errors.push(ReportError::at(codes::BAD_TYPE, at, message));
                        return None;
                    }
                    Rhs::Param(p.binding.clone())
                } else if let Some(value) = &cmp.value {
                    match crate::definition::literal_to_value(value) {
                        Ok(v) => {
                            if !value_comparable(&v, &info.contract) {
                                errors.push(ReportError::at(
                                    codes::BAD_TYPE,
                                    at,
                                    format!("`{}` cannot be compared with that value", cmp.field),
                                ));
                                return None;
                            }
                            Rhs::Value(v)
                        }
                        Err(message) => {
                            errors.push(ReportError::at(codes::BAD_TYPE, at, message));
                            return None;
                        }
                    }
                } else {
                    return None;
                };
                Some(CompiledFilter::Cmp {
                    entity: path.entity.clone(),
                    alias: path.alias,
                    field: path.field,
                    op: op.to_cmp(),
                    rhs,
                })
            }
        }
    }

    fn resolve(
        &self,
        path: &str,
        base: &str,
        aliases: &BTreeMap<String, String>,
        at: &str,
        errors: &mut Vec<ReportError>,
    ) -> Option<FieldPath> {
        let (alias, field) = split_path(path);
        let Some(entity) = aliases.get(&alias).cloned() else {
            errors.push(ReportError::at(
                codes::UNKNOWN_JOIN,
                at,
                format!("`{alias}` is not the base or a declared join"),
            ));
            return None;
        };
        let _ = base;
        let alias_name = Name::new(&alias).ok()?;
        let field_name = Name::new(&field).ok()?;
        Some(FieldPath {
            entity,
            alias: alias_name,
            field: field_name,
        })
    }

    fn field_info(
        &self,
        path: &FieldPath,
        errors: &mut Vec<ReportError>,
        at: &str,
    ) -> Option<FieldInfo> {
        let Some(doctype) = self.meta.doctype(&path.entity) else {
            errors.push(ReportError::at(
                codes::BASE_MISSING,
                at,
                format!("the site has no DocType `{}`", path.entity),
            ));
            return None;
        };
        if let Some(contract) = system_type(path.field.as_str()) {
            return Some(FieldInfo {
                contract,
                select_values: None,
                link_target: None,
            });
        }
        let Some(field) = doctype.field(path.field.as_str()) else {
            errors.push(ReportError::at(
                codes::UNKNOWN_FIELD,
                at,
                format!("`{}.{}` is not a field", path.entity, path.field),
            ));
            return None;
        };
        match field_kind_info(&field.kind) {
            Ok(info) => Some(info),
            Err(message) => {
                errors.push(ReportError::at(codes::BAD_TYPE, at, message));
                None
            }
        }
    }
}

/// A resolved field's contract type and the facts a parameter check needs.
#[derive(Clone, Debug)]
struct FieldInfo {
    contract: FieldType,
    select_values: Option<Vec<String>>,
    link_target: Option<String>,
}

fn system_type(field: &str) -> Option<FieldType> {
    Some(match field {
        "id" | "parent" => FieldType::Link,
        "version" | "idx" => FieldType::Int,
        "parent_field" => FieldType::Text,
        _ => return None,
    })
}

fn field_kind_info(kind: &FieldKind) -> Result<FieldInfo, String> {
    let (contract, select_values, link_target) = match kind {
        FieldKind::Bool => (FieldType::Bool, None, None),
        FieldKind::Int { .. } => (FieldType::Int, None, None),
        FieldKind::Decimal { scale, .. } => (FieldType::Decimal { scale: *scale }, None, None),
        FieldKind::Text { .. } | FieldKind::LongText { .. } => (FieldType::Text, None, None),
        FieldKind::Select { values } => (FieldType::Text, Some(values.clone()), None),
        FieldKind::Date => (FieldType::Date, None, None),
        FieldKind::Time => (FieldType::Time, None, None),
        FieldKind::DateTime => (FieldType::DateTime, None, None),
        FieldKind::Link { doctype } => (FieldType::Link, None, Some(doctype.as_str().to_owned())),
        FieldKind::DynamicLink { .. } => (FieldType::Link, None, None),
        FieldKind::Json { .. } => (FieldType::Json, None, None),
        FieldKind::Bytes => (FieldType::Bytes, None, None),
        FieldKind::Attach { .. } => (FieldType::Attach, None, None),
        FieldKind::Secret => (FieldType::Text, None, None),
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {
            return Err("a child table cannot be a report column".to_owned());
        }
        _ => return Err("this field type cannot be a report column".to_owned()),
    };
    Ok(FieldInfo {
        contract,
        select_values,
        link_target,
    })
}

fn param_fits_field(param: &CompiledParameter, info: &FieldInfo) -> Result<(), String> {
    let ok = match param.ty {
        ParamType::Text => {
            matches!(info.contract, FieldType::Text)
        }
        ParamType::Int => matches!(info.contract, FieldType::Int),
        ParamType::Decimal => {
            matches!(info.contract, FieldType::Decimal { .. } | FieldType::Int)
        }
        ParamType::Date => matches!(info.contract, FieldType::Date),
        ParamType::Link => {
            if !matches!(info.contract, FieldType::Link) {
                false
            } else if let ParamOptions::Link(target) = &param.options {
                info.link_target.as_deref().is_none_or(|t| t == target)
            } else {
                true
            }
        }
        ParamType::Select => {
            info.select_values.is_some() || matches!(info.contract, FieldType::Text)
        }
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "a `{}` parameter does not fit that field",
            param.ty.as_str()
        ))
    }
}

fn aggregate_arg_ok(kind: AggKind, info: &FieldInfo) -> Result<(), String> {
    let ok = match kind {
        AggKind::Sum | AggKind::Avg => {
            matches!(info.contract, FieldType::Int | FieldType::Decimal { .. })
        }
        AggKind::Count => true,
        AggKind::Min | AggKind::Max => !matches!(
            info.contract,
            FieldType::Json | FieldType::Bytes | FieldType::Attach
        ),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("`{}` does not fit that aggregate", kind.as_str()))
    }
}

fn value_comparable(value: &Value, ty: &FieldType) -> bool {
    let direct = matches!(
        (value, ty),
        (Value::Bool(_), FieldType::Bool)
            | (Value::Int(_), FieldType::Int)
            | (Value::Decimal(_), FieldType::Decimal { .. })
            | (Value::Text(_), FieldType::Text)
            | (Value::Date(_), FieldType::Date)
            | (Value::Time(_), FieldType::Time)
            | (Value::DateTime(_), FieldType::DateTime)
            | (Value::Id(_), FieldType::Link)
            | (Value::Json(_), FieldType::Json)
            | (Value::Bytes(_), FieldType::Bytes)
    );
    direct
        || matches!(
            (value, ty),
            (Value::Int(_), FieldType::Decimal { .. }) | (Value::Decimal(_), FieldType::Int)
        )
}

/// `alias.field`, or `field` for the base.
fn split_path(path: &str) -> (String, String) {
    match path.split_once('.') {
        Some((alias, field)) => (alias.to_owned(), field.to_owned()),
        None => (BASE_ALIAS.to_owned(), path.to_owned()),
    }
}

/// Every `Path` in a language expression, as `(root, parts)`, or `None` for a malformed path.
fn lang_paths(expr: &LangExpr) -> Vec<Option<(spot_expr::Root, Vec<String>)>> {
    let mut stack = vec![expr];
    let mut out = Vec::new();
    while let Some(e) = stack.pop() {
        match e {
            LangExpr::Path(root, parts) => out.push(Some((*root, parts.clone()))),
            LangExpr::Null
            | LangExpr::Bool(_)
            | LangExpr::Int(_)
            | LangExpr::Decimal(_)
            | LangExpr::Text(_) => {}
            LangExpr::List(items) => stack.extend(items.iter()),
            LangExpr::Unary(_, inner) => stack.push(inner),
            LangExpr::Binary(_, l, r) => {
                stack.push(l);
                stack.push(r);
            }
            LangExpr::Ternary(c, a, b) => {
                stack.push(c);
                stack.push(a);
                stack.push(b);
            }
            LangExpr::Call(_, args) => stack.extend(args.iter()),
        }
    }
    out
}
