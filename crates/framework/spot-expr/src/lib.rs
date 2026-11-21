//! The shared expression language (WP-UI-11.T1; ADR-28).
//!
//! A small, pure, total language for field rules, evaluated on the client and on the server
//! with identical results. The specification is `docs/expression-language.md`; the executable
//! form is the conformance corpus in `corpus/`, which the .NET implementation must also pass.
//!
//! ```
//! use spot_expr::{MapContext, Value, parse};
//! let ctx = MapContext::new(spot_expr::DateTime::from_micros(0).unwrap_or_else(|| unreachable!()))
//!     .with("doc.qty", Value::Int(3));
//! let e = parse("doc.qty > 2 ? 'many' : 'few'").unwrap_or_else(|e| panic!("{e}"));
//! assert_eq!(e.eval(&ctx), Ok(Value::Text("many".into())));
//! ```
//!
//! No floats, no clocks, no I/O: the only outside reads are the [`Context`]'s fields and `now`.

mod ast;
mod context;
mod decimal;
mod error;
mod eval;
mod functions;
mod lexer;
pub mod mapping;
mod parser;
mod print;
mod value;

pub use ast::{BinaryOp, Expr, Root, UnaryOp};
pub use context::{Context, MapContext};
pub use error::{EvalError, ParseError, codes};
pub use parser::{MAX_DEPTH, MAX_NODES, parse};
pub use value::{Date, DateTime, Value};
