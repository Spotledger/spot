//! The RFC 7644 filter subset an identity provider actually sends (ADR-070 7.2).
//!
//! Entra and Okta pre-check a `userName` before they create a user, filter groups by
//! `displayName`, and list the members of a group. That is `eq`, `ne`, `co`, `sw`, `ew`,
//! `gt`, `lt`, `ge`, `le`, `pr`, `and`, `or`, `not` and parentheses, over `userName`,
//! `externalId`, `displayName`, `active`, `id`, `emails.value`, `name.*`, the enterprise
//! extension and `members.*`. Everything else is refused with `400 invalidFilter`, never
//! silently ignored, so a client that asks a question this server cannot answer is told so.
//!
//! String comparison for `eq`, `ne`, `co` and `sw` is **case-insensitive**, which is what the
//! providers expect of `userName` and `displayName`; the wire keeps the client's own casing.

use crate::error::{ScimError, ScimResult};
use crate::resource::{Email, Manager, Member, Name, ScimGroup, ScimUser};

/// One value a filter compares against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScimValue {
    /// A string.
    Text(String),
    /// A boolean.
    Bool(bool),
    /// The `null` literal.
    Null,
}

impl ScimValue {
    fn as_text(&self) -> Option<&str> {
        match self {
            ScimValue::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// A comparison operator of RFC 7644.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Contains.
    Co,
    /// Starts with.
    Sw,
    /// Ends with.
    Ew,
    /// Greater than.
    Gt,
    /// Less than.
    Lt,
    /// Greater than or equal.
    Ge,
    /// Less than or equal.
    Le,
}

impl CompareOp {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "co" => Some(Self::Co),
            "sw" => Some(Self::Sw),
            "ew" => Some(Self::Ew),
            "gt" => Some(Self::Gt),
            "lt" => Some(Self::Lt),
            "ge" => Some(Self::Ge),
            "le" => Some(Self::Le),
            _ => None,
        }
    }
}

/// One attribute path, with an optional sub-attribute and an optional value filter
/// (`emails[type eq "work"].value`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrPath {
    /// The attribute as written, lower-cased; may itself be dotted (`name.givenName`).
    pub attr: String,
    /// A sub-attribute written after a value filter (`.value` in `emails[...].value`).
    pub sub: Option<String>,
    /// The value filter inside `[...]`.
    pub value_filter: Option<Box<Filter>>,
}

/// A parsed filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// `attr op value`.
    Compare {
        /// The attribute path.
        path: AttrPath,
        /// The operator.
        op: CompareOp,
        /// The value.
        value: ScimValue,
    },
    /// `attr pr`.
    Present {
        /// The attribute path.
        path: AttrPath,
    },
    /// All of the terms.
    And(Vec<Filter>),
    /// Any of the terms.
    Or(Vec<Filter>),
    /// The negation of a term.
    Not(Box<Filter>),
}

/// What a filter can be evaluated against.
pub enum Target<'a> {
    /// A user resource.
    User(&'a ScimUser),
    /// A group resource.
    Group(&'a ScimGroup),
}

/// A row of a multi-valued attribute, for a value filter.
enum Row<'a> {
    Email(&'a Email),
    Member(&'a Member),
    Manager(&'a Manager),
}

/// Something a filter can resolve an attribute path against.
trait Resolver {
    fn resolve(&self, path: &AttrPath) -> ScimResult<Vec<ScimValue>>;
}

impl Filter {
    /// Parses a filter, or refuses it with `400 invalidFilter`.
    pub fn parse(input: &str) -> ScimResult<Filter> {
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Err(ScimError::invalid_filter("the filter is empty"));
        }
        let mut parser = Parser { tokens, pos: 0 };
        let filter = parser.parse_or()?;
        if parser.pos != parser.tokens.len() {
            return Err(ScimError::invalid_filter(
                "the filter has trailing text after a complete expression",
            ));
        }
        Ok(filter)
    }

    /// Parses a bare attribute path, as a PATCH `path` carries (`emails[type eq "work"].value`).
    /// A path that does not parse is `400 invalidPath`.
    pub fn parse_path(input: &str) -> ScimResult<AttrPath> {
        let tokens = tokenize(input).map_err(|e| ScimError::invalid_path(e.detail))?;
        if tokens.is_empty() {
            return Err(ScimError::invalid_path("the path is empty"));
        }
        let mut parser = Parser { tokens, pos: 0 };
        let path = parser
            .parse_attr_path()
            .map_err(|e| ScimError::invalid_path(e.detail))?;
        if parser.pos != parser.tokens.len() {
            return Err(ScimError::invalid_path(
                "the path has trailing text after a complete attribute",
            ));
        }
        Ok(path)
    }

    /// Evaluates the filter against a resource. An attribute this resource does not have is
    /// `400 invalidFilter`, never a silent `false`.
    pub fn matches(&self, target: &Target<'_>) -> ScimResult<bool> {
        match target {
            Target::User(u) => self.eval(*u),
            Target::Group(g) => self.eval(*g),
        }
    }

    fn eval<R: Resolver>(&self, resolver: &R) -> ScimResult<bool> {
        match self {
            Filter::Compare { path, op, value } => {
                let values = resolver.resolve(path)?;
                Ok(compare(&values, *op, value))
            }
            Filter::Present { path } => Ok(!resolver.resolve(path)?.is_empty()),
            Filter::And(terms) => {
                for t in terms {
                    if !t.eval(resolver)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Filter::Or(terms) => {
                for t in terms {
                    if t.eval(resolver)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Filter::Not(term) => Ok(!term.eval(resolver)?),
        }
    }
}

fn eq_value(a: &ScimValue, b: &ScimValue) -> bool {
    match (a, b) {
        (ScimValue::Text(x), ScimValue::Text(y)) => x.eq_ignore_ascii_case(y),
        (ScimValue::Bool(x), ScimValue::Bool(y)) => x == y,
        (ScimValue::Null, ScimValue::Null) => true,
        _ => false,
    }
}

fn order(a: &ScimValue, b: &ScimValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (ScimValue::Text(x), ScimValue::Text(y)) => Some(x.cmp(y)),
        (ScimValue::Bool(x), ScimValue::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn compare(values: &[ScimValue], op: CompareOp, wanted: &ScimValue) -> bool {
    match op {
        CompareOp::Eq => values.iter().any(|v| eq_value(v, wanted)),
        CompareOp::Ne => !values.iter().any(|v| eq_value(v, wanted)),
        CompareOp::Co | CompareOp::Sw | CompareOp::Ew => {
            let Some(w) = wanted.as_text() else {
                return false;
            };
            let w = w.to_lowercase();
            values.iter().any(|v| {
                v.as_text().is_some_and(|text| {
                    let text = text.to_lowercase();
                    match op {
                        CompareOp::Co => text.contains(&w),
                        CompareOp::Sw => text.starts_with(&w),
                        CompareOp::Ew => text.ends_with(&w),
                        _ => false,
                    }
                })
            })
        }
        CompareOp::Gt | CompareOp::Lt | CompareOp::Ge | CompareOp::Le => values.iter().any(|v| {
            order(v, wanted).is_some_and(|o| match op {
                CompareOp::Gt => o.is_gt(),
                CompareOp::Lt => o.is_lt(),
                CompareOp::Ge => o.is_ge(),
                CompareOp::Le => o.is_le(),
                _ => false,
            })
        }),
    }
}

/// Resolves the enterprise extension's attributes whether the client wrote the full URN
/// (`urn:...:enterprise:2.0:User:department`) or the short name (`department`).
pub(crate) fn enterprise_attr(attr: &str) -> &str {
    attr.strip_prefix("urn:ietf:params:scim:schemas:extension:enterprise:2.0:user:")
        .unwrap_or(attr)
}

/// Splits `name.givenName` into `("name", Some("givenname"))`.
pub(crate) fn split_attr(attr: &str) -> (&str, Option<&str>) {
    match attr.rsplit_once('.') {
        Some((base, sub)) => (base, Some(sub)),
        None => (attr, None),
    }
}

/// The base attribute and sub-attribute of a path, after the enterprise prefix is stripped.
pub(crate) fn path_parts(path: &AttrPath) -> (&str, Option<&str>) {
    let raw = enterprise_attr(&path.attr);
    let (base, derived) = split_attr(raw);
    (base, path.sub.as_deref().or(derived))
}

fn text(value: &str) -> ScimValue {
    ScimValue::Text(value.to_owned())
}

fn opt(value: &Option<String>) -> Vec<ScimValue> {
    value.as_deref().map(text).into_iter().collect()
}

fn emails_of(emails: &[Email], sub: Option<&str>) -> Vec<ScimValue> {
    emails
        .iter()
        .filter_map(|e| match sub {
            None | Some("value") => Some(text(&e.value)),
            Some("type") => e.kind.as_deref().map(text),
            Some("primary") => e.primary.map(ScimValue::Bool),
            _ => None,
        })
        .collect()
}

fn members_of(members: &[Member], sub: Option<&str>) -> Vec<ScimValue> {
    members
        .iter()
        .filter_map(|m| match sub {
            None | Some("value") => Some(text(&m.value)),
            Some("display") => m.display.as_deref().map(text),
            Some("type") => m.kind.as_deref().map(text),
            _ => None,
        })
        .collect()
}

fn name_attr(name: &Name, sub: Option<&str>) -> Vec<ScimValue> {
    match sub {
        Some("formatted") => opt(&name.formatted),
        Some("givenname") => opt(&name.given_name),
        Some("familyname") => opt(&name.family_name),
        Some("middlename") => opt(&name.middle_name),
        Some("honorificprefix") => opt(&name.honorific_prefix),
        Some("honorificsuffix") => opt(&name.honorific_suffix),
        _ => Vec::new(),
    }
}

fn row_sub_one(row: &Row<'_>, sub: Option<&str>) -> Vec<ScimValue> {
    match row {
        Row::Email(e) => emails_of(std::slice::from_ref(e), sub),
        Row::Member(m) => members_of(std::slice::from_ref(m), sub),
        Row::Manager(m) => match sub {
            None | Some("value") => vec![text(&m.value)],
            Some("displayname") => opt(&m.display_name),
            _ => Vec::new(),
        },
    }
}

fn row_sub(rows: &[&Row<'_>], sub: Option<&str>) -> Vec<ScimValue> {
    rows.iter().flat_map(|row| row_sub_one(row, sub)).collect()
}

/// Evaluates a value filter over a multi-valued attribute and returns the matching rows.
fn filtered_rows<'a>(base: &'a [Row<'a>], inner: &Filter) -> ScimResult<Vec<&'a Row<'a>>> {
    let mut out = Vec::new();
    for row in base {
        if inner.eval(row)? {
            out.push(row);
        }
    }
    Ok(out)
}

impl Resolver for ScimUser {
    fn resolve(&self, path: &AttrPath) -> ScimResult<Vec<ScimValue>> {
        let (base, sub) = path_parts(path);
        if let Some(inner) = &path.value_filter {
            let rows: Vec<Row<'_>> = match base {
                "emails" => self.emails.iter().map(Row::Email).collect(),
                _ => {
                    return Err(ScimError::invalid_filter(format!(
                        "a value filter is not supported on `{}`",
                        path.attr
                    )));
                }
            };
            let matching = filtered_rows(&rows, inner)?;
            return Ok(row_sub(&matching, sub));
        }
        let values = match base {
            "id" => self.id.as_deref().map(text).into_iter().collect(),
            "username" => vec![text(&self.user_name)],
            "externalid" => opt(&self.external_id),
            "displayname" => opt(&self.display_name),
            "active" => vec![ScimValue::Bool(self.active)],
            "emails" => emails_of(&self.emails, sub),
            "name" => self
                .name
                .as_ref()
                .map(|n| name_attr(n, sub))
                .unwrap_or_default(),
            "department" => self
                .enterprise
                .as_ref()
                .map(|e| opt(&e.department))
                .unwrap_or_default(),
            "employeenumber" => self
                .enterprise
                .as_ref()
                .map(|e| opt(&e.employee_number))
                .unwrap_or_default(),
            "manager" => self
                .enterprise
                .as_ref()
                .and_then(|e| e.manager.as_ref())
                .map(|m| row_sub_one(&Row::Manager(m), sub))
                .unwrap_or_default(),
            _ => {
                return Err(ScimError::invalid_filter(format!(
                    "the filter names an attribute a user does not have: `{}`",
                    path.attr
                )));
            }
        };
        Ok(values)
    }
}

impl Resolver for ScimGroup {
    fn resolve(&self, path: &AttrPath) -> ScimResult<Vec<ScimValue>> {
        let (base, sub) = path_parts(path);
        if let Some(inner) = &path.value_filter {
            let rows: Vec<Row<'_>> = match base {
                "members" => self.members.iter().map(Row::Member).collect(),
                _ => {
                    return Err(ScimError::invalid_filter(format!(
                        "a value filter is not supported on `{}`",
                        path.attr
                    )));
                }
            };
            let matching = filtered_rows(&rows, inner)?;
            return Ok(row_sub(&matching, sub));
        }
        let values = match base {
            "id" => self.id.as_deref().map(text).into_iter().collect(),
            "externalid" => opt(&self.external_id),
            "displayname" => vec![text(&self.display_name)],
            "members" => members_of(&self.members, sub),
            _ => {
                return Err(ScimError::invalid_filter(format!(
                    "the filter names an attribute a group does not have: `{}`",
                    path.attr
                )));
            }
        };
        Ok(values)
    }
}

impl Resolver for Row<'_> {
    fn resolve(&self, path: &AttrPath) -> ScimResult<Vec<ScimValue>> {
        if path.value_filter.is_some() {
            return Err(ScimError::invalid_filter(
                "a value filter cannot be nested inside another value filter",
            ));
        }
        let (base, sub) = path_parts(path);
        let known = match self {
            Row::Email(_) => matches!(base, "value" | "type" | "primary"),
            Row::Member(_) => matches!(base, "value" | "display" | "type"),
            Row::Manager(_) => matches!(base, "value" | "displayname"),
        };
        if !known {
            return Err(ScimError::invalid_filter(format!(
                "the value filter names an attribute a row does not have: `{}`",
                path.attr
            )));
        }
        // A value filter writes `value eq "x"`: the attribute name is the value itself.
        let effective_sub = sub.or(Some(base));
        Ok(row_sub_one(self, effective_sub))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    And,
    Or,
    Not,
    Op(CompareOp),
    Pr,
    Ident(String),
    Str(String),
    Bool(bool),
    Null,
}

fn tokenize(input: &str) -> ScimResult<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            '"' => {
                let (value, next) = read_string(&chars, i)?;
                tokens.push(Token::Str(value));
                i = next;
            }
            c if is_word_char(c) => {
                let start = i;
                while i < chars.len() && is_word_char(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                tokens.push(classify(&word));
            }
            _ => {
                return Err(ScimError::invalid_filter(format!(
                    "the filter has a character that is not allowed: `{c}`"
                )));
            }
        }
    }
    Ok(tokens)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '$')
}

fn classify(word: &str) -> Token {
    let lower = word.to_ascii_lowercase();
    match lower.as_str() {
        "and" => Token::And,
        "or" => Token::Or,
        "not" => Token::Not,
        "pr" => Token::Pr,
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        "null" => Token::Null,
        _ => match CompareOp::parse(&lower) {
            Some(op) => Token::Op(op),
            None => Token::Ident(lower),
        },
    }
}

fn read_string(chars: &[char], start: usize) -> ScimResult<(String, usize)> {
    let mut out = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(*other),
                    None => {
                        return Err(ScimError::invalid_filter(
                            "the filter ends inside a string literal",
                        ));
                    }
                }
                i += 1;
            }
            '"' => return Ok((out, i + 1)),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Err(ScimError::invalid_filter(
        "the filter ends inside a string literal",
    ))
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn parse_or(&mut self) -> ScimResult<Filter> {
        let mut terms = vec![self.parse_and()?];
        while matches!(self.peek(), Some(Token::Or)) {
            self.pos += 1;
            terms.push(self.parse_and()?);
        }
        if terms.len() == 1 {
            Ok(terms.remove(0))
        } else {
            Ok(Filter::Or(terms))
        }
    }

    fn parse_and(&mut self) -> ScimResult<Filter> {
        let mut terms = vec![self.parse_not()?];
        while matches!(self.peek(), Some(Token::And)) {
            self.pos += 1;
            terms.push(self.parse_not()?);
        }
        if terms.len() == 1 {
            Ok(terms.remove(0))
        } else {
            Ok(Filter::And(terms))
        }
    }

    fn parse_not(&mut self) -> ScimResult<Filter> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.pos += 1;
            return Ok(Filter::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> ScimResult<Filter> {
        if matches!(self.peek(), Some(Token::LParen)) {
            self.pos += 1;
            let inner = self.parse_or()?;
            if !matches!(self.next(), Some(Token::RParen)) {
                return Err(ScimError::invalid_filter("a `(` is never closed"));
            }
            return Ok(inner);
        }
        let path = self.parse_attr_path()?;
        if matches!(self.peek(), Some(Token::Pr)) {
            self.pos += 1;
            return Ok(Filter::Present { path });
        }
        // A bare value path (`members[value eq "x"]`) is a filter on its own: it holds when
        // the multi-valued attribute has a matching row.
        if path.value_filter.is_some() && !matches!(self.peek(), Some(Token::Op(_))) {
            return Ok(Filter::Present { path });
        }
        let op = match self.next() {
            Some(Token::Op(op)) => op,
            _ => {
                return Err(ScimError::invalid_filter(
                    "an attribute is followed by a comparison operator or `pr`",
                ));
            }
        };
        let value = self.parse_value()?;
        Ok(Filter::Compare { path, op, value })
    }

    fn parse_attr_path(&mut self) -> ScimResult<AttrPath> {
        let attr = match self.next() {
            Some(Token::Ident(name)) => name,
            _ => {
                return Err(ScimError::invalid_filter(
                    "a comparison starts with an attribute name",
                ));
            }
        };
        let mut sub: Option<String> = None;
        let mut value_filter: Option<Box<Filter>> = None;
        loop {
            match self.peek() {
                Some(Token::LBracket) => {
                    self.pos += 1;
                    let inner = self.parse_or()?;
                    if !matches!(self.next(), Some(Token::RBracket)) {
                        return Err(ScimError::invalid_filter("a `[` is never closed"));
                    }
                    value_filter = Some(Box::new(inner));
                }
                // A trailing `.value` after a value filter tokenises as `.value`, because `.`
                // is part of a word.
                Some(Token::Ident(word)) if word.starts_with('.') => {
                    sub = Some(word.trim_start_matches('.').to_owned());
                    self.pos += 1;
                }
                _ => break,
            }
        }
        Ok(AttrPath {
            attr,
            sub,
            value_filter,
        })
    }

    fn parse_value(&mut self) -> ScimResult<ScimValue> {
        match self.next() {
            Some(Token::Str(value)) => Ok(ScimValue::Text(value)),
            Some(Token::Bool(b)) => Ok(ScimValue::Bool(b)),
            Some(Token::Null) => Ok(ScimValue::Null),
            Some(Token::Ident(word)) => Ok(ScimValue::Text(word)),
            _ => Err(ScimError::invalid_filter(
                "a comparison operator is followed by a value",
            )),
        }
    }
}
