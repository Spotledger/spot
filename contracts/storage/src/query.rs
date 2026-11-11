//! The typed query IR (ADR-45): the only way to read records other than loading a document.
//!
//! Backends compile a [`Query`] into their native form with every value bound as a parameter;
//! nothing here is ever pasted into SQL text. Semantics every backend follows:
//!
//! - Comparisons follow SQL three-valued logic: a comparison involving a null is not true, so
//!   the row is filtered out. Comparing with a literal `Value::Null` is rejected; use
//!   [`Expr::IsNull`].
//! - Without aggregation, rows are ordered by `order_by` and then by the `from` record's id, so
//!   results and pages are deterministic.
//! - Text sorts, range comparisons (`<`, `<=`, `>`, `>=`) and text `Min`/`Max` follow the
//!   catalog's [`Collation`](crate::Collation), ties broken by code point. Equality, `In`,
//!   grouping, `distinct` and text matching are exact.
//! - `Sum`, `Min` and `Max` over no rows give null; `Count` gives 0. There is no average and no
//!   division: rounding is data (ADR-12), applied above storage.
//! - Text patterns match literally: `%`, `_` and `\` in a pattern have no special meaning.
//! - A lock applies to the `from` rows only and is not allowed with aggregation.
//! - Geo, range and search tests (ADR-087) are separate operators, not text matching and not
//!   comparisons: `Text` stays literal. A backend answers `Unsupported` for a construct its
//!   [`Capabilities`](crate::Capabilities) do not advertise, after validation, never by
//!   approximating. Ranges are half-open, `[start, end)`. Distances are whole metres and
//!   coordinates are decimals: no float enters the IR, and a rank is only ever an ordering key.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use crate::error::{ErrorKind, StorageError, StorageResult};
use rust_decimal::Decimal;

use crate::id::Name;
use crate::limits::{
    MAX_GEO_RADIUS_M, MAX_RANGE_SPAN_DAYS, MAX_SEARCH_FIELDS, MAX_SEARCH_TERM_CHARS,
};
use crate::locks::RowLock;
use crate::schema::{
    Catalog, EntityDef, EntityKind, FieldType, LinkCheck, LinkTarget, RangeOf, system,
};
use crate::value::{RangePoint, Value, check_coordinate};

/// A read over one entity, optionally joined with others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The main entity.
    pub from: Source,
    /// Further entities, joined in order.
    pub joins: Vec<Join>,
    /// Output columns, at least one.
    pub select: Vec<Selection>,
    /// Row filter.
    pub filter: Option<Expr>,
    /// Grouping keys.
    pub group_by: Vec<Expr>,
    /// Filter on groups; needs aggregation.
    pub having: Option<Expr>,
    /// Sort keys.
    pub order_by: Vec<Order>,
    /// Maximum rows returned.
    pub limit: Option<u32>,
    /// Rows skipped before the first returned.
    pub offset: u64,
    /// Row lock on the `from` rows (inside a transaction only).
    pub lock: RowLock,
    /// Declares whether this query serves a transaction or a report (ADR-45, ADR-47).
    pub path: QueryPath,
}

/// Whether a query runs on a transactional path or serves a report.
///
/// Every filter on a transactional path must be covered by an index; CI verifies this from
/// query plans (ADR-45). Report queries read balance tables, a replica or the analytics mirror
/// (ADR-47) and are exempt from the index rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryPath {
    /// Serves a transaction or a form: must be index-covered.
    Transactional,
    /// Serves a report or analysis.
    Report,
}

/// An entity under an alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The entity.
    pub entity: Name,
    /// The alias used by field references; unique within the query.
    pub alias: Name,
}

/// A joined entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Join {
    /// Inner or left.
    pub kind: JoinKind,
    /// The joined entity.
    pub source: Source,
    /// Join condition.
    pub on: Expr,
}

/// Join kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    /// Only rows with a match.
    Inner,
    /// All left rows; fields of a missing right row read as null.
    Left,
}

/// An output column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// What to compute.
    pub expr: Expr,
    /// Column name in [`Rows::columns`]; unique within the query.
    pub alias: Name,
}

/// A field of an aliased source. System columns (`id`, `version`, `parent`, `parent_field`,
/// `idx`) can be referenced where the entity has them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldRef {
    /// Source alias.
    pub alias: Name,
    /// Field or system column.
    pub field: Name,
}

/// An expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A field value.
    Field(FieldRef),
    /// A literal, always sent as a bound parameter.
    Value(Value),
    /// A comparison.
    Cmp(Box<Expr>, CmpOp, Box<Expr>),
    /// All must hold (true when empty).
    And(Vec<Expr>),
    /// At least one must hold (false when empty).
    Or(Vec<Expr>),
    /// Negation.
    Not(Box<Expr>),
    /// True when the operand is null.
    IsNull(Box<Expr>),
    /// True when the operand equals one of the values (false when the list is empty).
    In(Box<Expr>, Vec<Value>),
    /// Literal text matching on a text operand.
    Text(Box<Expr>, TextMatch),
    /// Addition, subtraction or multiplication of numbers.
    Arith(Box<Expr>, ArithOp, Box<Expr>),
    /// An aggregate. `Count` with no argument counts rows.
    Agg {
        /// The function.
        func: AggFunc,
        /// The aggregated expression.
        arg: Option<Box<Expr>>,
        /// Aggregate distinct values only.
        distinct: bool,
    },
    /// The first non-null operand.
    Coalesce(Vec<Expr>),
    /// A spatial test on a `geo` field (ADR-087 section 4).
    Geo(Box<Expr>, GeoTest),
    /// A range test on a `range` field (ADR-087 section 5).
    Range(Box<Expr>, RangeTest),
    /// A full-text match over the `from` entity's declared search fields (ADR-087 section 7).
    /// True for a row the term matches; a row whose searchable fields are all null never
    /// matches.
    Search(SearchTerm),
    /// The relevance of a search term, as an ordering key. Valid only as an [`Order`]
    /// expression, never in `select`, a filter or a comparison; ties fall back to the `from`
    /// record's id like any other order.
    SearchRank(SearchTerm),
}

/// Spatial tests. Coordinates are decimals (at most seven places) in WGS-84 degrees and a
/// distance is a whole number of metres, measured on the spheroid. A test never returns a
/// distance: the answer is true or false.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GeoTest {
    /// The point is within `metres` of (`lon`, `lat`), at most [`MAX_GEO_RADIUS_M`].
    WithinDistance {
        /// Longitude of the centre, `-180..=180`.
        lon: Decimal,
        /// Latitude of the centre, `-90..=90`.
        lat: Decimal,
        /// Radius in whole metres.
        metres: u32,
    },
    /// The point is inside the box, edges included. `min_lon <= max_lon` and
    /// `min_lat <= max_lat`: a box across the antimeridian is two tests in v1.
    WithinBox {
        /// Western edge.
        min_lon: Decimal,
        /// Southern edge.
        min_lat: Decimal,
        /// Eastern edge.
        max_lon: Decimal,
        /// Northern edge.
        max_lat: Decimal,
    },
}

/// Range tests. Bounds are plain [`Value`]s of the field's kind (`Int`, `Decimal`, `Date` or
/// `DateTime`; a date is never compared with an instant), and every range is half-open,
/// `[start, end)`, so ranges that only touch do not overlap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeTest {
    /// The row's range overlaps `[start, end)`. `start < end`; for dates and instants the span
    /// is at most [`MAX_RANGE_SPAN_DAYS`] days.
    Overlaps {
        /// Start, included.
        start: Value,
        /// End, excluded.
        end: Value,
    },
    /// The row's range contains this value (a point, not a range).
    Contains(Value),
    /// The row's range contains all of `[start, end)`. Same limits as `Overlaps`.
    ContainsRange {
        /// Start, included.
        start: Value,
        /// End, excluded.
        end: Value,
    },
}

/// A search term over the `from` entity's declared search fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchTerm {
    /// How `text` is read.
    pub mode: SearchMode,
    /// What the caller typed, at most [`MAX_SEARCH_TERM_CHARS`] characters and not blank.
    pub text: String,
    /// The searchable fields to match, from 1 to [`MAX_SEARCH_FIELDS`], each declared in the
    /// entity's [`SearchDef`](crate::SearchDef).
    pub fields: Vec<Name>,
}

/// How a search term is parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// A consumer search box: forgiving, quoted phrases and `-` exclusions allowed.
    Web,
    /// Every word, in order, adjacent.
    Phrase,
    /// Every word, in any order.
    Plain,
}

/// Which optional constructs a query uses, for capability checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Constructs {
    /// The query has an [`Expr::Geo`].
    pub geo: bool,
    /// The query has an [`Expr::Range`].
    pub range: bool,
    /// The query has an [`Expr::Search`] or [`Expr::SearchRank`].
    pub search: bool,
}

impl Expr {
    /// `alias.field`.
    pub fn field(alias: &Name, field: &Name) -> Expr {
        Expr::Field(FieldRef {
            alias: alias.clone(),
            field: field.clone(),
        })
    }

    /// A literal.
    pub fn value(v: impl Into<Value>) -> Expr {
        Expr::Value(v.into())
    }

    /// `self <op> other`.
    pub fn cmp(self, op: CmpOp, other: Expr) -> Expr {
        Expr::Cmp(Box::new(self), op, Box::new(other))
    }

    /// `self = other`.
    pub fn eq(self, other: Expr) -> Expr {
        self.cmp(CmpOp::Eq, other)
    }

    fn has_agg(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, Expr::Agg { .. }));
        found
    }

    fn walk(&self, f: &mut impl FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Field(_) | Expr::Value(_) => {}
            Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => v.iter().for_each(|e| e.walk(f)),
            Expr::Not(e)
            | Expr::IsNull(e)
            | Expr::In(e, _)
            | Expr::Text(e, _)
            | Expr::Geo(e, _)
            | Expr::Range(e, _) => e.walk(f),
            Expr::Search(_) | Expr::SearchRank(_) => {}
            Expr::Agg { arg, .. } => {
                if let Some(a) = arg {
                    a.walk(f);
                }
            }
        }
    }
}

/// Comparison operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// Arithmetic operators (no division; see module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
}

/// Aggregate functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// Number of rows or non-null values.
    Count,
    /// Sum of numbers.
    Sum,
    /// Smallest value.
    Min,
    /// Largest value.
    Max,
}

/// Literal text matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextMatch {
    /// Where the pattern must occur.
    pub mode: TextMode,
    /// The literal pattern.
    pub pattern: String,
    /// Ignore case (Unicode simple case folding).
    pub case_insensitive: bool,
}

/// Text matching modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMode {
    /// Whole text equals the pattern.
    Equals,
    /// Text starts with the pattern.
    Prefix,
    /// Text contains the pattern.
    Contains,
    /// Text ends with the pattern.
    Suffix,
}

/// A sort key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    /// What to sort by.
    pub expr: Expr,
    /// Direction.
    pub dir: OrderDir,
    /// Where nulls go; always explicit, never backend-dependent.
    pub nulls: Nulls,
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderDir {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

/// Placement of nulls in a sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nulls {
    /// Nulls before all values.
    First,
    /// Nulls after all values.
    Last,
}

/// Query results.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Rows {
    /// Column names, from the selection aliases.
    pub columns: Vec<Name>,
    /// One vector of values per row, in column order.
    pub rows: Vec<Vec<Value>>,
}

/// Runs queries inside a transaction.
pub trait QueryExecutor: Send {
    /// Validates the query against the transaction's catalog (see [`Query::validate`]), then
    /// runs it and returns all rows.
    fn query(&mut self, query: &Query) -> impl Future<Output = StorageResult<Rows>> + Send;
}

impl Query {
    /// A plain query over `entity` aliased as `alias`, selecting nothing yet.
    pub fn from(entity: &Name, alias: &Name, path: QueryPath) -> Query {
        Query {
            from: Source {
                entity: entity.clone(),
                alias: alias.clone(),
            },
            joins: vec![],
            select: vec![],
            filter: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            limit: None,
            offset: 0,
            lock: RowLock::None,
            path,
        }
    }

    /// Which optional constructs (geo, range, search) the query uses anywhere.
    pub fn constructs(&self) -> Constructs {
        let mut c = Constructs::default();
        let mut see = |e: &Expr| match e {
            Expr::Geo(..) => c.geo = true,
            Expr::Range(..) => c.range = true,
            Expr::Search(_) | Expr::SearchRank(_) => c.search = true,
            _ => {}
        };
        let exprs = self
            .select
            .iter()
            .map(|s| &s.expr)
            .chain(self.joins.iter().map(|j| &j.on))
            .chain(self.filter.iter())
            .chain(self.group_by.iter())
            .chain(self.having.iter())
            .chain(self.order_by.iter().map(|o| &o.expr));
        for e in exprs {
            e.walk(&mut see);
        }
        c
    }

    /// Whether the query aggregates (grouping or any aggregate function).
    pub fn is_aggregate(&self) -> bool {
        !self.group_by.is_empty()
            || self.having.is_some()
            || self.select.iter().any(|s| s.expr.has_agg())
            || self.order_by.iter().any(|o| o.expr.has_agg())
    }

    /// Checks the query against `catalog`. Every backend calls this before running a query, so
    /// all backends reject the same queries with the same kinds.
    pub fn validate(&self, catalog: &Catalog) -> StorageResult<()> {
        let mut aliases: BTreeMap<&Name, &EntityDef> = BTreeMap::new();
        for s in std::iter::once(&self.from).chain(self.joins.iter().map(|j| &j.source)) {
            let e = catalog.entity(&s.entity)?;
            if aliases.insert(&s.alias, e).is_some() {
                return Err(invalid(format!("alias `{}` is used twice", s.alias)));
            }
        }
        let from_entity = catalog.entity(&self.from.entity)?;
        let ctx = Ctx {
            aliases,
            from_entity,
        };
        if self.select.is_empty() {
            return Err(invalid("a query must select at least one column".into()));
        }
        let mut cols = BTreeSet::new();
        for s in &self.select {
            if !cols.insert(&s.alias) {
                return Err(invalid(format!("column alias `{}` is used twice", s.alias)));
            }
            ctx.check(&s.expr, true)?;
        }
        for j in &self.joins {
            ctx.check(&j.on, false)?;
        }
        if let Some(f) = &self.filter {
            ctx.check(f, false)?;
        }
        for g in &self.group_by {
            ctx.check(g, false)?;
        }
        if let Some(h) = &self.having {
            ctx.check(h, true)?;
        }
        for o in &self.order_by {
            // A rank is an ordering key: allowed as a whole sort key, nowhere else.
            match &o.expr {
                Expr::SearchRank(t) => ctx.check_search(t)?,
                other => {
                    ctx.check(other, true)?;
                    ctx.refuse_geo_order(other)?;
                }
            }
        }
        let outside_order = self
            .select
            .iter()
            .map(|s| &s.expr)
            .chain(self.joins.iter().map(|j| &j.on))
            .chain(self.filter.iter())
            .chain(self.group_by.iter())
            .chain(self.having.iter())
            .chain(self.order_by.iter().filter_map(|o| match &o.expr {
                Expr::SearchRank(_) => None,
                other => Some(other),
            }));
        for e in outside_order {
            let mut rank = false;
            e.walk(&mut |x| rank |= matches!(x, Expr::SearchRank(_)));
            if rank {
                return Err(invalid(
                    "SearchRank is only valid as a whole order_by key, never in select, a filter, \
                     grouping or a comparison"
                        .into(),
                ));
            }
        }
        if self.is_aggregate() {
            if self.lock != RowLock::None {
                return Err(invalid(
                    "row locks are not allowed on aggregate queries".into(),
                ));
            }
            let grouped: Vec<&Expr> = self.group_by.iter().collect();
            let outputs = self
                .select
                .iter()
                .map(|s| &s.expr)
                .chain(self.having.iter())
                .chain(self.order_by.iter().map(|o| &o.expr));
            for e in outputs {
                ungrouped_fields(e, &grouped)?;
            }
        }
        Ok(())
    }
}

struct Ctx<'a> {
    aliases: BTreeMap<&'a Name, &'a EntityDef>,
    /// The `from` entity, the one search terms apply to.
    from_entity: &'a EntityDef,
}

impl Ctx<'_> {
    fn field_type(&self, r: &FieldRef) -> StorageResult<FieldType> {
        let e = self
            .aliases
            .get(&r.alias)
            .ok_or_else(|| invalid(format!("unknown alias `{}`", r.alias)))?;
        let is_child = matches!(e.kind, EntityKind::Child { .. });
        let link_self = || FieldType::Link {
            target: LinkTarget::Entity(e.name.clone()),
            check: LinkCheck::Database,
        };
        Ok(match r.field.as_str() {
            system::ID => link_self(),
            system::VERSION if !is_child => FieldType::Int,
            system::IDX if is_child => FieldType::Int,
            system::PARENT_FIELD if is_child => FieldType::Text { max_chars: None },
            system::PARENT => match &e.kind {
                EntityKind::Child { parent } => FieldType::Link {
                    target: LinkTarget::Entity(parent.clone()),
                    check: LinkCheck::Database,
                },
                EntityKind::Document => e.field(&r.field)?.ty.clone(),
            },
            _ => e.field(&r.field)?.ty.clone(),
        })
    }

    fn check(&self, expr: &Expr, agg_allowed: bool) -> StorageResult<()> {
        match expr {
            Expr::Field(r) => self.field_type(r).map(|_| ()),
            Expr::Value(_) => Ok(()),
            Expr::Cmp(a, _, b) => {
                for (x, y) in [(a, b), (b, a)] {
                    if let Expr::Value(v) = x.as_ref() {
                        if v.is_null() {
                            return Err(invalid(
                                "comparison with null is never true; use IsNull".into(),
                            ));
                        }
                        if let Expr::Field(r) = y.as_ref() {
                            let ty = self.field_type(r)?;
                            if !comparable(v, &ty) {
                                return Err(invalid(format!(
                                    "`{}.{}` ({ty:?}) cannot be compared with {v:?}",
                                    r.alias, r.field
                                )));
                            }
                        }
                    }
                }
                self.check(a, agg_allowed)?;
                self.check(b, agg_allowed)
            }
            Expr::In(e, values) => {
                if values.iter().any(Value::is_null) {
                    return Err(invalid("IN lists cannot contain null; use IsNull".into()));
                }
                self.check(e, agg_allowed)
            }
            Expr::Text(e, _) => {
                if let Expr::Field(r) = e.as_ref()
                    && !matches!(self.field_type(r)?, FieldType::Text { .. })
                {
                    return Err(invalid(format!(
                        "text matching needs a text field; `{}.{}` is not text",
                        r.alias, r.field
                    )));
                }
                self.check(e, agg_allowed)
            }
            Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => {
                v.iter().try_for_each(|e| self.check(e, agg_allowed))
            }
            Expr::Not(e) | Expr::IsNull(e) => self.check(e, agg_allowed),
            Expr::Geo(e, test) => {
                self.operand_type(e, "a geo test", |t| matches!(t, FieldType::Geo { .. }))?;
                check_geo_test(test)?;
                self.check(e, agg_allowed)
            }
            Expr::Range(e, test) => {
                let of = match self
                    .operand_type(e, "a range test", |t| matches!(t, FieldType::Range { .. }))?
                {
                    FieldType::Range { of } => of,
                    _ => return Err(invalid("a range test needs a range field".into())),
                };
                check_range_test(of, test)?;
                self.check(e, agg_allowed)
            }
            Expr::Search(t) => self.check_search(t),
            // Reached only outside an order_by key, where `validate` has already refused it.
            Expr::SearchRank(_) => Err(invalid(
                "SearchRank is only valid as a whole order_by key".into(),
            )),
            Expr::Arith(a, _, b) => {
                self.check(a, agg_allowed)?;
                self.check(b, agg_allowed)
            }
            Expr::Agg { func, arg, .. } => {
                if !agg_allowed {
                    return Err(invalid(
                        "aggregates are only allowed in select, having and order by".into(),
                    ));
                }
                match arg {
                    None if *func == AggFunc::Count => Ok(()),
                    None => Err(invalid(format!("{func:?} needs an argument"))),
                    Some(a) if a.has_agg() => Err(invalid("aggregates cannot be nested".into())),
                    Some(a) => {
                        if *func != AggFunc::Count
                            && let Expr::Field(r) = a.as_ref()
                            && matches!(
                                self.field_type(r)?,
                                FieldType::Geo { .. } | FieldType::Range { .. }
                            )
                        {
                            return Err(invalid(format!(
                                "{func:?} is not defined for the geo or range field `{}.{}`",
                                r.alias, r.field
                            )));
                        }
                        self.check(a, false)
                    }
                }
            }
        }
    }
}

impl Ctx<'_> {
    /// The type of `operand`, which must be a field reference whose type passes `ok`.
    fn operand_type(
        &self,
        operand: &Expr,
        what: &str,
        ok: impl Fn(&FieldType) -> bool,
    ) -> StorageResult<FieldType> {
        let Expr::Field(r) = operand else {
            return Err(invalid(format!(
                "{what} needs a field of the matching type"
            )));
        };
        let ty = self.field_type(r)?;
        if ok(&ty) {
            Ok(ty)
        } else {
            Err(invalid(format!(
                "{what} needs a field of the matching type; `{}.{}` is not one",
                r.alias, r.field
            )))
        }
    }

    fn check_search(&self, t: &SearchTerm) -> StorageResult<()> {
        let Some(def) = &self.from_entity.search else {
            return Err(invalid(format!(
                "`{}` declares no search fields; search is opt-in",
                self.from_entity.name
            )));
        };
        if t.text.trim().is_empty() {
            return Err(invalid("a search term must not be blank".into()));
        }
        if t.text.chars().count() > MAX_SEARCH_TERM_CHARS {
            return Err(invalid(format!(
                "a search term is at most {MAX_SEARCH_TERM_CHARS} characters"
            )));
        }
        if t.fields.is_empty() || t.fields.len() > MAX_SEARCH_FIELDS {
            return Err(invalid(format!(
                "a search term names between 1 and {MAX_SEARCH_FIELDS} fields"
            )));
        }
        let mut seen = BTreeSet::new();
        for f in &t.fields {
            if !def.fields.iter().any(|d| &d.name == f) || !seen.insert(f) {
                return Err(invalid(format!(
                    "`{}.{f}` is not a declared searchable field (or is named twice)",
                    self.from_entity.name
                )));
            }
        }
        Ok(())
    }

    /// `order_by` on a geo field is refused in v1: distance ordering is the client's.
    fn refuse_geo_order(&self, key: &Expr) -> StorageResult<()> {
        if let Expr::Field(r) = key
            && matches!(self.field_type(r)?, FieldType::Geo { .. })
        {
            return Err(invalid(format!(
                "`{}.{}` is a geo field and cannot be ordered by; sort by distance on the client",
                r.alias, r.field
            )));
        }
        Ok(())
    }
}

fn check_geo_test(test: &GeoTest) -> StorageResult<()> {
    let coord =
        |what: &str, v: Decimal, limit: i64| check_coordinate(what, v, limit).map_err(invalid);
    match test {
        GeoTest::WithinDistance { lon, lat, metres } => {
            coord("longitude", *lon, 180)?;
            coord("latitude", *lat, 90)?;
            if *metres > MAX_GEO_RADIUS_M {
                return Err(invalid(format!(
                    "a radius of {metres} m is above the limit of {MAX_GEO_RADIUS_M} m"
                )));
            }
        }
        GeoTest::WithinBox {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        } => {
            coord("min_lon", *min_lon, 180)?;
            coord("max_lon", *max_lon, 180)?;
            coord("min_lat", *min_lat, 90)?;
            coord("max_lat", *max_lat, 90)?;
            if min_lon > max_lon || min_lat > max_lat {
                return Err(invalid(
                    "a box needs min_lon <= max_lon and min_lat <= max_lat; split a box across \
                     the antimeridian into two tests"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

fn point_of(of: RangeOf, v: &Value) -> StorageResult<RangePoint> {
    match RangePoint::from_value(v) {
        Some(p) if p.of() == of => Ok(p),
        _ => Err(invalid(format!(
            "a {of:?} range is tested with {of:?} values; got {v:?} (a date is never compared with \
             an instant)"
        ))),
    }
}

fn check_range_test(of: RangeOf, test: &RangeTest) -> StorageResult<()> {
    match test {
        RangeTest::Contains(v) => point_of(of, v).map(|_| ()),
        RangeTest::Overlaps { start, end } | RangeTest::ContainsRange { start, end } => {
            let (a, b) = (point_of(of, start)?, point_of(of, end)?);
            if a.compare(&b) != Some(std::cmp::Ordering::Less) {
                return Err(invalid(
                    "the tested range is empty (start is not before end); ranges are half-open, \
                     [start, end)"
                        .into(),
                ));
            }
            if a.days_until(&b)
                .is_some_and(|d| d > i64::from(MAX_RANGE_SPAN_DAYS))
            {
                return Err(invalid(format!(
                    "the tested range spans more than {MAX_RANGE_SPAN_DAYS} days"
                )));
            }
            Ok(())
        }
    }
}

fn comparable(v: &Value, ty: &FieldType) -> bool {
    v.fits(ty)
        || matches!(
            (v, ty),
            (Value::Int(_), FieldType::Decimal { .. }) | (Value::Decimal(_), FieldType::Int)
        )
}

/// Fields used outside aggregates in an aggregate query must be grouping keys.
fn ungrouped_fields(expr: &Expr, grouped: &[&Expr]) -> StorageResult<()> {
    if grouped.contains(&expr) {
        return Ok(());
    }
    match expr {
        Expr::Agg { .. } | Expr::Value(_) => Ok(()),
        Expr::Field(r) => Err(invalid(format!(
            "`{}.{}` must be aggregated or listed in group by",
            r.alias, r.field
        ))),
        Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => {
            ungrouped_fields(a, grouped)?;
            ungrouped_fields(b, grouped)
        }
        Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => {
            v.iter().try_for_each(|e| ungrouped_fields(e, grouped))
        }
        Expr::Not(e)
        | Expr::IsNull(e)
        | Expr::In(e, _)
        | Expr::Text(e, _)
        | Expr::Geo(e, _)
        | Expr::Range(e, _) => ungrouped_fields(e, grouped),
        Expr::Search(_) | Expr::SearchRank(_) => Err(invalid(
            "a search term is a row-level test and cannot appear in grouped output".into(),
        )),
    }
}

fn invalid(msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidQuery, msg)
}

#[cfg(test)]
mod tests {
    use time::{Date, Month};

    use super::*;
    use crate::contract_tests::{geo_catalog, range_catalog, search_catalog};

    fn n(s: &str) -> Name {
        Name::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn filtered(entity: &str, field: &str, filter: Expr) -> Query {
        let mut q = Query::from(&n(entity), &n("x"), QueryPath::Report);
        q.select = vec![Selection {
            expr: Expr::field(&n("x"), &n(field)),
            alias: n(field),
        }];
        q.filter = Some(filter);
        q
    }

    fn pin() -> Box<Expr> {
        Box::new(Expr::field(&n("x"), &n("pin")))
    }

    fn kind(r: StorageResult<()>) -> Option<ErrorKind> {
        r.err().map(|e| e.kind)
    }

    fn day(d: u8) -> Value {
        Value::Date(
            Date::from_calendar_date(2026, Month::November, d).unwrap_or_else(|e| panic!("{e}")),
        )
    }

    #[test]
    fn geo_tests_validate_coordinates_radius_and_operand() {
        let cat = geo_catalog().unwrap_or_else(|e| panic!("{e}"));
        let within = |lat: Decimal, metres: u32| {
            filtered(
                "place",
                "name",
                Expr::Geo(
                    pin(),
                    GeoTest::WithinDistance {
                        lon: Decimal::ZERO,
                        lat,
                        metres,
                    },
                ),
            )
        };
        assert_eq!(within(Decimal::ZERO, 500_000).validate(&cat), Ok(()));
        assert_eq!(
            kind(within(Decimal::ZERO, 500_001).validate(&cat)),
            Some(ErrorKind::InvalidQuery)
        );
        assert_eq!(
            kind(within(Decimal::from(91), 10).validate(&cat)),
            Some(ErrorKind::InvalidQuery)
        );
        let on_text = filtered(
            "place",
            "name",
            Expr::Geo(
                Box::new(Expr::field(&n("x"), &n("name"))),
                GeoTest::WithinBox {
                    min_lon: Decimal::ZERO,
                    min_lat: Decimal::ZERO,
                    max_lon: Decimal::ONE,
                    max_lat: Decimal::ONE,
                },
            ),
        );
        assert_eq!(kind(on_text.validate(&cat)), Some(ErrorKind::InvalidQuery));
        let mut ordered = within(Decimal::ZERO, 10);
        ordered.order_by = vec![Order {
            expr: Expr::field(&n("x"), &n("pin")),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        assert_eq!(kind(ordered.validate(&cat)), Some(ErrorKind::InvalidQuery));
    }

    #[test]
    fn range_tests_are_half_open_typed_and_bounded() {
        let cat = range_catalog().unwrap_or_else(|e| panic!("{e}"));
        let on = |test| {
            filtered(
                "stay",
                "unit",
                Expr::Range(Box::new(Expr::field(&n("x"), &n("during"))), test),
            )
        };
        let overlaps = |a, b| RangeTest::Overlaps { start: a, end: b };
        assert_eq!(on(overlaps(day(1), day(3))).validate(&cat), Ok(()));
        assert_eq!(on(RangeTest::Contains(day(2))).validate(&cat), Ok(()));
        for bad in [
            overlaps(day(3), day(3)),
            overlaps(day(4), day(3)),
            overlaps(day(1), Value::Int(3)),
            overlaps(day(1), Value::DateTime(time::OffsetDateTime::UNIX_EPOCH)),
            overlaps(Value::Null, day(3)),
            RangeTest::Contains(Value::Null),
            RangeTest::Contains(Value::DateTime(time::OffsetDateTime::UNIX_EPOCH)),
        ] {
            assert_eq!(
                kind(on(bad.clone()).validate(&cat)),
                Some(ErrorKind::InvalidQuery),
                "{bad:?}"
            );
        }
        let far = Value::Date(
            Date::from_calendar_date(2130, Month::November, 1).unwrap_or_else(|e| panic!("{e}")),
        );
        assert_eq!(
            kind(on(overlaps(day(1), far)).validate(&cat)),
            Some(ErrorKind::InvalidQuery),
            "a span of 100 years and more"
        );
    }

    fn term(text: &str, fields: &[&str]) -> SearchTerm {
        SearchTerm {
            mode: SearchMode::Web,
            text: text.into(),
            fields: fields.iter().map(|f| n(f)).collect(),
        }
    }

    #[test]
    fn search_terms_name_declared_fields_and_rank_orders_only() {
        let cat = search_catalog().unwrap_or_else(|e| panic!("{e}"));
        let q = filtered(
            "listing",
            "title",
            Expr::Search(term("harbour", &["title"])),
        );
        assert_eq!(q.validate(&cat), Ok(()));
        for bad in [
            term("", &["title"]),
            term("x", &[]),
            term("x", &["nowhere"]),
            term("x", &["title", "title"]),
            term(&"x".repeat(MAX_SEARCH_TERM_CHARS + 1), &["title"]),
        ] {
            let q = filtered("listing", "title", Expr::Search(bad.clone()));
            assert_eq!(
                kind(q.validate(&cat)),
                Some(ErrorKind::InvalidQuery),
                "{bad:?}"
            );
        }
        // The widest legal term is accepted.
        let widest = filtered(
            "listing",
            "title",
            Expr::Search(term(&"x".repeat(MAX_SEARCH_TERM_CHARS), &["title", "body"])),
        );
        assert_eq!(widest.validate(&cat), Ok(()));
        // A rank orders; it is never selected, filtered on or nested.
        let rank = || Expr::SearchRank(term("harbour", &["title"]));
        let mut ordered = q.clone();
        ordered.order_by = vec![Order {
            expr: rank(),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        }];
        assert_eq!(ordered.validate(&cat), Ok(()));
        let mut selected = q.clone();
        selected.select.push(Selection {
            expr: rank(),
            alias: n("r"),
        });
        assert_eq!(kind(selected.validate(&cat)), Some(ErrorKind::InvalidQuery));
        let filtered_on_rank = filtered("listing", "title", rank().cmp(CmpOp::Gt, Expr::value(0)));
        assert_eq!(
            kind(filtered_on_rank.validate(&cat)),
            Some(ErrorKind::InvalidQuery)
        );
    }

    #[test]
    fn constructs_are_reported_for_capability_checks() {
        let q = filtered(
            "listing",
            "title",
            Expr::Search(term("harbour", &["title"])),
        );
        assert_eq!(
            q.constructs(),
            Constructs {
                search: true,
                ..Constructs::default()
            }
        );
        assert_eq!(
            filtered("listing", "title", Expr::And(vec![])).constructs(),
            Constructs::default()
        );
    }
}
