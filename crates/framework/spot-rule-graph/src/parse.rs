//! The rule-text lexer and recursive-descent parser.
//!
//! The grammar is small and deliberately so:
//!
//! ```text
//! program := rule*
//! rule    := head ":-" body "."
//! head    := name "(" (headarg ("," headarg)*)? ")"
//! headarg := term | agg
//! agg     := ("count" | "sum" | "min" | "max") "<" name ">"
//! body    := literal ("," literal)*
//! literal := [ "not" ] name "(" (term ("," term)*)? ")"
//!          | "where" "(" expression ")"
//! term    := "_" | name | const
//! ```
//!
//! A name may be dotted (`account.parent`); a variable may not. A guard is captured verbatim
//! between balanced parentheses and handed to [`spot_expr::parse`], so the expression grammar
//! is never duplicated here. Every parse fault reports the line and column of the offending
//! token, and the parser recovers at the next rule terminator, so a file with several faults
//! reports them all.

use rust_decimal::Decimal;
use spot_expr::{Expr, Root};

use crate::ast::{AggFunc, Const, Head, HeadArg, Literal, Program, Rule, Span, Term};
use crate::error::{PARSE, Problem, RuleGraphError};

/// Parses a program, reporting every fault with its line and column.
///
/// # Errors
///
/// [`RuleGraphError::Invalid`] with one [`Problem`] per fault (code [`PARSE`]).
pub fn parse(text: &str) -> Result<Program, RuleGraphError> {
    let mut parser = Parser::new(text);
    let program = parser.program();
    if parser.problems.is_empty() {
        Ok(program)
    } else {
        Err(RuleGraphError::Invalid(parser.problems))
    }
}

/// The variable names a guard reads (`doc.<name>`) and the paths it reads that a rule cannot
/// bind (a `parent.` root, or a deeper path).
#[must_use]
pub fn guard_paths(expr: &Expr) -> (Vec<String>, Vec<String>) {
    let mut vars = Vec::new();
    let mut bad = Vec::new();
    collect_paths(expr, &mut vars, &mut bad);
    (vars, bad)
}

fn collect_paths(expr: &Expr, vars: &mut Vec<String>, bad: &mut Vec<String>) {
    match expr {
        Expr::Path(Root::Doc, segments) => {
            if segments.len() == 1 {
                if !vars.contains(&segments[0]) {
                    vars.push(segments[0].clone());
                }
            } else {
                bad.push(format!("doc.{}", segments.join(".")));
            }
        }
        Expr::Path(Root::Parent, segments) => {
            bad.push(format!("parent.{}", segments.join(".")));
        }
        Expr::Unary(_, inner) => collect_paths(inner, vars, bad),
        Expr::Binary(_, left, right) => {
            collect_paths(left, vars, bad);
            collect_paths(right, vars, bad);
        }
        Expr::Ternary(a, b, c) => {
            collect_paths(a, vars, bad);
            collect_paths(b, vars, bad);
            collect_paths(c, vars, bad);
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_paths(arg, vars, bad);
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_paths(item, vars, bad);
            }
        }
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Decimal(_) | Expr::Text(_) => {}
    }
}

/// One lexical token.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// A name, possibly dotted.
    Name(String),
    /// An integer literal.
    Int(i64),
    /// A decimal literal.
    Decimal(Decimal),
    /// A text literal.
    Str(String),
    /// `(`.
    LParen,
    /// `)`.
    RParen,
    /// `,`.
    Comma,
    /// `:-`.
    ColonDash,
    /// `.`.
    Dot,
    /// `!`.
    Bang,
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `-`.
    Minus,
    /// A captured `where ( ... )` guard, holding the expression text verbatim.
    Guard(String),
    /// The end of input.
    Eof,
}

/// A token with its source position.
#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    line: u32,
    column: u32,
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: u32,
    column: u32,
    tokens: Vec<Token>,
    problems: Vec<Problem>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            line: 1,
            column: 1,
            tokens: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        self.src[self.pos..].chars().nth(1)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn push(&mut self, tok: Tok, line: u32, column: u32) {
        self.tokens.push(Token { tok, line, column });
    }

    fn lex(mut self) -> (Vec<Token>, Vec<Problem>) {
        loop {
            self.skip_trivia();
            let (line, column) = (self.line, self.column);
            let Some(ch) = self.peek() else { break };
            match ch {
                '(' => {
                    self.bump();
                    self.push(Tok::LParen, line, column);
                }
                ')' => {
                    self.bump();
                    self.push(Tok::RParen, line, column);
                }
                ',' => {
                    self.bump();
                    self.push(Tok::Comma, line, column);
                }
                '!' => {
                    self.bump();
                    self.push(Tok::Bang, line, column);
                }
                '<' => {
                    self.bump();
                    self.push(Tok::Lt, line, column);
                }
                '>' => {
                    self.bump();
                    self.push(Tok::Gt, line, column);
                }
                '-' => {
                    self.bump();
                    self.push(Tok::Minus, line, column);
                }
                '.' => {
                    self.bump();
                    self.push(Tok::Dot, line, column);
                }
                ':' => {
                    self.bump();
                    if self.peek() == Some('-') {
                        self.bump();
                        self.push(Tok::ColonDash, line, column);
                    } else {
                        self.problems.push(Problem::new(
                            PARSE,
                            line,
                            column,
                            "expected `:-` to open a rule body",
                        ));
                    }
                }
                '\'' | '"' => self.string(ch, line, column),
                '0'..='9' => self.number(line, column),
                c if is_ident_start(c) => self.name(line, column),
                other => {
                    self.bump();
                    self.problems.push(Problem::new(
                        PARSE,
                        line,
                        column,
                        format!("unexpected character `{other}`"),
                    ));
                }
            }
        }
        let (line, column) = (self.line, self.column);
        self.tokens.push(Token {
            tok: Tok::Eof,
            line,
            column,
        });
        (self.tokens, self.problems)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        self.bump();
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn name(&mut self, line: u32, column: u32) {
        let mut text = String::new();
        self.read_ident(&mut text);
        // A dotted name: `.` immediately followed by an identifier character. A `.` followed by
        // anything else is the rule terminator, never part of the name.
        while self.peek() == Some('.') && self.peek_second().is_some_and(is_ident_start) {
            self.bump();
            text.push('.');
            self.read_ident(&mut text);
        }
        // `where (` opens a guard: capture its balanced text verbatim so the expression grammar
        // (which the rule lexer does not know) is never tokenized here.
        if text == "where" {
            let (saved_pos, saved_line, saved_column) = (self.pos, self.line, self.column);
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() == Some('(') {
                let raw = self.capture_guard(line, column);
                self.push(Tok::Guard(raw), line, column);
                return;
            }
            self.pos = saved_pos;
            self.line = saved_line;
            self.column = saved_column;
        }
        self.push(Tok::Name(text), line, column);
    }

    /// Consumes a balanced `( ... )` and returns the text between the parentheses, skipping
    /// parentheses inside text literals.
    fn capture_guard(&mut self, line: u32, column: u32) -> String {
        self.bump();
        let start = self.pos;
        let mut depth = 1_usize;
        let mut end = self.pos;
        let mut quote: Option<char> = None;
        loop {
            let Some(ch) = self.peek() else {
                self.problems
                    .push(Problem::new(PARSE, line, column, "unclosed `where (`"));
                break;
            };
            if let Some(open) = quote {
                self.bump();
                if ch == '\\' {
                    self.bump();
                } else if ch == open {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    self.bump();
                }
                '(' => {
                    depth += 1;
                    self.bump();
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = self.pos;
                        self.bump();
                        break;
                    }
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
        self.src[start..end].to_owned()
    }

    fn read_ident(&mut self, out: &mut String) {
        while let Some(c) = self.peek() {
            if is_ident_char(c) {
                out.push(c);
                self.bump();
            } else {
                break;
            }
        }
    }

    fn number(&mut self, line: u32, column: u32) {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                text.push(c);
                self.bump();
            } else {
                break;
            }
        }
        let mut decimal = false;
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            decimal = true;
            text.push('.');
            self.bump();
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    text.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if decimal {
            match text.parse::<Decimal>() {
                Ok(value) => self.push(Tok::Decimal(value), line, column),
                Err(_) => self.problems.push(Problem::new(
                    PARSE,
                    line,
                    column,
                    format!("`{text}` is not a decimal in range"),
                )),
            }
        } else {
            match text.parse::<i64>() {
                Ok(value) => self.push(Tok::Int(value), line, column),
                Err(_) => self.problems.push(Problem::new(
                    PARSE,
                    line,
                    column,
                    format!("`{text}` is not an integer in range"),
                )),
            }
        }
    }

    fn string(&mut self, quote: char, line: u32, column: u32) {
        self.bump();
        let mut text = String::new();
        loop {
            match self.peek() {
                None => {
                    self.problems.push(Problem::new(
                        PARSE,
                        line,
                        column,
                        "unterminated text literal",
                    ));
                    break;
                }
                Some(c) if c == quote => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        Some('n') => text.push('\n'),
                        Some('t') => text.push('\t'),
                        Some('\\') => text.push('\\'),
                        Some('\'') => text.push('\''),
                        Some('"') => text.push('"'),
                        Some(other) => {
                            self.problems.push(Problem::new(
                                PARSE,
                                line,
                                column,
                                format!("unknown escape `\\{other}`"),
                            ));
                        }
                        None => {
                            self.problems.push(Problem::new(
                                PARSE,
                                line,
                                column,
                                "unterminated text literal",
                            ));
                            break;
                        }
                    }
                }
                Some(c) => {
                    text.push(c);
                    self.bump();
                }
            }
        }
        self.push(Tok::Str(text), line, column);
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    problems: Vec<Problem>,
}

impl Parser {
    fn new(src: &str) -> Self {
        let (tokens, problems) = Lexer::new(src).lex();
        Self {
            tokens,
            at: 0,
            problems,
        }
    }

    fn peek(&self) -> &Tok {
        &self.tokens[self.at.min(self.tokens.len() - 1)].tok
    }

    fn peek_ahead(&self, n: usize) -> &Tok {
        &self.tokens[(self.at + n).min(self.tokens.len() - 1)].tok
    }

    fn position(&self) -> Span {
        let token = &self.tokens[self.at.min(self.tokens.len() - 1)];
        Span::new(token.line, token.column)
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.at.min(self.tokens.len() - 1)].clone();
        if !matches!(token.tok, Tok::Eof) {
            self.at += 1;
        }
        token
    }

    fn program(&mut self) -> Program {
        let mut rules = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            match self.rule() {
                Ok(rule) => rules.push(rule),
                Err(problem) => {
                    self.problems.push(problem);
                    self.recover();
                }
            }
        }
        Program { rules }
    }

    /// Skips to just past the next `.` (or the end), so one broken rule does not hide the next.
    fn recover(&mut self) {
        while !matches!(self.peek(), Tok::Dot | Tok::Eof) {
            self.bump();
        }
        if matches!(self.peek(), Tok::Dot) {
            self.bump();
        }
    }

    fn rule(&mut self) -> Result<Rule, Problem> {
        let span = self.position();
        let head = self.head()?;
        if !matches!(self.peek(), Tok::ColonDash) {
            return Err(self.error("expected `:-` after the rule head"));
        }
        self.bump();
        let body = self.body()?;
        if !matches!(self.peek(), Tok::Dot) {
            return Err(self.error("expected `.` to end the rule"));
        }
        self.bump();
        Ok(Rule { head, body, span })
    }

    fn head(&mut self) -> Result<Head, Problem> {
        let span = self.position();
        let predicate = self.predicate_name()?;
        if !matches!(self.peek(), Tok::LParen) {
            return Err(self.error("expected `(` after the head predicate"));
        }
        self.bump();
        let mut args = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                args.push(self.head_arg()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if !matches!(self.peek(), Tok::RParen) {
            return Err(self.error("expected `)` to close the head"));
        }
        self.bump();
        Ok(Head {
            predicate,
            args,
            span,
        })
    }

    fn head_arg(&mut self) -> Result<HeadArg, Problem> {
        if let Tok::Name(name) = self.peek().clone()
            && let Some(func) = AggFunc::parse(&name)
            && matches!(self.peek_ahead(1), Tok::Lt)
        {
            let span = self.position();
            self.bump();
            self.bump();
            let var = match self.bump().tok {
                Tok::Name(var) => var,
                _ => return Err(self.error("expected a variable inside `<...>`")),
            };
            if !matches!(self.peek(), Tok::Gt) {
                return Err(self.error("expected `>` to close the aggregate"));
            }
            self.bump();
            return Ok(HeadArg::Agg { func, var, span });
        }
        match self.term()? {
            Term::Var(var) => Ok(HeadArg::Var(var)),
            Term::Const(value) => Ok(HeadArg::Const(value)),
            Term::Anon => Err(self.error("`_` is not allowed in a rule head")),
        }
    }

    fn body(&mut self) -> Result<Vec<Literal>, Problem> {
        let mut literals = Vec::new();
        loop {
            literals.push(self.literal()?);
            if matches!(self.peek(), Tok::Comma) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(literals)
    }

    fn literal(&mut self) -> Result<Literal, Problem> {
        if let Tok::Guard(raw) = self.peek().clone() {
            return self.guard(raw);
        }
        if let Tok::Name(name) = self.peek().clone()
            && name == "not"
            && matches!(self.peek_ahead(1), Tok::Name(_))
        {
            let span = self.position();
            self.bump();
            let (predicate, args) = self.atom()?;
            return Ok(Literal::Neg {
                predicate,
                args,
                span,
            });
        }
        let span = self.position();
        let (predicate, args) = self.atom()?;
        Ok(Literal::Atom {
            predicate,
            args,
            span,
        })
    }

    fn atom(&mut self) -> Result<(String, Vec<Term>), Problem> {
        let predicate = self.predicate_name()?;
        if !matches!(self.peek(), Tok::LParen) {
            return Err(self.error("expected `(` after a predicate"));
        }
        self.bump();
        let mut args = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                args.push(self.term()?);
                if matches!(self.peek(), Tok::Comma) {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        if !matches!(self.peek(), Tok::RParen) {
            return Err(self.error("expected `)` to close the atom"));
        }
        self.bump();
        Ok((predicate, args))
    }

    fn guard(&mut self, source: String) -> Result<Literal, Problem> {
        let span = self.position();
        self.bump();
        match spot_expr::parse(&source) {
            Ok(expr) => {
                let (vars, bad) = guard_paths(&expr);
                for path in bad {
                    self.problems.push(Problem::new(
                        PARSE,
                        span.line,
                        span.column,
                        format!("a guard cannot read `{path}`"),
                    ));
                }
                Ok(Literal::Guard {
                    source,
                    expr,
                    vars,
                    span,
                })
            }
            Err(error) => Err(Problem::new(
                PARSE,
                span.line,
                span.column,
                format!("the guard is not a valid expression: {error}"),
            )),
        }
    }

    fn term(&mut self) -> Result<Term, Problem> {
        if matches!(self.peek(), Tok::Minus) {
            let span = self.position();
            self.bump();
            let token = self.bump();
            let value = match token.tok {
                Tok::Int(n) => Const::Int(-n),
                Tok::Decimal(d) => Const::Decimal(-d),
                _ => {
                    return Err(Problem::new(
                        PARSE,
                        span.line,
                        span.column,
                        "`-` must be followed by a number",
                    ));
                }
            };
            return Ok(Term::Const(value));
        }
        let token = self.bump();
        match token.tok {
            Tok::Name(name) => {
                if name == "_" {
                    Ok(Term::Anon)
                } else if name == "true" {
                    Ok(Term::Const(Const::Bool(true)))
                } else if name == "false" {
                    Ok(Term::Const(Const::Bool(false)))
                } else if name == "null" {
                    Ok(Term::Const(Const::Null))
                } else if name.contains('.') {
                    Err(Problem::new(
                        PARSE,
                        token.line,
                        token.column,
                        format!("`{name}` is not a variable"),
                    ))
                } else {
                    Ok(Term::Var(name))
                }
            }
            Tok::Int(n) => Ok(Term::Const(Const::Int(n))),
            Tok::Decimal(d) => Ok(Term::Const(Const::Decimal(d))),
            Tok::Str(s) => Ok(Term::Const(Const::Text(s))),
            _ => Err(Problem::new(
                PARSE,
                token.line,
                token.column,
                "expected a variable, `_` or a constant",
            )),
        }
    }

    fn predicate_name(&mut self) -> Result<String, Problem> {
        let token = self.bump();
        match token.tok {
            Tok::Name(name) => Ok(name),
            _ => Err(Problem::new(
                PARSE,
                token.line,
                token.column,
                "expected a predicate name",
            )),
        }
    }

    fn error(&self, message: &str) -> Problem {
        let span = self.position();
        Problem::new(PARSE, span.line, span.column, message)
    }
}
