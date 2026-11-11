//! The small expression language of the case files, and the command words of a setup line.
//!
//! An expression is an S-expression. A bare word is a field of the `from` entity (`code`) or
//! of a joined alias (`j.code`), a number is an integer, `null`, `true` and `false` are
//! themselves, and a quoted string is a tagged value (see [`crate::values`]). Operators:
//!
//! - comparison `(= a b)` `(<> a b)` `(< a b)` `(<= a b)` `(> a b)` `(>= a b)`
//! - logic `(and ...)` `(or ...)` `(not a)` `(null? a)`
//! - lists `(in a v1 v2 ...)` `(in-seq a N)` (the integers 1 to N) `(coalesce a b ...)`
//! - text `(equals a "pattern")`, also `prefix`, `contains`, `suffix`, each with a `-ci` form
//!   for case-insensitive; the pattern is raw text, not a tagged value
//! - arithmetic `(+ a b)` `(- a b)` `(* a b)`
//! - aggregates `(count)` `(count a)` `(count-distinct a)` `(sum a)` `(sum-distinct a)`
//!   `(min a)` `(max a)`
//! - spatial (ADR-087 §4) `(within-distance a "geo:13.4050 52.5200" 5000)` and
//!   `(within-box a "geo:13.0 52.0" "geo:13.5 52.6)")`: the filter's own point and whole metres
//! - range (§5) `(range-overlaps a "range:[date:2026-11-01,date:2026-11-03)")`,
//!   `(range-contains a "date:2026-11-02")`, `(range-contains-range a "range:[...)")`
//! - search (§7) `(search web "a term" note title)` in a filter, and `(search-rank plain "term"
//!   note)` as a whole `order` key

use spot_storage_contract::{
    AggFunc, ArithOp, CmpOp, Expr, FieldRef, GeoTest, Join, JoinKind, Name, Nulls, Order, OrderDir,
    Query, QueryPath, RangeTest, SearchMode, SearchTerm, Selection, Source, TextMatch, TextMode,
    Value,
};

use crate::values::{Ids, parse_geo, parse_range, parse_value};

/// The alias of the `from` entity.
pub const FROM_ALIAS: &str = "i";

#[derive(Debug, Clone)]
enum Sexp {
    Atom(String),
    Str(String),
    List(Vec<Sexp>),
}

/// Reads the text of a quoted string after its opening quote, up to the closing one.
fn read_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String, String> {
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err("unterminated string".into()),
            Some('"') => return Ok(out),
            Some('\\') => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('u') => {
                    if chars.next() != Some('{') {
                        return Err("expected \\u{...}".into());
                    }
                    let mut hex = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        hex.push(c);
                    }
                    let code =
                        u32::from_str_radix(&hex, 16).map_err(|_| format!("bad \\u{{{hex}}}"))?;
                    out.push(char::from_u32(code).ok_or_else(|| format!("bad scalar {hex}"))?);
                }
                other => return Err(format!("bad escape \\{}", other.unwrap_or(' '))),
            },
            Some(c) => out.push(c),
        }
    }
}

fn parse_sexp(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Sexp, String> {
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
    match chars.next() {
        None => Err("unexpected end of expression".into()),
        Some('(') => {
            let mut items = Vec::new();
            loop {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                match chars.peek() {
                    None => return Err("missing `)`".into()),
                    Some(')') => {
                        chars.next();
                        return Ok(Sexp::List(items));
                    }
                    Some(_) => items.push(parse_sexp(chars)?),
                }
            }
        }
        Some(')') => Err("unexpected `)`".into()),
        Some('"') => Ok(Sexp::Str(read_quoted(chars)?)),
        Some(c) => {
            let mut atom = String::from(c);
            while let Some(&n) = chars.peek() {
                if n.is_whitespace() || n == '(' || n == ')' || n == '"' {
                    break;
                }
                atom.push(n);
                chars.next();
            }
            Ok(Sexp::Atom(atom))
        }
    }
}

fn name(s: &str) -> Result<Name, String> {
    Name::new(s).map_err(|e| e.to_string())
}

fn field_ref(atom: &str) -> Result<Expr, String> {
    let (alias, field) = atom.split_once('.').unwrap_or((FROM_ALIAS, atom));
    Ok(Expr::Field(FieldRef {
        alias: name(alias)?,
        field: name(field)?,
    }))
}

fn to_value(s: &Sexp, ids: &mut Ids) -> Result<Value, String> {
    match s {
        Sexp::Str(v) => parse_value(v, ids),
        Sexp::Atom(a) if a == "null" => Ok(Value::Null),
        Sexp::Atom(a) if a == "true" => Ok(Value::Bool(true)),
        Sexp::Atom(a) if a == "false" => Ok(Value::Bool(false)),
        Sexp::Atom(a) => a
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| format!("`{a}` is not a value")),
        Sexp::List(_) => Err("a value was expected, not a list".into()),
    }
}

fn text_op(op: &str) -> Option<(TextMode, bool)> {
    let (base, ci) = op.strip_suffix("-ci").map_or((op, false), |b| (b, true));
    let mode = match base {
        "equals" => TextMode::Equals,
        "prefix" => TextMode::Prefix,
        "contains" => TextMode::Contains,
        "suffix" => TextMode::Suffix,
        _ => return None,
    };
    Some((mode, ci))
}

fn to_expr(s: &Sexp, ids: &mut Ids) -> Result<Expr, String> {
    let list = match s {
        Sexp::Str(_) => return Ok(Expr::Value(to_value(s, ids)?)),
        Sexp::Atom(a) => {
            return match to_value(s, ids) {
                Ok(v) => Ok(Expr::Value(v)),
                Err(_) => field_ref(a),
            };
        }
        Sexp::List(l) => l,
    };
    let Some(Sexp::Atom(op)) = list.first() else {
        return Err("a list must start with an operator".into());
    };
    let args = &list[1..];
    let one = |ids: &mut Ids| -> Result<Box<Expr>, String> {
        match args {
            [a] => Ok(Box::new(to_expr(a, ids)?)),
            _ => Err(format!("`{op}` takes one argument")),
        }
    };
    let all = |ids: &mut Ids| -> Result<Vec<Expr>, String> {
        args.iter().map(|a| to_expr(a, ids)).collect()
    };
    let cmp = |o: CmpOp, ids: &mut Ids| -> Result<Expr, String> {
        match args {
            [a, b] => Ok(Expr::Cmp(
                Box::new(to_expr(a, ids)?),
                o,
                Box::new(to_expr(b, ids)?),
            )),
            _ => Err(format!("`{op}` takes two arguments")),
        }
    };
    let arith = |o: ArithOp, ids: &mut Ids| -> Result<Expr, String> {
        match args {
            [a, b] => Ok(Expr::Arith(
                Box::new(to_expr(a, ids)?),
                o,
                Box::new(to_expr(b, ids)?),
            )),
            _ => Err(format!("`{op}` takes two arguments")),
        }
    };
    let agg = |f: AggFunc, distinct: bool, ids: &mut Ids| -> Result<Expr, String> {
        let arg = match args {
            [] => None,
            [a] => Some(Box::new(to_expr(a, ids)?)),
            _ => return Err(format!("`{op}` takes at most one argument")),
        };
        Ok(Expr::Agg {
            func: f,
            arg,
            distinct,
        })
    };
    match op.as_str() {
        "=" => cmp(CmpOp::Eq, ids),
        "<>" => cmp(CmpOp::Ne, ids),
        "<" => cmp(CmpOp::Lt, ids),
        "<=" => cmp(CmpOp::Le, ids),
        ">" => cmp(CmpOp::Gt, ids),
        ">=" => cmp(CmpOp::Ge, ids),
        "and" => Ok(Expr::And(all(ids)?)),
        "or" => Ok(Expr::Or(all(ids)?)),
        "coalesce" => Ok(Expr::Coalesce(all(ids)?)),
        "not" => Ok(Expr::Not(one(ids)?)),
        "null?" => Ok(Expr::IsNull(one(ids)?)),
        "+" => arith(ArithOp::Add, ids),
        "-" => arith(ArithOp::Sub, ids),
        "*" => arith(ArithOp::Mul, ids),
        "count" => agg(AggFunc::Count, false, ids),
        "count-distinct" => agg(AggFunc::Count, true, ids),
        "sum" => agg(AggFunc::Sum, false, ids),
        "sum-distinct" => agg(AggFunc::Sum, true, ids),
        "min" => agg(AggFunc::Min, false, ids),
        "max" => agg(AggFunc::Max, false, ids),
        "in-seq" => match args {
            [x, Sexp::Atom(n)] => {
                let n: i64 = n.parse().map_err(|_| "in-seq needs a count".to_owned())?;
                Ok(Expr::In(
                    Box::new(to_expr(x, ids)?),
                    (1..=n).map(Value::Int).collect(),
                ))
            }
            _ => Err("`in-seq` takes an operand and a count".into()),
        },
        "in" => {
            let Some((head, values)) = args.split_first() else {
                return Err("`in` needs an operand".into());
            };
            let values = values
                .iter()
                .map(|v| to_value(v, ids))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Expr::In(Box::new(to_expr(head, ids)?), values))
        }
        "within-distance" => match args {
            [field, Sexp::Str(centre), Sexp::Atom(metres)] => {
                let p = parse_geo(centre)?;
                let metres: u32 = metres
                    .parse()
                    .map_err(|_| format!("`{metres}` is not a whole number of metres"))?;
                Ok(Expr::Geo(
                    Box::new(to_expr(field, ids)?),
                    GeoTest::WithinDistance {
                        lon: p.lon,
                        lat: p.lat,
                        metres,
                    },
                ))
            }
            _ => Err("`within-distance` takes a field, a quoted geo point and metres".into()),
        },
        "within-box" => match args {
            [field, Sexp::Str(south_west), Sexp::Str(north_east)] => {
                let a = parse_geo(south_west)?;
                let b = parse_geo(north_east)?;
                Ok(Expr::Geo(
                    Box::new(to_expr(field, ids)?),
                    GeoTest::WithinBox {
                        min_lon: a.lon,
                        min_lat: a.lat,
                        max_lon: b.lon,
                        max_lat: b.lat,
                    },
                ))
            }
            _ => Err("`within-box` takes a field and two quoted geo points".into()),
        },
        "range-overlaps" | "range-contains-range" => match args {
            [field, Sexp::Str(range)] => {
                let r = parse_range(range, ids)?;
                let (start, end) = r
                    .bounds()
                    .ok_or_else(|| format!("`{range}` is not a bounded range"))?;
                let (start, end) = (start.to_value(), end.to_value());
                let test = if op == "range-overlaps" {
                    RangeTest::Overlaps { start, end }
                } else {
                    RangeTest::ContainsRange { start, end }
                };
                Ok(Expr::Range(Box::new(to_expr(field, ids)?), test))
            }
            _ => Err(format!("`{op}` takes a field and a quoted range")),
        },
        "range-contains" => match args {
            [field, value] => Ok(Expr::Range(
                Box::new(to_expr(field, ids)?),
                RangeTest::Contains(to_value(value, ids)?),
            )),
            _ => Err("`range-contains` takes a field and a value".into()),
        },
        "search" | "search-rank" => {
            let Some((Sexp::Atom(mode), rest)) = args.split_first() else {
                return Err(format!("`{op}` starts with a mode: web, phrase or plain"));
            };
            let mode = match mode.as_str() {
                "web" => SearchMode::Web,
                "phrase" => SearchMode::Phrase,
                "plain" => SearchMode::Plain,
                other => return Err(format!("unknown search mode `{other}`")),
            };
            let Some((Sexp::Str(text), fields)) = rest.split_first() else {
                return Err(format!("`{op}` takes a quoted term after the mode"));
            };
            let fields = fields
                .iter()
                .map(|f| match f {
                    Sexp::Atom(a) => name(a),
                    Sexp::Str(a) => name(a),
                    Sexp::List(_) => Err("a search field is a name, not a list".to_owned()),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let term = SearchTerm {
                mode,
                text: text.clone(),
                fields,
            };
            Ok(if op == "search" {
                Expr::Search(term)
            } else {
                Expr::SearchRank(term)
            })
        }
        other => match text_op(other) {
            Some((mode, case_insensitive)) => match args {
                [a, Sexp::Str(p)] => Ok(Expr::Text(
                    Box::new(to_expr(a, ids)?),
                    TextMatch {
                        mode,
                        pattern: p.clone(),
                        case_insensitive,
                    },
                )),
                _ => Err(format!("`{other}` takes an operand and a quoted pattern")),
            },
            None => Err(format!("unknown operator `{other}`")),
        },
    }
}

/// Parses one expression.
pub fn parse_expr(text: &str, ids: &mut Ids) -> Result<Expr, String> {
    let mut chars = text.chars().peekable();
    let sexp = parse_sexp(&mut chars)?;
    if chars.any(|c| !c.is_whitespace()) {
        return Err(format!("trailing text after the expression in `{text}`"));
    }
    to_expr(&sexp, ids)
}

/// Parses `<expr> [asc|desc] [nulls-first|nulls-last]` (default: ascending, nulls last).
pub fn parse_order(text: &str, ids: &mut Ids) -> Result<Order, String> {
    let mut chars = text.chars().peekable();
    let sexp = parse_sexp(&mut chars)?;
    let rest: String = chars.collect();
    let mut dir = OrderDir::Asc;
    let mut nulls = Nulls::Last;
    for word in rest.split_whitespace() {
        match word {
            "asc" => dir = OrderDir::Asc,
            "desc" => dir = OrderDir::Desc,
            "nulls-first" => nulls = Nulls::First,
            "nulls-last" => nulls = Nulls::Last,
            other => return Err(format!("unknown order word `{other}` in `{text}`")),
        }
    }
    Ok(Order {
        expr: to_expr(&sexp, ids)?,
        dir,
        nulls,
    })
}

/// A join, as written in a case.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSpec {
    /// `inner` or `left`.
    pub kind: String,
    /// The joined entity.
    pub entity: String,
    /// Its alias.
    pub alias: String,
    /// The join condition.
    pub on: String,
}

/// A query, as written in a case.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySpec {
    /// The `from` entity; default `item`.
    pub from: Option<String>,
    /// Joined entities.
    #[serde(default)]
    pub joins: Vec<JoinSpec>,
    /// Output expressions, in order.
    #[serde(default)]
    pub select: Vec<String>,
    /// Row filter.
    pub filter: Option<String>,
    /// Grouping keys.
    #[serde(default)]
    pub group: Vec<String>,
    /// Group filter.
    pub having: Option<String>,
    /// Sort keys.
    #[serde(default)]
    pub order: Vec<String>,
    /// Row limit.
    pub limit: Option<u32>,
    /// Rows skipped.
    #[serde(default)]
    pub offset: u64,
}

/// Builds the IR query for a spec.
pub fn build_query(spec: &QuerySpec, ids: &mut Ids) -> Result<Query, String> {
    let from = name(spec.from.as_deref().unwrap_or("item"))?;
    let mut q = Query::from(&from, &name(FROM_ALIAS)?, QueryPath::Report);
    for j in &spec.joins {
        q.joins.push(Join {
            kind: match j.kind.as_str() {
                "inner" => JoinKind::Inner,
                "left" => JoinKind::Left,
                other => return Err(format!("unknown join kind `{other}`")),
            },
            source: Source {
                entity: name(&j.entity)?,
                alias: name(&j.alias)?,
            },
            on: parse_expr(&j.on, ids)?,
        });
    }
    for (i, s) in spec.select.iter().enumerate() {
        q.select.push(Selection {
            expr: parse_expr(s, ids)?,
            alias: name(&format!("c{i}"))?,
        });
    }
    if let Some(f) = &spec.filter {
        q.filter = Some(parse_expr(f, ids)?);
    }
    for g in &spec.group {
        q.group_by.push(parse_expr(g, ids)?);
    }
    if let Some(h) = &spec.having {
        q.having = Some(parse_expr(h, ids)?);
    }
    for o in &spec.order {
        q.order_by.push(parse_order(o, ids)?);
    }
    q.limit = spec.limit;
    q.offset = spec.offset;
    Ok(q)
}

/// Splits a command line into words. A word may contain quoted parts, so
/// `code="t:a b"` is one word, `code=t:a b` of the unquoted form is two.
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Ok(out);
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            if c == '"' {
                word.push_str(&read_quoted(&mut chars)?);
            } else {
                word.push(c);
            }
        }
        out.push(word);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions_parse() -> Result<(), String> {
        let mut ids = Ids::new();
        let e = parse_expr(
            r#"(and (= code "t:a b") (contains-ci note "x") (in qty 1 2))"#,
            &mut ids,
        )?;
        assert!(matches!(e, Expr::And(v) if v.len() == 3));
        assert!(parse_expr("(bogus a)", &mut ids).is_err());
        assert!(parse_expr("(= a b) c", &mut ids).is_err());
        let o = parse_order("(+ qty 1) desc nulls-first", &mut ids)?;
        assert_eq!((o.dir, o.nulls), (OrderDir::Desc, Nulls::First));
        assert_eq!(
            words(r#"a code="t:x y" qty="i:1""#)?,
            ["a", "code=t:x y", "qty=i:1"]
        );
        Ok(())
    }
}
