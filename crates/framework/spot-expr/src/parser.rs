//! The parser: recursive descent over the grammar of specification section 3, with the node
//! and depth limits checked as each node is created (post-order).

use crate::ast::{BinaryOp, Expr, Root, UnaryOp};
use crate::error::{ParseError, codes};
use crate::functions;
use crate::lexer::{Tok, Token, lex};

/// The most nodes an expression may have.
pub const MAX_NODES: usize = 2000;
/// The deepest nesting an expression may have; the root is depth 1.
pub const MAX_DEPTH: usize = 64;

/// Parses an expression.
pub fn parse(source: &str) -> Result<Expr, ParseError> {
    let tokens = lex(source)?;
    let mut p = Parser {
        tokens,
        at: 0,
        nodes: 0,
    };
    let (expr, _) = p.ternary()?;
    match p.peek().tok {
        Tok::End => Ok(expr),
        _ => Err(p.unexpected()),
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    nodes: usize,
}

/// A parsed node and its depth.
type Node = (Expr, usize);

impl Parser {
    fn peek(&self) -> &Token {
        // The token list always ends with `End`, and `at` never passes it.
        &self.tokens[self.at.min(self.tokens.len() - 1)]
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if !matches!(t.tok, Tok::End) {
            self.at += 1;
        }
        t
    }

    fn unexpected(&self) -> ParseError {
        let t = self.peek();
        let what = match &t.tok {
            Tok::End => "the end of the expression".to_owned(),
            other => format!("`{}`", describe(other)),
        };
        ParseError::new(codes::SYNTAX, t.offset, format!("unexpected {what}"))
    }

    fn expect(&mut self, want: &Tok) -> Result<Token, ParseError> {
        if &self.peek().tok == want {
            Ok(self.bump())
        } else {
            Err(self.unexpected())
        }
    }

    /// Creates a node: counts it and checks both limits.
    fn node(
        &mut self,
        expr: Expr,
        children_depth: usize,
        offset: usize,
    ) -> Result<Node, ParseError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ParseError::new(
                codes::TOO_COMPLEX,
                offset,
                format!("an expression has at most {MAX_NODES} nodes"),
            ));
        }
        let depth = children_depth + 1;
        if depth > MAX_DEPTH {
            return Err(ParseError::new(
                codes::TOO_DEEP,
                offset,
                format!("an expression nests at most {MAX_DEPTH} deep"),
            ));
        }
        Ok((expr, depth))
    }

    fn ternary(&mut self) -> Result<Node, ParseError> {
        let offset = self.peek().offset;
        let cond = self.or()?;
        if self.peek().tok != Tok::Question {
            return Ok(cond);
        }
        self.bump();
        let yes = self.ternary()?;
        self.expect(&Tok::Colon)?;
        let no = self.ternary()?;
        let depth = cond.1.max(yes.1).max(no.1);
        self.node(
            Expr::Ternary(Box::new(cond.0), Box::new(yes.0), Box::new(no.0)),
            depth,
            offset,
        )
    }

    fn binary_chain(
        &mut self,
        next: fn(&mut Parser) -> Result<Node, ParseError>,
        ops: &[(Tok, BinaryOp)],
    ) -> Result<Node, ParseError> {
        let offset = self.peek().offset;
        let mut left = next(self)?;
        loop {
            let Some(op) = ops
                .iter()
                .find(|(t, _)| *t == self.peek().tok)
                .map(|(_, op)| *op)
            else {
                return Ok(left);
            };
            self.bump();
            let right = next(self)?;
            let depth = left.1.max(right.1);
            left = self.node(
                Expr::Binary(op, Box::new(left.0), Box::new(right.0)),
                depth,
                offset,
            )?;
        }
    }

    fn or(&mut self) -> Result<Node, ParseError> {
        self.binary_chain(Parser::and, &[(Tok::Or, BinaryOp::Or)])
    }

    fn and(&mut self) -> Result<Node, ParseError> {
        self.binary_chain(Parser::not, &[(Tok::And, BinaryOp::And)])
    }

    fn not(&mut self) -> Result<Node, ParseError> {
        if self.peek().tok == Tok::Not {
            let offset = self.bump().offset;
            let inner = self.not()?;
            return self.node(
                Expr::Unary(UnaryOp::Not, Box::new(inner.0)),
                inner.1,
                offset,
            );
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Node, ParseError> {
        let offset = self.peek().offset;
        let left = self.additive()?;
        let op = match self.peek().tok {
            Tok::EqEq => BinaryOp::Eq,
            Tok::NotEq => BinaryOp::Ne,
            Tok::Lt => BinaryOp::Lt,
            Tok::Le => BinaryOp::Le,
            Tok::Gt => BinaryOp::Gt,
            Tok::Ge => BinaryOp::Ge,
            Tok::In => BinaryOp::In,
            _ => return Ok(left),
        };
        self.bump();
        let right = self.additive()?;
        let depth = left.1.max(right.1);
        self.node(
            Expr::Binary(op, Box::new(left.0), Box::new(right.0)),
            depth,
            offset,
        )
    }

    fn additive(&mut self) -> Result<Node, ParseError> {
        self.binary_chain(
            Parser::multiplicative,
            &[(Tok::Plus, BinaryOp::Add), (Tok::Minus, BinaryOp::Sub)],
        )
    }

    fn multiplicative(&mut self) -> Result<Node, ParseError> {
        self.binary_chain(
            Parser::unary,
            &[
                (Tok::Star, BinaryOp::Mul),
                (Tok::Slash, BinaryOp::Div),
                (Tok::Percent, BinaryOp::Rem),
            ],
        )
    }

    fn unary(&mut self) -> Result<Node, ParseError> {
        if self.peek().tok == Tok::Minus {
            let offset = self.bump().offset;
            let inner = self.unary()?;
            return self.node(
                Expr::Unary(UnaryOp::Neg, Box::new(inner.0)),
                inner.1,
                offset,
            );
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, ParseError> {
        let token = self.peek().clone();
        let offset = token.offset;
        match token.tok {
            Tok::Int(n) => {
                self.bump();
                self.node(Expr::Int(n), 0, offset)
            }
            Tok::Dec(d) => {
                self.bump();
                self.node(Expr::Decimal(d), 0, offset)
            }
            Tok::Text(s) => {
                self.bump();
                self.node(Expr::Text(s), 0, offset)
            }
            Tok::True => {
                self.bump();
                self.node(Expr::Bool(true), 0, offset)
            }
            Tok::False => {
                self.bump();
                self.node(Expr::Bool(false), 0, offset)
            }
            Tok::Null => {
                self.bump();
                self.node(Expr::Null, 0, offset)
            }
            Tok::LParen => {
                self.bump();
                let inner = self.ternary()?;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Tok::LBracket => {
                self.bump();
                let (items, depth) = self.arguments(&Tok::RBracket)?;
                self.node(Expr::List(items), depth, offset)
            }
            Tok::Ident(name) => {
                self.bump();
                if self.peek().tok == Tok::LParen {
                    self.bump();
                    let (args, depth) = self.arguments(&Tok::RParen)?;
                    match functions::arity(&name) {
                        None => {
                            return Err(ParseError::new(
                                codes::UNKNOWN_FUNCTION,
                                offset,
                                format!("`{name}` is not a function of the language"),
                            ));
                        }
                        Some(arity) if !arity.accepts(args.len()) => {
                            return Err(ParseError::new(
                                codes::ARITY,
                                offset,
                                format!("`{name}` does not take {} arguments", args.len()),
                            ));
                        }
                        Some(_) => {}
                    }
                    return self.node(Expr::Call(name, args), depth, offset);
                }
                let root = match name.as_str() {
                    "doc" => Root::Doc,
                    "parent" => Root::Parent,
                    _ => {
                        return Err(ParseError::new(
                            codes::UNKNOWN_NAME,
                            offset,
                            format!("`{name}` is not a name in the language"),
                        ));
                    }
                };
                let mut path = Vec::new();
                while self.peek().tok == Tok::Dot {
                    self.bump();
                    match self.bump() {
                        Token {
                            tok: Tok::Ident(segment),
                            ..
                        } => path.push(segment),
                        other => {
                            self.at -= 1;
                            let _ = other;
                            return Err(self.unexpected());
                        }
                    }
                }
                if path.is_empty() {
                    return Err(ParseError::new(
                        codes::UNKNOWN_NAME,
                        offset,
                        format!("`{name}` must be followed by a field name"),
                    ));
                }
                self.node(Expr::Path(root, path), 0, offset)
            }
            _ => Err(self.unexpected()),
        }
    }

    /// Comma-separated expressions up to the closing token; the depth of the deepest.
    fn arguments(&mut self, close: &Tok) -> Result<(Vec<Expr>, usize), ParseError> {
        let mut items = Vec::new();
        let mut depth = 0;
        if &self.peek().tok == close {
            self.bump();
            return Ok((items, depth));
        }
        loop {
            let (e, d) = self.ternary()?;
            items.push(e);
            depth = depth.max(d);
            match &self.peek().tok {
                Tok::Comma => {
                    self.bump();
                }
                t if t == close => {
                    self.bump();
                    return Ok((items, depth));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Int(n) => n.to_string(),
        Tok::Dec(d) => d.to_string(),
        Tok::Text(_) => "text".to_owned(),
        Tok::Ident(s) => s.clone(),
        Tok::And => "and".into(),
        Tok::Or => "or".into(),
        Tok::Not => "not".into(),
        Tok::In => "in".into(),
        Tok::True => "true".into(),
        Tok::False => "false".into(),
        Tok::Null => "null".into(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::Comma => ",".into(),
        Tok::Dot => ".".into(),
        Tok::Question => "?".into(),
        Tok::Colon => ":".into(),
        Tok::Plus => "+".into(),
        Tok::Minus => "-".into(),
        Tok::Star => "*".into(),
        Tok::Slash => "/".into(),
        Tok::Percent => "%".into(),
        Tok::EqEq => "==".into(),
        Tok::NotEq => "!=".into(),
        Tok::Lt => "<".into(),
        Tok::Le => "<=".into(),
        Tok::Gt => ">".into(),
        Tok::Ge => ">=".into(),
        Tok::End => "end".into(),
    }
}
