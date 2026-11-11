//! The canonical text of an expression (specification section 10).

use std::fmt;

use crate::ast::{Expr, UnaryOp};

/// Double-quoted text with `\\ \" \n \t \r` escapes.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The precedence level of a node (specification section 3); primaries are 9.
fn level(e: &Expr) -> u8 {
    match e {
        Expr::Ternary(..) => 1,
        Expr::Binary(op, ..) => op.level(),
        Expr::Unary(UnaryOp::Not, _) => 4,
        Expr::Unary(UnaryOp::Neg, _) => 8,
        _ => 9,
    }
}

fn write(e: &Expr, min_level: u8, out: &mut String) {
    let paren = level(e) < min_level;
    if paren {
        out.push('(');
    }
    match e {
        Expr::Null => out.push_str("null"),
        Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Int(n) => out.push_str(&n.to_string()),
        Expr::Decimal(d) => out.push_str(&d.to_string()),
        Expr::Text(s) => out.push_str(&quote(s)),
        Expr::List(items) => {
            out.push('[');
            comma_separated(items, out);
            out.push(']');
        }
        Expr::Path(root, path) => {
            out.push_str(root.as_str());
            for segment in path {
                out.push('.');
                out.push_str(segment);
            }
        }
        Expr::Unary(UnaryOp::Not, inner) => {
            out.push_str("not ");
            write(inner, 4, out);
        }
        Expr::Unary(UnaryOp::Neg, inner) => {
            out.push('-');
            write(inner, 8, out);
        }
        Expr::Binary(op, l, r) => {
            let own = op.level();
            // Comparison is not associative: both sides must bind tighter. The other binary
            // operators are left-associative: the right side must bind tighter.
            let (left_min, right_min) = if own == 5 { (6, 6) } else { (own, own + 1) };
            write(l, left_min, out);
            out.push(' ');
            out.push_str(op.as_str());
            out.push(' ');
            write(r, right_min, out);
        }
        Expr::Ternary(c, a, b) => {
            write(c, 2, out);
            out.push_str(" ? ");
            write(a, 1, out);
            out.push_str(" : ");
            write(b, 1, out);
        }
        Expr::Call(name, args) => {
            out.push_str(name);
            out.push('(');
            comma_separated(args, out);
            out.push(')');
        }
    }
    if paren {
        out.push(')');
    }
}

fn comma_separated(items: &[Expr], out: &mut String) {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write(item, 1, out);
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write(self, 1, &mut out);
        f.write_str(&out)
    }
}
