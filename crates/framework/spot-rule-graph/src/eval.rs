//! The deterministic semi-naive evaluator.
//!
//! Evaluation runs the strata the compiler assigned, lowest first. Within a stratum, aggregate
//! rules run once (their inputs are complete in a lower stratum) and the rest reach their least
//! fixed point by semi-naive iteration: each round joins one positive body atom against the
//! tuples discovered in the previous round and the others against the full relation. Output is
//! sorted and deduplicated by [`Datum`]'s total order, so no hash-map iteration order leaks.
//!
//! Evaluation is a pure function of the compiled program, the fact source and the query's
//! instant: it reads no clock (time is the query's argument) and no randomness. A fuel limit
//! ([`DEFAULT_FUEL`]) turns a program that would derive too much into
//! [`RuleGraphError::FuelExhausted`] instead of running forever.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use spot_expr::{Context, DateTime, Root, Value as ExprValue};

use crate::ast::AggFunc;
use crate::compile::{Compiled, PredKind, RAtom, RGuard, RHead, RHeadArg, RRule, RTerm};
use crate::datum::Datum;
use crate::error::{RuleGraphError, UNKNOWN_QUERY};
use crate::source::FactSource;

/// One row: the values of a predicate's arguments, in argument order.
pub type Tuple = Vec<Datum>;

/// The default fuel limit: how many tuples evaluation may derive before it gives up.
pub const DEFAULT_FUEL: u64 = 1_000_000;

type Bindings = BTreeMap<String, Datum>;
type Relations = BTreeMap<String, BTreeSet<Tuple>>;
type Emit<'e> = &'e mut dyn FnMut(&Bindings) -> Result<(), RuleGraphError>;

/// What to evaluate: a predicate, and the instant guards' `now()` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The predicate whose tuples the evaluation returns.
    pub predicate: String,
    /// The instant a guard's `now()` reads. Time is an argument, never a clock read.
    pub now: DateTime,
}

impl Query {
    /// A query for `predicate`, at the epoch (`1970-01-01T00:00:00Z`) unless
    /// [`Query::at`] sets another instant.
    #[must_use]
    pub fn new(predicate: impl Into<String>) -> Self {
        Self {
            predicate: predicate.into(),
            now: epoch(),
        }
    }

    /// The same query at `now`.
    #[must_use]
    pub fn at(mut self, now: DateTime) -> Self {
        self.now = now;
        self
    }
}

/// The epoch instant.
fn epoch() -> DateTime {
    DateTime::from_micros(0).unwrap_or_else(|| unreachable!("1970-01-01T00:00:00Z is always valid"))
}

/// The result of an evaluation: one row per tuple, sorted and deduplicated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rows {
    /// How many columns each row has (the queried predicate's arity).
    pub columns: usize,
    /// The rows, in [`Datum`] order.
    pub rows: Vec<Tuple>,
}

impl Rows {
    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows.
    #[must_use]
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }
}

/// The rule evaluator.
#[derive(Clone, Debug)]
pub struct Engine {
    fuel: u64,
}

impl Engine {
    /// An engine with the default fuel.
    #[must_use]
    pub fn new() -> Self {
        Self { fuel: DEFAULT_FUEL }
    }

    /// An engine with `fuel` as its limit on derived tuples.
    #[must_use]
    pub fn with_fuel(fuel: u64) -> Self {
        Self { fuel }
    }

    /// The fuel limit.
    #[must_use]
    pub fn fuel(&self) -> u64 {
        self.fuel
    }

    /// Evaluates `compiled` over `source` and returns the tuples of `query.predicate`.
    ///
    /// # Errors
    ///
    /// [`RuleGraphError::Eval`] when the query names an unknown predicate or a guard fails,
    /// [`RuleGraphError::FuelExhausted`] when evaluation derives more than the fuel allows.
    pub fn evaluate(
        &self,
        compiled: &Compiled,
        source: &dyn FactSource,
        query: &Query,
    ) -> Result<Rows, RuleGraphError> {
        let predicate =
            compiled
                .predicate(&query.predicate)
                .ok_or_else(|| RuleGraphError::Eval {
                    code: UNKNOWN_QUERY,
                    message: format!("the program does not define `{}`", query.predicate),
                })?;
        let columns = predicate.arity;
        let mut relations = load_base(compiled, source);
        let fuel = Cell::new(0_u64);
        for stratum in 0..compiled.strata_count() {
            self.eval_stratum(compiled, stratum, &mut relations, query.now, &fuel)?;
        }
        let rows = relations
            .remove(&query.predicate)
            .map(|set| set.into_iter().collect())
            .unwrap_or_default();
        Ok(Rows { columns, rows })
    }

    fn eval_stratum(
        &self,
        compiled: &Compiled,
        stratum: usize,
        relations: &mut Relations,
        now: DateTime,
        fuel: &Cell<u64>,
    ) -> Result<(), RuleGraphError> {
        let rule_indices = &compiled.strata[stratum];

        // Aggregate rules read complete lower strata, so they run before the fixpoint.
        for &index in rule_indices {
            if compiled.rules[index].aggregate {
                self.eval_aggregate(&compiled.rules[index], relations, now, fuel)?;
            }
        }

        // The first round joins each positive literal against the tuples it has now (base facts
        // for an edge or field predicate, derived tuples from a lower stratum), so a rule with
        // only base literals still fires. Later rounds use only what the previous round added.
        let mut delta: Relations = Relations::new();
        for &index in rule_indices {
            let rule = &compiled.rules[index];
            if rule.aggregate {
                continue;
            }
            for atom in &rule.positives {
                let entry = delta.entry(atom.pred.clone()).or_default();
                if let Some(existing) = relations.get(&atom.pred) {
                    entry.extend(existing.iter().cloned());
                }
            }
        }

        loop {
            let mut added: Relations = Relations::new();
            for &index in rule_indices {
                let rule = &compiled.rules[index];
                if rule.aggregate {
                    continue;
                }
                if rule.positives.is_empty() {
                    // A rule with no positive atom (a guard-only body) fires once per round;
                    // its head is all constants, so the second round derives nothing new.
                    self.join(rule, None, relations, &delta, now, fuel, &mut |bindings| {
                        let tuple = eval_head(&rule.head, bindings)?;
                        added
                            .entry(rule.head.pred.clone())
                            .or_default()
                            .insert(tuple);
                        Ok(())
                    })?;
                    continue;
                }
                for literal in 0..rule.positives.len() {
                    self.join(
                        rule,
                        Some(literal),
                        relations,
                        &delta,
                        now,
                        fuel,
                        &mut |bindings| {
                            let tuple = eval_head(&rule.head, bindings)?;
                            added
                                .entry(rule.head.pred.clone())
                                .or_default()
                                .insert(tuple);
                            Ok(())
                        },
                    )?;
                }
            }
            let mut fresh: Relations = Relations::new();
            for (predicate, tuples) in added {
                let relation = relations.entry(predicate.clone()).or_default();
                let new: BTreeSet<Tuple> = tuples
                    .into_iter()
                    .filter(|tuple| !relation.contains(tuple))
                    .collect();
                if !new.is_empty() {
                    relation.extend(new.iter().cloned());
                    fresh.insert(predicate, new);
                }
            }
            if fresh.is_empty() {
                break;
            }
            delta = fresh;
        }
        Ok(())
    }

    fn eval_aggregate(
        &self,
        rule: &RRule,
        relations: &mut Relations,
        now: DateTime,
        fuel: &Cell<u64>,
    ) -> Result<(), RuleGraphError> {
        let agg_count = rule
            .head
            .args
            .iter()
            .filter(|arg| matches!(arg, RHeadArg::Agg(..)))
            .count();
        let mut groups: BTreeMap<Tuple, Vec<Vec<Datum>>> = BTreeMap::new();
        {
            let empty = Relations::new();
            let joiner = Joiner {
                rule,
                relations,
                delta: &empty,
                delta_lit: None,
                now,
                fuel,
                limit: self.fuel,
            };
            let bindings = Bindings::new();
            joiner.run(0, &bindings, &mut |bindings| {
                let mut key = Tuple::new();
                let mut columns: Vec<Vec<Datum>> = vec![Vec::new(); agg_count];
                let mut column = 0_usize;
                for arg in &rule.head.args {
                    match arg {
                        RHeadArg::Agg(_, var) => {
                            let value = bindings.get(var).cloned().ok_or_else(|| {
                                RuleGraphError::aggregate(format!(
                                    "aggregate variable `{var}` is unbound"
                                ))
                            })?;
                            columns[column].push(value);
                            column += 1;
                        }
                        RHeadArg::Var(var) => {
                            key.push(bindings.get(var).cloned().ok_or_else(|| {
                                RuleGraphError::eval(format!("head variable `{var}` is unbound"))
                            })?)
                        }
                        RHeadArg::Const(value) => key.push(value.clone()),
                    }
                }
                let entry = groups
                    .entry(key)
                    .or_insert_with(|| vec![Vec::new(); agg_count]);
                for (index, values) in columns.into_iter().enumerate() {
                    entry[index].extend(values);
                }
                Ok(())
            })?;
        }

        for (key, columns) in groups {
            let mut tuple = Tuple::new();
            let mut key_iter = key.into_iter();
            let mut column_iter = columns.into_iter();
            for arg in &rule.head.args {
                match arg {
                    RHeadArg::Agg(func, _) => {
                        let values = column_iter.next().unwrap_or_default();
                        tuple.push(aggregate(*func, &values)?);
                    }
                    RHeadArg::Var(_) | RHeadArg::Const(_) => {
                        if let Some(value) = key_iter.next() {
                            tuple.push(value);
                        }
                    }
                }
            }
            relations
                .entry(rule.head.pred.clone())
                .or_default()
                .insert(tuple);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn join(
        &self,
        rule: &RRule,
        delta_lit: Option<usize>,
        relations: &Relations,
        delta: &Relations,
        now: DateTime,
        fuel: &Cell<u64>,
        emit: Emit<'_>,
    ) -> Result<(), RuleGraphError> {
        let joiner = Joiner {
            rule,
            relations,
            delta,
            delta_lit,
            now,
            fuel,
            limit: self.fuel,
        };
        let bindings = Bindings::new();
        joiner.run(0, &bindings, emit)
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// The base facts of every edge and field predicate, loaded once.
fn load_base(compiled: &Compiled, source: &dyn FactSource) -> Relations {
    let mut relations = Relations::new();
    for predicate in compiled.predicates() {
        match &predicate.kind {
            PredKind::Edge => {
                let mut set = BTreeSet::new();
                for (from, to) in source.edges(&predicate.name) {
                    set.insert(vec![Datum::Record(from), Datum::Record(to)]);
                }
                relations.insert(predicate.name.clone(), set);
            }
            PredKind::Field { doctype, field } => {
                let mut set = BTreeSet::new();
                for (record, value) in source.fields(doctype, field) {
                    if let Some(datum) = Datum::from_storage(&value) {
                        set.insert(vec![Datum::Record(record), datum]);
                    }
                }
                relations.insert(predicate.name.clone(), set);
            }
            PredKind::Derived => {}
        }
    }
    relations
}

struct Joiner<'a> {
    rule: &'a RRule,
    relations: &'a Relations,
    delta: &'a Relations,
    delta_lit: Option<usize>,
    now: DateTime,
    fuel: &'a Cell<u64>,
    limit: u64,
}

impl Joiner<'_> {
    fn run(
        &self,
        position: usize,
        bindings: &Bindings,
        emit: Emit<'_>,
    ) -> Result<(), RuleGraphError> {
        if position == self.rule.positives.len() {
            return self.finish(bindings, emit);
        }
        let atom = &self.rule.positives[position];
        let source = if self.delta_lit == Some(position) {
            self.delta
        } else {
            self.relations
        };
        if let Some(tuples) = source.get(&atom.pred) {
            for tuple in tuples {
                let mut next = bindings.clone();
                if unify(&mut next, &atom.terms, tuple) {
                    self.run(position + 1, &next, emit)?;
                }
            }
        }
        Ok(())
    }

    fn finish(&self, bindings: &Bindings, emit: Emit<'_>) -> Result<(), RuleGraphError> {
        for guard in &self.rule.guards {
            if !self.guard_holds(guard, bindings)? {
                return Ok(());
            }
        }
        for atom in &self.rule.negatives {
            if self.negative_matches(atom, bindings) {
                return Ok(());
            }
        }
        let derived = self.fuel.get().saturating_add(1);
        self.fuel.set(derived);
        if derived > self.limit {
            return Err(RuleGraphError::FuelExhausted { limit: self.limit });
        }
        emit(bindings)
    }

    fn guard_holds(&self, guard: &RGuard, bindings: &Bindings) -> Result<bool, RuleGraphError> {
        let context = GuardContext {
            vars: bindings,
            now: self.now,
        };
        match guard.expr.eval(&context) {
            Ok(ExprValue::Bool(value)) => Ok(value),
            Ok(ExprValue::Null) => Ok(false),
            Ok(other) => Err(RuleGraphError::eval(format!(
                "the guard `{}` returned {} where a boolean is required",
                guard.source,
                other.type_name()
            ))),
            Err(error) => Err(RuleGraphError::eval(format!(
                "the guard `{}` failed: {error}",
                guard.source
            ))),
        }
    }

    fn negative_matches(&self, atom: &RAtom, bindings: &Bindings) -> bool {
        self.relations.get(&atom.pred).is_some_and(|tuples| {
            tuples
                .iter()
                .any(|tuple| unify_check(bindings, &atom.terms, tuple))
        })
    }
}

/// A guard's evaluation context: the rule's bound variables as `doc.<name>`, and the query's
/// instant.
struct GuardContext<'a> {
    vars: &'a Bindings,
    now: DateTime,
}

impl Context for GuardContext<'_> {
    fn field(&self, root: Root, path: &[String]) -> Option<ExprValue> {
        if root != Root::Doc || path.len() != 1 {
            return None;
        }
        self.vars.get(&path[0]).map(Datum::to_expr)
    }

    fn now(&self) -> DateTime {
        self.now
    }
}

fn unify(bindings: &mut Bindings, terms: &[RTerm], tuple: &[Datum]) -> bool {
    if terms.len() != tuple.len() {
        return false;
    }
    for (term, datum) in terms.iter().zip(tuple) {
        match term {
            RTerm::Anon => {}
            RTerm::Const(value) => {
                if value != datum {
                    return false;
                }
            }
            RTerm::Var(var) => match bindings.get(var) {
                Some(bound) => {
                    if bound != datum {
                        return false;
                    }
                }
                None => {
                    bindings.insert(var.clone(), datum.clone());
                }
            },
        }
    }
    true
}

fn unify_check(bindings: &Bindings, terms: &[RTerm], tuple: &[Datum]) -> bool {
    terms.len() == tuple.len()
        && terms.iter().zip(tuple).all(|(term, datum)| match term {
            RTerm::Anon => true,
            RTerm::Const(value) => value == datum,
            RTerm::Var(var) => bindings.get(var).is_some_and(|bound| bound == datum),
        })
}

fn eval_head(head: &RHead, bindings: &Bindings) -> Result<Tuple, RuleGraphError> {
    head.args
        .iter()
        .map(|arg| match arg {
            RHeadArg::Var(var) => bindings
                .get(var)
                .cloned()
                .ok_or_else(|| RuleGraphError::eval(format!("head variable `{var}` is unbound"))),
            RHeadArg::Const(value) => Ok(value.clone()),
            RHeadArg::Agg(func, _) => Err(RuleGraphError::aggregate(format!(
                "aggregate `{}` is not allowed outside an aggregate rule",
                func.as_str()
            ))),
        })
        .collect()
}

fn aggregate(func: AggFunc, values: &[Datum]) -> Result<Datum, RuleGraphError> {
    match func {
        AggFunc::Count => {
            let distinct: BTreeSet<&Datum> = values.iter().collect();
            let count = i64::try_from(distinct.len())
                .map_err(|_| RuleGraphError::aggregate("count overflows an integer"))?;
            Ok(Datum::Int(count))
        }
        AggFunc::Sum => {
            let mut integers: i64 = 0;
            let mut decimal: Option<Decimal> = None;
            for value in values {
                match value {
                    Datum::Null => {}
                    Datum::Int(n) => match decimal.as_mut() {
                        Some(acc) => *acc += Decimal::from(*n),
                        None => {
                            integers = integers.checked_add(*n).ok_or_else(|| {
                                RuleGraphError::aggregate("sum overflows an integer")
                            })?;
                        }
                    },
                    Datum::Decimal(d) => {
                        let acc = decimal.get_or_insert(Decimal::from(integers));
                        *acc += *d;
                    }
                    other => {
                        return Err(RuleGraphError::aggregate(format!(
                            "sum cannot add a {} value",
                            other.type_name()
                        )));
                    }
                }
            }
            Ok(match decimal {
                Some(d) => Datum::Decimal(d),
                None => Datum::Int(integers),
            })
        }
        AggFunc::Min => Ok(values
            .iter()
            .filter(|value| !matches!(value, Datum::Null))
            .min()
            .cloned()
            .unwrap_or(Datum::Null)),
        AggFunc::Max => Ok(values
            .iter()
            .filter(|value| !matches!(value, Datum::Null))
            .max()
            .cloned()
            .unwrap_or(Datum::Null)),
    }
}

/// The error codes this module can raise, re-exported for callers that match on them.
pub mod codes {
    pub use crate::error::{AGGREGATE, EVAL, FUEL_EXHAUSTED, UNKNOWN_QUERY};
}
