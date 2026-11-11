//! Expression compilation. Every compound result is parenthesised or a function call, so
//! results compose without precedence surprises.

use spot_storage_contract::{
    AggFunc, ArithOp, CmpOp, ErrorKind, Expr, StorageError, StorageResult, TextMatch, TextMode,
    Value,
};

use crate::compile::{Builder, invalid};
use crate::types::{ColumnType, Param};

/// The byte-wise collation used wherever the contract demands exactness.
const EXACT: &str = "\"C\"";

/// A compiled expression: its SQL, and its type when known (`None` only for a bare null).
pub(crate) struct Compiled {
    pub(crate) sql: String,
    pub(crate) ty: Option<ColumnType>,
    /// The SQL already ends in an explicit `COLLATE "C"`.
    exact: bool,
}

impl Compiled {
    pub(crate) fn new(sql: String, ty: ColumnType) -> Self {
        Self {
            sql,
            ty: Some(ty),
            exact: false,
        }
    }

    fn is_text(&self) -> bool {
        self.ty == Some(ColumnType::Text)
    }
}

fn is_null_literal(e: &Expr) -> bool {
    matches!(e, Expr::Value(Value::Null))
}

fn cmp_sql(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

impl<'q> Builder<'q> {
    /// `sql COLLATE "C"`: byte-wise, exact.
    pub(crate) fn exact(&self, c: &Compiled) -> String {
        if c.exact {
            c.sql.clone()
        } else {
            format!("{} COLLATE {EXACT}", c.sql)
        }
    }

    /// `sql COLLATE <tenant collation>`.
    pub(crate) fn collated(&self, c: &Compiled) -> String {
        format!("{} COLLATE {}", c.sql, self.collation)
    }

    /// A boolean clause (`WHERE`, `ON`, `HAVING`).
    pub(crate) fn condition(&mut self, e: &'q Expr, what: &str) -> StorageResult<String> {
        let c = self.expr(e)?;
        match c.ty {
            Some(ColumnType::Bool) => Ok(c.sql),
            other => Err(invalid(format!("a {what} must be boolean, not {other:?}"))),
        }
    }

    /// Compiles `e`. A text expression that is a grouping key is emitted with an explicit
    /// `COLLATE "C"`, exactly as `GROUP BY` emits it, so PostgreSQL sees the same expression.
    pub(crate) fn expr(&mut self, e: &'q Expr) -> StorageResult<Compiled> {
        let c = self.expr_inner(e)?;
        if c.is_text() && !c.exact && self.grouped.contains(e) {
            return Ok(Compiled {
                sql: format!("({} COLLATE {EXACT})", c.sql),
                ty: c.ty,
                exact: true,
            });
        }
        Ok(c)
    }

    /// Compiles `e`; a null literal takes `hint` as its type.
    fn expr_hint(&mut self, e: &'q Expr, hint: Option<ColumnType>) -> StorageResult<Compiled> {
        if is_null_literal(e) {
            let ty = hint.unwrap_or(ColumnType::Text);
            let p = self.param(Param::Null(ty.pg_type()));
            return Ok(Compiled {
                sql: p,
                ty: hint,
                exact: false,
            });
        }
        self.expr(e)
    }

    /// Compiles two operands, typing a null literal after the other operand.
    fn pair(&mut self, a: &'q Expr, b: &'q Expr) -> StorageResult<(Compiled, Compiled)> {
        if is_null_literal(a) && !is_null_literal(b) {
            let rb = self.expr(b)?;
            let ra = self.expr_hint(a, rb.ty)?;
            return Ok((ra, rb));
        }
        let ra = self.expr(a)?;
        let rb = self.expr_hint(b, ra.ty)?;
        Ok((ra, rb))
    }

    pub(crate) fn expr_inner(&mut self, e: &'q Expr) -> StorageResult<Compiled> {
        match e {
            Expr::Field(r) => {
                let ty = self.field_type(&r.alias, &r.field)?;
                Ok(Compiled::new(self.column(&r.alias, &r.field)?, ty))
            }
            Expr::Value(v) => self.value(v),
            Expr::Cmp(a, op, b) => self.cmp(a, *op, b),
            Expr::And(v) => self.junction(v, " AND ", "TRUE"),
            Expr::Or(v) => self.junction(v, " OR ", "FALSE"),
            Expr::Not(x) => {
                let c = self.boolean(x)?;
                Ok(Compiled::new(format!("(NOT {})", c.sql), ColumnType::Bool))
            }
            Expr::IsNull(x) => {
                let c = self.expr(x)?;
                Ok(Compiled::new(
                    format!("({} IS NULL)", c.sql),
                    ColumnType::Bool,
                ))
            }
            Expr::In(x, values) => self.in_list(x, values),
            Expr::Text(x, m) => self.text_match(x, m),
            Expr::Arith(a, op, b) => self.arith(a, *op, b),
            Expr::Agg {
                func,
                arg,
                distinct,
            } => self.aggregate(*func, arg.as_deref(), *distinct),
            Expr::Coalesce(v) => self.coalesce(v),
            Expr::Geo(x, test) => self.geo(x, test),
            Expr::Range(x, test) => self.range(x, test),
            Expr::Search(term) => self.search(term),
            // Reached only outside an order_by key (the ordering path handles it first).
            Expr::SearchRank(_) => Err(invalid("SearchRank is only valid as a whole order_by key")),
        }
    }

    fn value(&mut self, v: &Value) -> StorageResult<Compiled> {
        let (param, ty) = match v {
            Value::Null => (Param::Null(ColumnType::Text.pg_type()), None),
            Value::Bool(b) => (Param::Bool(*b), Some(ColumnType::Bool)),
            Value::Int(i) => (Param::Int8(*i), Some(ColumnType::Int)),
            Value::Decimal(d) => (
                Param::Numeric(*d),
                Some(ColumnType::Decimal {
                    scale: Some(d.scale()),
                }),
            ),
            Value::Text(s) => {
                if s.contains('\0') {
                    return Err(StorageError::new(
                        ErrorKind::InvalidValue,
                        "PostgreSQL text cannot contain NUL",
                    ));
                }
                (Param::Text(s.clone()), Some(ColumnType::Text))
            }
            Value::Date(d) => (Param::Date(*d), Some(ColumnType::Date)),
            Value::Time(t) => (Param::Time(*t), Some(ColumnType::Time)),
            Value::DateTime(t) => (Param::TimestampTz(*t), Some(ColumnType::DateTime)),
            Value::Id(id) => (Param::Uuid(*id), Some(ColumnType::Id)),
            Value::Json(j) => (Param::Jsonb(j.clone()), Some(ColumnType::Json)),
            Value::Bytes(b) => (Param::Bytea(b.clone()), Some(ColumnType::Bytes)),
            Value::GeoPoint(_) | Value::Range(_) => {
                return Err(StorageError::new(
                    ErrorKind::Unsupported,
                    "this compiler does not bind geo or range values yet (ADR-087)",
                ));
            }
        };
        Ok(Compiled {
            sql: self.param(param),
            ty,
            exact: false,
        })
    }

    /// An operand of `NOT`, `AND` or `OR`.
    fn boolean(&mut self, e: &'q Expr) -> StorageResult<Compiled> {
        let c = self.expr(e)?;
        match c.ty {
            Some(ColumnType::Bool) => Ok(c),
            other => Err(invalid(format!(
                "a logical operand must be boolean, not {other:?}"
            ))),
        }
    }

    fn junction(&mut self, v: &'q [Expr], op: &str, empty: &str) -> StorageResult<Compiled> {
        let mut parts = Vec::new();
        for e in v {
            parts.push(self.boolean(e)?.sql);
        }
        let sql = if parts.is_empty() {
            empty.to_owned()
        } else {
            format!("({})", parts.join(op))
        };
        Ok(Compiled::new(sql, ColumnType::Bool))
    }

    /// Casts an integer expression to `numeric` when it meets a decimal.
    fn to_numeric(c: Compiled) -> Compiled {
        Compiled::new(
            format!("CAST({} AS numeric)", c.sql),
            ColumnType::Decimal { scale: Some(0) },
        )
    }

    /// Checks that two operands can meet, and casts the integer side to `numeric` when the
    /// other is a decimal.
    fn unify(a: Compiled, b: Compiled) -> StorageResult<(Compiled, Compiled)> {
        match (a.ty, b.ty) {
            (Some(x), Some(y)) if !x.compatible(y) => {
                Err(invalid(format!("cannot combine {x:?} with {y:?}")))
            }
            (Some(ColumnType::Int), Some(ColumnType::Decimal { .. })) => {
                Ok((Self::to_numeric(a), b))
            }
            (Some(ColumnType::Decimal { .. }), Some(ColumnType::Int)) => {
                Ok((a, Self::to_numeric(b)))
            }
            _ => Ok((a, b)),
        }
    }

    fn cmp(&mut self, a: &'q Expr, op: CmpOp, b: &'q Expr) -> StorageResult<Compiled> {
        let (l, r) = self.pair(a, b)?;
        let (l, r) = Self::unify(l, r)?;
        let sym = cmp_sql(op);
        let sql = if l.is_text() && r.is_text() {
            match op {
                // Equality is exact whatever the column's collation.
                CmpOp::Eq | CmpOp::Ne => format!("({} {sym} {})", self.exact(&l), self.exact(&r)),
                // Ranges follow the tenant collation (deterministic: ties are by code point).
                CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
                    format!("({} {sym} {})", self.collated(&l), self.collated(&r))
                }
            }
        } else {
            format!("({} {sym} {})", l.sql, r.sql)
        };
        Ok(Compiled::new(sql, ColumnType::Bool))
    }

    fn in_list(&mut self, x: &'q Expr, values: &[Value]) -> StorageResult<Compiled> {
        let operand = self.expr(x)?;
        if values.is_empty() {
            return Ok(Compiled::new("FALSE".into(), ColumnType::Bool));
        }
        let any_decimal = values.iter().any(|v| matches!(v, Value::Decimal(_)));
        let cast_operand = operand.ty == Some(ColumnType::Int) && any_decimal;
        let mut items = Vec::new();
        for v in values {
            let c = self.value(v)?;
            if let (Some(a), Some(b)) = (operand.ty, c.ty)
                && !a.compatible(b)
            {
                return Err(invalid(format!("cannot compare {a:?} with {b:?} in IN")));
            }
            let cast_value = matches!(v, Value::Int(_))
                && (cast_operand || matches!(operand.ty, Some(ColumnType::Decimal { .. })));
            items.push(if cast_value {
                Self::to_numeric(c).sql
            } else {
                c.sql
            });
        }
        let operand = if cast_operand {
            Self::to_numeric(operand)
        } else {
            operand
        };
        let left = if operand.is_text() {
            self.exact(&operand)
        } else {
            operand.sql
        };
        Ok(Compiled::new(
            format!("({left} IN ({}))", items.join(", ")),
            ColumnType::Bool,
        ))
    }

    fn text_match(&mut self, x: &'q Expr, m: &TextMatch) -> StorageResult<Compiled> {
        let operand = self.expr(x)?;
        if !operand.is_text() {
            return Err(invalid(format!(
                "text matching needs a text operand, not {:?}",
                operand.ty
            )));
        }
        if m.pattern.contains('\0') {
            return Err(StorageError::new(
                ErrorKind::InvalidValue,
                "PostgreSQL text cannot contain NUL",
            ));
        }
        let pattern = self.param(Param::Text(m.pattern.clone()));
        // Operands are compared byte-wise. Case folding uses the tenant collation, because
        // `lower` under "C" would only fold ASCII.
        let (text, pat) = if m.case_insensitive {
            (
                format!(
                    "lower({} COLLATE {}) COLLATE {EXACT}",
                    operand.sql, self.collation
                ),
                format!(
                    "lower({pattern} COLLATE {}) COLLATE {EXACT}",
                    self.collation
                ),
            )
        } else {
            (self.exact(&operand), format!("{pattern} COLLATE {EXACT}"))
        };
        let sql = match m.mode {
            TextMode::Equals => format!("({text} = {pat})"),
            TextMode::Prefix => format!("starts_with({text}, {pat})"),
            TextMode::Contains => format!("(strpos({text}, {pat}) > 0)"),
            TextMode::Suffix => {
                format!("(right({text}, char_length({pat})) COLLATE {EXACT} = {pat})")
            }
        };
        Ok(Compiled::new(sql, ColumnType::Bool))
    }

    fn arith(&mut self, a: &'q Expr, op: ArithOp, b: &'q Expr) -> StorageResult<Compiled> {
        let (l, r) = self.pair(a, b)?;
        for c in [&l, &r] {
            if c.ty.is_some_and(|t| !t.is_number()) {
                return Err(invalid(format!("arithmetic needs numbers, not {:?}", c.ty)));
            }
        }
        let scales = (l.ty, r.ty);
        let (l, r) = Self::unify(l, r)?;
        let sym = match op {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
        };
        let ty = match scales {
            (Some(ColumnType::Int), Some(ColumnType::Int)) => ColumnType::Int,
            _ => {
                let scale_of = |t: Option<ColumnType>| match t {
                    Some(ColumnType::Int) => Some(0),
                    Some(ColumnType::Decimal { scale }) => scale,
                    _ => None,
                };
                let scale = match (scale_of(scales.0), scale_of(scales.1)) {
                    (Some(x), Some(y)) => Some(match op {
                        ArithOp::Mul => x + y,
                        ArithOp::Add | ArithOp::Sub => x.max(y),
                    }),
                    _ => None,
                };
                ColumnType::Decimal { scale }
            }
        };
        Ok(Compiled::new(format!("({} {sym} {})", l.sql, r.sql), ty))
    }

    fn coalesce(&mut self, v: &'q [Expr]) -> StorageResult<Compiled> {
        if v.is_empty() {
            return Err(invalid("COALESCE needs at least one operand"));
        }
        // Compile typed operands first so null literals can take their type.
        let mut slots: Vec<Option<Compiled>> = Vec::new();
        for e in v {
            slots.push(if is_null_literal(e) {
                None
            } else {
                Some(self.expr(e)?)
            });
        }
        let mut ty: Option<ColumnType> = None;
        for c in slots.iter().flatten() {
            ty = match (ty, c.ty) {
                (None, t) => t,
                (Some(a), Some(b)) if !a.compatible(b) => {
                    return Err(invalid(format!("COALESCE mixes {a:?} and {b:?}")));
                }
                (Some(ColumnType::Int), Some(d @ ColumnType::Decimal { .. }))
                | (Some(d @ ColumnType::Decimal { .. }), Some(ColumnType::Int)) => {
                    Some(ColumnType::Decimal {
                        scale: match d {
                            ColumnType::Decimal { scale } => scale,
                            _ => None,
                        },
                    })
                }
                (
                    Some(ColumnType::Decimal { scale: a }),
                    Some(ColumnType::Decimal { scale: b }),
                ) => Some(ColumnType::Decimal {
                    scale: a.zip(b).map(|(x, y)| x.max(y)),
                }),
                (Some(a), Some(_)) => Some(a),
                (Some(a), None) => Some(a),
            };
        }
        let to_decimal = matches!(ty, Some(ColumnType::Decimal { .. }));
        let mut parts = Vec::new();
        for (e, slot) in v.iter().zip(slots) {
            let c = match slot {
                Some(c) => c,
                None => self.expr_hint(e, ty)?,
            };
            parts.push(if to_decimal && c.ty == Some(ColumnType::Int) {
                Self::to_numeric(c).sql
            } else {
                c.sql
            });
        }
        Ok(Compiled {
            sql: format!("COALESCE({})", parts.join(", ")),
            ty,
            exact: false,
        })
    }

    fn aggregate(
        &mut self,
        func: AggFunc,
        arg: Option<&'q Expr>,
        distinct: bool,
    ) -> StorageResult<Compiled> {
        let Some(arg) = arg else {
            // Validation guarantees this is `Count`.
            return Ok(Compiled::new("COUNT(*)".into(), ColumnType::Int));
        };
        let c = self.expr(arg)?;
        let Some(ty) = c.ty else {
            return Err(invalid("an aggregate needs a typed argument"));
        };
        match func {
            AggFunc::Count => {
                let inner = if distinct && c.is_text() {
                    // Distinct text is exact.
                    format!("DISTINCT {}", self.exact(&c))
                } else if distinct {
                    format!("DISTINCT {}", c.sql)
                } else {
                    c.sql
                };
                Ok(Compiled::new(format!("COUNT({inner})"), ColumnType::Int))
            }
            AggFunc::Sum => {
                if !ty.is_number() {
                    return Err(invalid(format!("SUM needs numbers, not {ty:?}")));
                }
                let d = if distinct { "DISTINCT " } else { "" };
                Ok(match ty {
                    // `SUM(int8)` is `numeric` in PostgreSQL; the contract keeps integers.
                    // The cast fails with 22003 (InvalidValue) outside the `int8` range.
                    ColumnType::Int => {
                        Compiled::new(format!("CAST(SUM({d}{}) AS int8)", c.sql), ColumnType::Int)
                    }
                    _ => Compiled::new(format!("SUM({d}{})", c.sql), ty),
                })
            }
            AggFunc::Min | AggFunc::Max => self.min_max(func == AggFunc::Min, c, ty),
        }
    }

    /// `MIN`/`MAX`, ignoring nulls, null over no values. Types PostgreSQL has no aggregate for
    /// are ordered through `array_agg`.
    fn min_max(&mut self, min: bool, c: Compiled, ty: ColumnType) -> StorageResult<Compiled> {
        let name = if min { "MIN" } else { "MAX" };
        let sql = match ty {
            ColumnType::Text => format!("{name}({})", self.collated(&c)),
            ColumnType::Int
            | ColumnType::Decimal { .. }
            | ColumnType::Date
            | ColumnType::Time
            | ColumnType::DateTime => format!("{name}({})", c.sql),
            // false < true: the minimum is "all true", the maximum "any true".
            ColumnType::Bool => format!("{}({})", if min { "bool_and" } else { "bool_or" }, c.sql),
            ColumnType::Id | ColumnType::Bytes => format!(
                "(array_agg({x} ORDER BY {x} {dir}) FILTER (WHERE {x} IS NOT NULL))[1]",
                x = c.sql,
                dir = if min { "ASC" } else { "DESC" }
            ),
            ColumnType::Json => {
                return Err(invalid("MIN and MAX are not defined for JSON"));
            }
        };
        Ok(Compiled::new(sql, ty))
    }
}
