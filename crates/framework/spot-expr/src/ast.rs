//! The syntax tree.

use rust_decimal::Decimal;

/// The root of a field path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Root {
    /// The document being edited.
    Doc,
    /// Its parent document (for a child row).
    Parent,
}

impl Root {
    /// The root's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Root::Doc => "doc",
            Root::Parent => "parent",
        }
    }
}

/// A unary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// `not`
    Not,
    /// `-`
    Neg,
}

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// `or`
    Or,
    /// `and`
    And,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `in`
    In,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
}

impl BinaryOp {
    /// The operator's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            BinaryOp::Or => "or",
            BinaryOp::And => "and",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::In => "in",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
        }
    }

    /// The precedence level of the operator (specification section 3).
    pub(crate) fn level(self) -> u8 {
        match self {
            BinaryOp::Or => 2,
            BinaryOp::And => 3,
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::In => 5,
            BinaryOp::Add | BinaryOp::Sub => 6,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => 7,
        }
    }
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// `null`
    Null,
    /// `true` or `false`
    Bool(bool),
    /// An integer literal (never negative: `-5` is `Neg(Int(5))`).
    Int(i64),
    /// A decimal literal, with its written scale.
    Decimal(Decimal),
    /// A text literal.
    Text(String),
    /// A list.
    List(Vec<Expr>),
    /// A field path: the root, then the names.
    Path(Root, Vec<String>),
    /// A unary operation.
    Unary(UnaryOp, Box<Expr>),
    /// A binary operation.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `c ? a : b`
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// A call of a library function.
    Call(String, Vec<Expr>),
}
