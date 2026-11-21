//! Boolean predicates with constants folded away, so "reads everything" and "reads nothing"
//! are recognised instead of emitted as `And([])` and `Or([])`.

use spot_storage_contract::Expr;

/// A predicate over rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pred {
    True,
    False,
    Expr(Expr),
}

impl Pred {
    pub(crate) fn and(parts: Vec<Pred>) -> Pred {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Pred::True => {}
                Pred::False => return Pred::False,
                Pred::Expr(e) => out.push(e),
            }
        }
        match out.len() {
            0 => Pred::True,
            1 => Pred::Expr(out.remove(0)),
            _ => Pred::Expr(Expr::And(out)),
        }
    }

    pub(crate) fn or(parts: Vec<Pred>) -> Pred {
        let mut out = Vec::new();
        for p in parts {
            match p {
                Pred::False => {}
                Pred::True => return Pred::True,
                Pred::Expr(e) => out.push(e),
            }
        }
        match out.len() {
            0 => Pred::False,
            1 => Pred::Expr(out.remove(0)),
            _ => Pred::Expr(Expr::Or(out)),
        }
    }
}
