//! The compiled report: everything a run needs but the principal.
//!
//! [`CompiledReport`] is the compiler's output and the runner's input. It carries the resolved
//! columns, joins, filter tree, grouping, ordering and limits, and builds the IR
//! [`Query`](spot_storage_contract::Query) at run time with the caller's bound parameters. It
//! never carries a principal: the runner ANDs the permission filter on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_expr::Expr as LangExpr;
use spot_storage_contract::{
    AggFunc, CmpOp, Expr, Join, JoinKind, Name, Nulls, Order, OrderDir, Query, QueryPath,
    Selection, Source, Value,
};

use crate::definition::{AggKind, BASE_ALIAS, ParamType};
use crate::encode::FieldType;
use crate::error::{ReportError, codes};

/// The field the owner rules compare with the acting user, as the document engine uses it.
pub const CREATED_BY: &str = "created_by";

/// A resolved field: its owning DocType, the alias it is read under, and its types.
#[derive(Clone, Debug)]
pub struct FieldPath {
    /// The DocType the field belongs to.
    pub entity: String,
    /// The alias in the compiled query.
    pub alias: Name,
    /// The field name.
    pub field: Name,
}

/// The options a parameter offers (`ReportParameter.options`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamOptions {
    /// No options.
    None,
    /// A fixed list, for a `select`.
    Values(Vec<String>),
    /// Candidates from a DocType, for a `link`.
    Link(String),
}

/// One compiled parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledParameter {
    /// The name a request keys by.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The parameter type.
    pub ty: ParamType,
    /// Must be supplied.
    pub required: bool,
    /// The value used when none is supplied.
    pub default: Option<Value>,
    /// The options the description offers.
    pub options: ParamOptions,
    /// The name filters bind by.
    pub binding: String,
}

impl CompiledParameter {
    /// A value of this parameter's type, for compile-time query validation.
    fn placeholder(&self) -> Value {
        match self.ty {
            ParamType::Text => Value::Text(String::new()),
            ParamType::Int => Value::Int(0),
            ParamType::Decimal => Value::Decimal(rust_decimal::Decimal::ZERO),
            ParamType::Date => Value::Date(
                time::Date::from_calendar_date(1970, time::Month::January, 1)
                    .unwrap_or(time::Date::MIN),
            ),
            ParamType::Link => Value::Id(spot_storage_contract::RecordId::new_v7()),
            ParamType::Select => Value::Text(
                match &self.options {
                    ParamOptions::Values(values) => values.first().cloned(),
                    _ => None,
                }
                .unwrap_or_default(),
            ),
        }
    }
}

/// How one output column is produced.
#[derive(Clone, Debug)]
pub enum ColumnOrigin {
    /// A plain field of the base or a joined alias.
    Field(FieldPath),
    /// An aggregate over a field.
    Aggregate {
        /// The aggregated field.
        path: FieldPath,
        /// The function.
        func: AggKind,
    },
    /// An exact average, returned as a `[sum, count]` decimal pair.
    Avg {
        /// The averaged field.
        path: FieldPath,
    },
    /// A computed column, by index into [`CompiledReport::computed`].
    Computed(usize),
}

/// One output column.
#[derive(Clone, Debug)]
pub struct CompiledColumn {
    /// The storage name (`ReportColumn.name`).
    pub name: String,
    /// Human label.
    pub label: String,
    /// The contract type.
    pub field_type: FieldType,
    /// A format hint, when declared.
    pub format: Option<String>,
    /// How the column is produced.
    pub origin: ColumnOrigin,
}

impl CompiledColumn {
    /// The field this column reads, when it reads one (not a computed column).
    #[must_use]
    pub fn field_path(&self) -> Option<&FieldPath> {
        match &self.origin {
            ColumnOrigin::Field(path)
            | ColumnOrigin::Aggregate { path, .. }
            | ColumnOrigin::Avg { path } => Some(path),
            ColumnOrigin::Computed(_) => None,
        }
    }

    /// Whether the column aggregates.
    #[must_use]
    pub fn is_aggregate(&self) -> bool {
        matches!(
            self.origin,
            ColumnOrigin::Aggregate { .. } | ColumnOrigin::Avg { .. }
        )
    }
}

/// One computed column: its expression, the columns it reads and its result type.
#[derive(Clone, Debug)]
pub struct CompiledComputed {
    /// The output name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The result's contract type.
    pub field_type: FieldType,
    /// A format hint, when declared.
    pub format: Option<String>,
    /// The parsed expression.
    pub expr: Arc<LangExpr>,
    /// The output column names it reads.
    pub inputs: Vec<String>,
}

/// One join.
#[derive(Clone, Debug)]
pub struct CompiledJoin {
    /// The alias.
    pub alias: Name,
    /// The joined DocType.
    pub doctype: Name,
    /// The source whose link field points at the join.
    pub from_alias: Name,
    /// The link field.
    pub link_field: Name,
}

/// One sort key.
#[derive(Clone, Debug)]
pub struct CompiledOrder {
    /// The field path, or `None` when the key is an aggregate column.
    pub path: FieldPath,
    /// The direction.
    pub dir: OrderDir,
    /// Where nulls go.
    pub nulls: Nulls,
    /// The aggregate the key orders by, when it names an aggregate column.
    pub agg: Option<AggKind>,
}

/// The right side of a comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rhs {
    /// A parameter, by filter binding name.
    Param(String),
    /// A literal.
    Value(Value),
}

/// One node of the compiled filter tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompiledFilter {
    /// Every child must hold.
    All(Vec<CompiledFilter>),
    /// At least one child must hold.
    Any(Vec<CompiledFilter>),
    /// The child must not hold.
    Not(Box<CompiledFilter>),
    /// One comparison.
    Cmp {
        /// The DocType the field belongs to.
        entity: String,
        /// The alias.
        alias: Name,
        /// The field.
        field: Name,
        /// The operator.
        op: CmpOp,
        /// The right side.
        rhs: Rhs,
    },
}

impl CompiledFilter {
    /// The IR expression, or `None` when an optional parameter was not supplied (the branch
    /// drops).
    fn to_expr(&self, resolve: &dyn Fn(&str) -> Option<Value>) -> Option<Expr> {
        match self {
            CompiledFilter::All(children) => {
                let mut parts = Vec::new();
                for c in children {
                    if let Some(e) = c.to_expr(resolve) {
                        parts.push(e);
                    }
                }
                match parts.len() {
                    0 => None,
                    1 => parts.pop(),
                    _ => Some(Expr::And(parts)),
                }
            }
            CompiledFilter::Any(children) => {
                let mut parts = Vec::new();
                for c in children {
                    if let Some(e) = c.to_expr(resolve) {
                        parts.push(e);
                    }
                }
                match parts.len() {
                    0 => Some(Expr::Value(Value::Bool(false))),
                    1 => parts.pop(),
                    _ => Some(Expr::Or(parts)),
                }
            }
            CompiledFilter::Not(inner) => inner.to_expr(resolve).map(|e| Expr::Not(Box::new(e))),
            CompiledFilter::Cmp {
                alias,
                field,
                op,
                rhs,
                ..
            } => {
                let value = match rhs {
                    Rhs::Value(v) => v.clone(),
                    Rhs::Param(name) => resolve(name)?,
                };
                Some(Expr::field(alias, field).cmp(*op, Expr::Value(value)))
            }
        }
    }
}

/// A report compiled against one metadata version: everything a run needs but the principal.
#[derive(Clone, Debug)]
pub struct CompiledReport {
    /// The report's name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The base DocType.
    pub base: Name,
    /// The roles that may see the report exists.
    pub roles: Vec<String>,
    /// The default row limit.
    pub default_limit: u32,
    /// The largest row limit a run honours.
    pub max_rows: u32,
    /// The parameters.
    pub parameters: Vec<CompiledParameter>,
    /// The output columns, in result order.
    pub columns: Vec<CompiledColumn>,
    /// The computed columns.
    pub computed: Vec<CompiledComputed>,
    /// The joins.
    pub joins: Vec<CompiledJoin>,
    /// The grouping keys.
    pub group_by: Vec<FieldPath>,
    /// The sort keys.
    pub order: Vec<CompiledOrder>,
    /// The filter tree, when the report declares one.
    pub filter: Option<CompiledFilter>,
    /// The metadata version this was compiled at.
    pub meta_version: u64,
    /// The content digest of the DocTypes this was compiled against.
    ///
    /// With `meta_version` it is the report's metadata identity
    /// ([`crate::MetaIdentity`]): a version on its own does not promise the same DocTypes.
    pub meta_digest: String,
}

impl CompiledReport {
    /// The base alias.
    #[must_use]
    pub fn base_alias(&self) -> Name {
        Name::new(BASE_ALIAS).unwrap_or_else(|_| self.base.clone())
    }

    /// Builds the report's own query, binding parameters through `resolve` (which returns
    /// `None` for an optional parameter that was not supplied, dropping that filter branch).
    ///
    /// # Errors
    ///
    /// A residual build error (an alias or name that is not a valid [`Name`]).
    pub fn build_query(
        &self,
        resolve: &dyn Fn(&str) -> Option<Value>,
    ) -> Result<Query, ReportError> {
        let base_alias = self.base_alias();
        let mut query = Query::from(&self.base, &base_alias, QueryPath::Report);
        query.select = self.select()?;
        query.joins = self.join_exprs()?;
        query.filter = self.filter.as_ref().and_then(|f| f.to_expr(resolve));
        query.group_by = self
            .group_by
            .iter()
            .map(|p| Expr::field(&p.alias, &p.field))
            .collect();
        query.order_by = self
            .order
            .iter()
            .map(|o| self.order_expr(o))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(query)
    }

    /// The IR select list: one entry per data column, two for an `avg` pair.
    fn select(&self) -> Result<Vec<Selection>, ReportError> {
        let mut out = Vec::new();
        for column in &self.columns {
            match &column.origin {
                ColumnOrigin::Field(path) => out.push(Selection {
                    expr: Expr::field(&path.alias, &path.field),
                    alias: name(&column.name)?,
                }),
                ColumnOrigin::Aggregate { path, func } => out.push(Selection {
                    expr: aggregate_expr(*func, path),
                    alias: name(&column.name)?,
                }),
                ColumnOrigin::Avg { path } => {
                    out.push(Selection {
                        expr: aggregate_expr(AggKind::Sum, path),
                        alias: name(&format!("{}__sum", column.name))?,
                    });
                    out.push(Selection {
                        expr: aggregate_expr(AggKind::Count, path),
                        alias: name(&format!("{}__count", column.name))?,
                    });
                }
                ColumnOrigin::Computed(_) => {}
            }
        }
        Ok(out)
    }

    fn join_exprs(&self) -> Result<Vec<Join>, ReportError> {
        let mut out = Vec::new();
        for join in &self.joins {
            out.push(Join {
                kind: JoinKind::Left,
                source: Source {
                    entity: join.doctype.clone(),
                    alias: join.alias.clone(),
                },
                on: Expr::field(&join.from_alias, &join.link_field)
                    .eq(Expr::field(&join.alias, &name("id")?)),
            });
        }
        Ok(out)
    }

    fn order_expr(&self, order: &CompiledOrder) -> Result<Order, ReportError> {
        let expr = match order.agg {
            Some(func) => aggregate_expr(func, &order.path),
            None => Expr::field(&order.path.alias, &order.path.field),
        };
        Ok(Order {
            expr,
            dir: order.dir,
            nulls: order.nulls,
        })
    }

    /// The placeholder resolver: every parameter bound to a value of its type, so the full
    /// filter tree (no branch dropped) validates.
    #[must_use]
    pub fn placeholders(&self) -> BTreeMap<String, Value> {
        self.parameters
            .iter()
            .map(|p| (p.binding.clone(), p.placeholder()))
            .collect()
    }

    /// Whether `principal_roles` admits this report: the system sees all; otherwise any role
    /// in common.
    #[must_use]
    pub fn admits(&self, is_system: bool, roles: &BTreeSet<String>) -> bool {
        is_system || self.roles.iter().any(|r| roles.contains(r))
    }
}

fn name(s: &str) -> Result<Name, ReportError> {
    Name::new(s).map_err(|e| ReportError::new(codes::INVALID_DEFINITION, e.to_string()))
}

fn aggregate_expr(func: AggKind, path: &FieldPath) -> Expr {
    let storage = match func {
        AggKind::Sum => AggFunc::Sum,
        AggKind::Count => AggFunc::Count,
        AggKind::Min => AggFunc::Min,
        AggKind::Max => AggFunc::Max,
        AggKind::Avg => AggFunc::Sum,
    };
    Expr::Agg {
        func: storage,
        arg: Some(Box::new(Expr::field(&path.alias, &path.field))),
        distinct: false,
    }
}
