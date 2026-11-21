//! Generates the HTTP route registry: one `pub mod` per `src/api/ports/*.rs` and
//! `src/api/router/routes/*.rs` file (not `mod.rs`), a `REGISTERED` list of the declared port keys
//! and a `REGISTRY` table of the route modules that declare `SERVES`, `routes` and
//! `required_ports`.
//!
//! A route file may also declare `pub const PUBLIC: Public` (a group served outside the bearer
//! layer). This script checks the contract for it: a public group may serve only operations that
//! carry `security: []` in `contracts/native-api/openapi.yaml`, and an authenticated group may
//! not serve one (a plain-text scan of the contract, no YAML dependency). A public file must also
//! mount exactly the contract (method, path) pairs of its `SERVES` (`check_public_routes`).
//!
//! The scanner is plain functions over names and source text, so `tests/routes_registry.rs`
//! includes this file and tests them without a build. Std only: no dependency.

use std::path::{Path, PathBuf};

/// What the scanner found in one module file.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Module {
    /// The module (file stem), a valid Rust identifier.
    pub name: String,
    /// The file, relative to the manifest directory, with forward slashes.
    pub path: String,
    /// The file declares what its kind registers (a port key, or the three route items).
    pub registers: bool,
    /// A route file that registers and declares `PUBLIC`.
    pub public: bool,
    /// The operation ids of `SERVES` (a registering route file).
    pub serves: Vec<String>,
}

/// Which directory is scanned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Kind {
    /// `src/api/ports`: the port traits.
    Ports,
    /// `src/api/router/routes`: the route groups.
    Routes,
}

#[allow(dead_code)]
impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Ports => "src/api/ports",
            Kind::Routes => "src/api/router/routes",
        }
    }
}

/// Whether `name` can be a module name: `[a-z_][a-z0-9_]*`, and not a keyword that cannot
/// follow `mod`.
#[allow(dead_code)]
pub fn valid_module_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !matches!(
            name,
            "self" | "super" | "crate" | "mod" | "fn" | "pub" | "use" | "type" | "as"
        )
}

/// The marker line of a public group, at column 0.
const PUBLIC_MARKER: &str = "pub const PUBLIC: Public";

/// Whether a route file's source declares `PUBLIC`.
#[allow(dead_code)]
pub fn is_public(source: &str) -> bool {
    source.lines().any(|l| l.starts_with(PUBLIC_MARKER))
}

/// The operation ids of a route file's `pub const SERVES: &[&str] = &["a", "b"];` (the list may
/// span lines). Anything that is not a literal list of plain strings is an error: the contract
/// check must read what the group serves, so a computed `SERVES` stops the build.
#[allow(dead_code)]
pub fn serves_of(file_name: &str, source: &str) -> Result<Vec<String>, String> {
    // Column 0 only, like `inspect`: a comment or a string that mentions the declaration must not
    // be read as it.
    let mut offset = 0;
    let mut start = None;
    for line in source.split_inclusive('\n') {
        if line.starts_with("pub const SERVES: &[&str]") {
            start = Some(offset);
            break;
        }
        offset += line.len();
    }
    let Some(start) = start else {
        return Ok(Vec::new());
    };
    let rest = &source[start..];
    let Some(end) = rest.find(';') else {
        return Err(format!("{file_name}: SERVES has no terminating `;`"));
    };
    let decl = &rest[..end];
    let Some(eq) = decl.find('=') else {
        return Err(format!("{file_name}: SERVES has no value"));
    };
    let value = decl[eq + 1..].trim();
    let Some(list) = value.strip_prefix("&[").and_then(|v| v.strip_suffix(']')) else {
        return Err(format!(
            "{file_name}: SERVES must be a literal list `&[\"id\", ..]` so the build can check it \
             against the contract"
        ));
    };
    let mut ids = Vec::new();
    for item in list.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        match item.strip_prefix('"').and_then(|i| i.strip_suffix('"')) {
            Some(id) if !id.is_empty() && !id.contains(['"', '\\']) => ids.push(id.to_owned()),
            _ => {
                return Err(format!(
                    "{file_name}: SERVES must be a literal list of plain string ids; found `{item}`"
                ));
            }
        }
    }
    Ok(ids)
}

/// The `/api` operations of the contract text as (operationId, has `security: []`). A plain scan
/// of the `paths:` section: an operation is a `get|put|post|patch|delete:` key at four spaces,
/// its `operationId:` and `security:` keys sit at six. Only the exact line `security: []` makes
/// an operation public; a missing or any other `security` entry leaves it authenticated.
#[allow(dead_code)]
pub fn contract_operations(yaml: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut in_paths = false;
    let mut current: Option<(Option<String>, bool)> = None;
    fn close(current: &mut Option<(Option<String>, bool)>, out: &mut Vec<(String, bool)>) {
        if let Some((Some(id), public)) = current.take() {
            out.push((id, public));
        }
    }
    for raw in yaml.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            close(&mut current, &mut out);
            in_paths = line == "paths:";
            continue;
        }
        if !in_paths {
            continue;
        }
        if indent <= 4 {
            close(&mut current, &mut out);
            let key = line.trim();
            if indent == 4
                && matches!(
                    key,
                    "get:" | "put:" | "post:" | "patch:" | "delete:" | "head:" | "options:"
                )
            {
                current = Some((None, false));
            }
            continue;
        }
        if indent == 6
            && let Some((id, public)) = current.as_mut()
        {
            if let Some(value) = line.trim().strip_prefix("operationId:") {
                *id = Some(value.trim().to_owned());
            } else if line.trim() == "security: []" {
                *public = true;
            }
        }
    }
    close(&mut current, &mut out);
    out
}

/// One route group as the contract check sees it.
#[allow(dead_code)]
pub struct GroupFacts<'a> {
    /// The module name.
    pub name: &'a str,
    /// Whether it declares `PUBLIC`.
    pub public: bool,
    /// Its `SERVES`.
    pub serves: &'a [String],
}

/// The security rule between the groups and the contract (`contract_file` is only named in the
/// message): a public group serves only `security: []` operations and every one of them must be
/// found (an operation the check cannot see is refused, fail closed); an authenticated group
/// serves none.
#[allow(dead_code)]
pub fn check_security(
    contract_file: &str,
    yaml: &str,
    groups: &[GroupFacts<'_>],
) -> Result<(), String> {
    let operations = contract_operations(yaml);
    for group in groups {
        for id in group.serves {
            let found = operations.iter().find(|(op, _)| op == id);
            match (group.public, found) {
                (true, Some((_, true))) | (false, Some((_, false))) | (false, None) => {}
                (true, Some((_, false))) => {
                    return Err(format!(
                        "{contract_file}: operation `{id}` is served by the public group `{}` \
                         but is not `security: []` in the contract",
                        group.name
                    ));
                }
                (true, None) => {
                    return Err(format!(
                        "{contract_file}: operation `{id}` served by the public group `{}` is \
                         not an operation of the contract; a public group may serve only \
                         `security: []` operations",
                        group.name
                    ));
                }
                (false, Some((_, true))) => {
                    return Err(format!(
                        "{contract_file}: operation `{id}` is `security: []` in the contract \
                         but the group `{}` serving it does not declare `PUBLIC` (it would \
                         answer 401 to every real request)",
                        group.name
                    ));
                }
            }
        }
    }
    Ok(())
}

// --- the mounted routes of a public file ---------------------------------------------------------

/// One lexical token of a route file: comments are gone, string contents are kept apart from
/// code, so nothing inside a comment or a string is ever read as code (and nothing in code is
/// hidden by one).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(char),
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Reads a `"..."` string whose opening quote is at `chars[i]`; returns its raw content and the
/// index after the closing quote.
fn lex_string(file: &str, chars: &[char], i: usize) -> Result<(String, usize), String> {
    let mut out = String::new();
    let mut j = i + 1;
    while let Some(&c) = chars.get(j) {
        match c {
            '"' => return Ok((out, j + 1)),
            '\\' => {
                out.push('\\');
                if let Some(&next) = chars.get(j + 1) {
                    out.push(next);
                }
                j += 2;
            }
            _ => {
                out.push(c);
                j += 1;
            }
        }
    }
    Err(format!("{file}: unterminated string literal"))
}

/// Reads a raw string `r#"..."#` whose hashes start at `chars[i]`.
fn lex_raw_string(file: &str, chars: &[char], i: usize) -> Result<(String, usize), String> {
    let mut hashes = 0;
    let mut j = i;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return Err(format!(
            "{file}: raw identifiers and malformed raw strings are not understood by the route \
             scanner"
        ));
    }
    j += 1;
    let start = j;
    while j < chars.len() {
        if chars.get(j) == Some(&'"') && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
            let content: String = chars[start..j].iter().collect();
            return Ok((content, j + 1 + hashes));
        }
        j += 1;
    }
    Err(format!("{file}: unterminated raw string literal"))
}

/// Splits a route file into tokens, dropping whitespace, line comments and (nested) block
/// comments. An unterminated string or comment is an error.
fn tokenize(file: &str, source: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            while chars.get(i).is_some_and(|&c| c != '\n') {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 1;
            i += 2;
            while depth > 0 {
                match (chars.get(i), chars.get(i + 1)) {
                    (Some('/'), Some('*')) => {
                        depth += 1;
                        i += 2;
                    }
                    (Some('*'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                    }
                    (Some(_), _) => i += 1,
                    (None, _) => return Err(format!("{file}: unterminated block comment")),
                }
            }
        } else if c == '"' {
            let (content, end) = lex_string(file, &chars, i)?;
            toks.push(Tok::Str(content));
            i = end;
        } else if c == '\'' {
            // A character literal, or a lifetime tick.
            if next == Some('\\') {
                let mut j = i + 3;
                while chars.get(j).is_some_and(|&c| c != '\'') && j < i + 14 {
                    j += 1;
                }
                if chars.get(j) != Some(&'\'') {
                    return Err(format!("{file}: malformed character literal"));
                }
                toks.push(Tok::Str(String::new()));
                i = j + 1;
            } else if chars.get(i + 2) == Some(&'\'') {
                toks.push(Tok::Str(String::new()));
                i += 3;
            } else {
                toks.push(Tok::Punct('\''));
                i += 1;
            }
        } else if is_ident_start(c) {
            let start = i;
            while chars.get(i).is_some_and(|&c| is_ident_start(c)) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let after = chars.get(i).copied();
            if matches!(word.as_str(), "r" | "br" | "cr") && matches!(after, Some('"' | '#')) {
                let (content, end) = lex_raw_string(file, &chars, i)?;
                toks.push(Tok::Str(content));
                i = end;
            } else if matches!(word.as_str(), "b" | "c") && after == Some('"') {
                let (content, end) = lex_string(file, &chars, i)?;
                toks.push(Tok::Str(content));
                i = end;
            } else {
                toks.push(Tok::Ident(word));
            }
        } else {
            toks.push(Tok::Punct(c));
            i += 1;
        }
    }
    Ok(toks)
}

fn is_ident(tok: Option<&Tok>, name: &str) -> bool {
    matches!(tok, Some(Tok::Ident(n)) if n == name)
}

fn is_punct(tok: Option<&Tok>, c: char) -> bool {
    matches!(tok, Some(Tok::Punct(p)) if *p == c)
}

/// The index of the bracket closing the one opened at `toks[open]` (`(`, `[` or `{`), with every
/// bracket kind balanced; `None` when unbalanced.
fn matching_close(toks: &[Tok], open: usize) -> Option<usize> {
    let mut stack: Vec<char> = Vec::new();
    for (k, tok) in toks.iter().enumerate().skip(open) {
        if let Tok::Punct(c) = tok {
            match c {
                '(' => stack.push(')'),
                '[' => stack.push(']'),
                '{' => stack.push('}'),
                ')' | ']' | '}' => {
                    if stack.pop() != Some(*c) {
                        return None;
                    }
                    if stack.is_empty() {
                        return Some(k);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// The method names a route may use, in axum's `routing` module.
const METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

/// Macros a public route file may invoke inside a function body (never inside `routes()`).
const BODY_MACROS: &[&str] = &[
    "format",
    "matches",
    "json",
    "vec",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "write",
    "writeln",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "println",
    "eprintln",
];

/// The `tracing` macros, accepted only as `tracing::name!`.
const TRACING_MACROS: &[&str] = &["error", "warn", "info", "debug", "trace"];

/// Keywords that can precede a negation `!(..)` and are not macro names.
const KEYWORDS_BEFORE_BANG: &[&str] = &[
    "if", "while", "return", "match", "in", "else", "break", "continue", "yield", "mut", "let",
];

/// Names `.layer(..)`, `.route_layer(..)` and `.with_state(..)` arguments may not contain: they
/// add no path, so anything that builds or mixes in routes there is refused.
const ROUTER_BUILDERS: &[&str] = &[
    "Router",
    "MethodRouter",
    "route",
    "route_service",
    "nest",
    "nest_service",
    "merge",
    "fallback",
    "fallback_service",
    "on",
    "any",
];

/// Normalises a path template to one form: `{id}` and `:id` become `{}`, `{*rest}` and `*rest`
/// become `{*}`.
fn normalize_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if seg.starts_with("{*") || seg.starts_with('*') {
                "{*}".to_owned()
            } else if (seg.starts_with('{') && seg.ends_with('}')) || seg.starts_with(':') {
                "{}".to_owned()
            } else {
                seg.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Parses the method expression of a `.route(path, EXPR)` call: a chain of `get(..)`, `post(..)`,
/// ... calls. Returns the upper-case methods.
fn parse_method_chain(file: &str, path: &str, expr: &[Tok]) -> Result<Vec<String>, String> {
    let bad = |why: &str| {
        format!(
            "{file}: the route `{path}` has a method expression the scanner does not understand \
             ({why}); a public file may use only a chain of {} calls",
            METHODS.join("/")
        )
    };
    let mut methods: Vec<String> = Vec::new();
    let mut pos = 0;
    loop {
        let Some(Tok::Ident(name)) = expr.get(pos) else {
            return Err(bad("expected a method name"));
        };
        if !METHODS.contains(&name.as_str()) {
            return Err(bad(&format!("`{name}` is not one of the plain methods")));
        }
        if !is_punct(expr.get(pos + 1), '(') {
            return Err(bad("expected `(`"));
        }
        let Some(close) = matching_close(expr, pos + 1) else {
            return Err(bad("unbalanced brackets"));
        };
        let upper = name.to_ascii_uppercase();
        if methods.contains(&upper) {
            return Err(bad(&format!("`{name}` appears twice")));
        }
        methods.push(upper);
        pos = close + 1;
        if pos == expr.len() {
            return Ok(methods);
        }
        if !is_punct(expr.get(pos), '.') {
            return Err(bad("expected `.` or the end of the call"));
        }
        pos += 1;
    }
}

/// The index of the `routes` function and the `Router` identifiers it owns, or an error.
struct RoutesFn {
    /// Token index of `Router` in the signature and of `Router` that starts the body.
    router_idents: [usize; 2],
    /// Token range of the body, between the braces.
    body: (usize, usize),
}

fn find_routes_fn(file: &str, toks: &[Tok]) -> Result<RoutesFn, String> {
    let fns: Vec<usize> = (0..toks.len())
        .filter(|&i| is_ident(toks.get(i), "fn") && is_ident(toks.get(i + 1), "routes"))
        .collect();
    let [i] = fns[..] else {
        return Err(format!(
            "{file}: a public route file must declare exactly one `fn routes()`"
        ));
    };
    let sig_ok = is_punct(toks.get(i + 2), '(')
        && is_punct(toks.get(i + 3), ')')
        && is_punct(toks.get(i + 4), '-')
        && is_punct(toks.get(i + 5), '>')
        && is_ident(toks.get(i + 6), "Router")
        && is_punct(toks.get(i + 7), '<')
        && is_ident(toks.get(i + 8), "RouteState")
        && is_punct(toks.get(i + 9), '>')
        && is_punct(toks.get(i + 10), '{');
    if !sig_ok {
        return Err(format!(
            "{file}: `routes` must be exactly `fn routes() -> Router<RouteState> {{ .. }}`"
        ));
    }
    let open = i + 10;
    let Some(close) = matching_close(toks, open) else {
        return Err(format!("{file}: the body of `routes()` is unbalanced"));
    };
    let start = open + 1;
    let head_ok = is_ident(toks.get(start), "Router")
        && is_punct(toks.get(start + 1), ':')
        && is_punct(toks.get(start + 2), ':')
        && is_ident(toks.get(start + 3), "new")
        && is_punct(toks.get(start + 4), '(')
        && is_punct(toks.get(start + 5), ')');
    if !head_ok {
        return Err(format!(
            "{file}: `routes()` must be one expression that starts at `Router::new()` and \
             continues only with `.route(..)`, `.layer(..)`, `.route_layer(..)` and \
             `.with_state(..)` calls"
        ));
    }
    Ok(RoutesFn {
        router_idents: [i + 6, start],
        body: (start + 6, close),
    })
}

/// Reads the chain of `routes()`; returns the (METHOD, normalised path) of every `.route`.
fn parse_chain(
    file: &str,
    toks: &[Tok],
    body: (usize, usize),
) -> Result<Vec<(String, String)>, String> {
    let mut mounted: Vec<(String, String)> = Vec::new();
    let mut pos = body.0;
    while pos < body.1 {
        let (Some(Tok::Punct('.')), Some(Tok::Ident(name)), true) = (
            toks.get(pos),
            toks.get(pos + 1),
            is_punct(toks.get(pos + 2), '('),
        ) else {
            return Err(format!(
                "{file}: `routes()` must continue only with `.route(..)`, `.layer(..)`, \
                 `.route_layer(..)` and `.with_state(..)` calls after `Router::new()`"
            ));
        };
        let Some(close) = matching_close(toks, pos + 2) else {
            return Err(format!("{file}: `.{name}(` in `routes()` is unbalanced"));
        };
        let args = toks.get(pos + 3..close).unwrap_or(&[]);
        match name.as_str() {
            "route" => {
                let (Some(Tok::Str(path)), true) = (args.first(), is_punct(args.get(1), ','))
                else {
                    return Err(format!(
                        "{file}: a `.route(` call whose first argument is not a plain string \
                         literal path; a public file may use only literal paths"
                    ));
                };
                if path.contains('\\') || !path.starts_with('/') {
                    return Err(format!(
                        "{file}: the route path `{path}` is not a plain literal path starting \
                         with `/`"
                    ));
                }
                let mut expr = args.get(2..).unwrap_or(&[]);
                if is_punct(expr.last(), ',') {
                    expr = &expr[..expr.len() - 1];
                }
                for method in parse_method_chain(file, path, expr)? {
                    let entry = (method, normalize_path(path));
                    if mounted.contains(&entry) {
                        return Err(format!("{file}: `{} {path}` is mounted twice", entry.0));
                    }
                    mounted.push(entry);
                }
            }
            "layer" | "route_layer" | "with_state" => {
                for (k, tok) in args.iter().enumerate() {
                    let forbidden = match tok {
                        Tok::Ident(n) => ROUTER_BUILDERS.contains(&n.as_str()),
                        Tok::Punct('!') => !is_punct(args.get(k + 1), '='),
                        _ => false,
                    };
                    if forbidden {
                        return Err(format!(
                            "{file}: the arguments of `.{name}(` may not build or mix in \
                             routes or invoke a macro; a public file's layers add no path"
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "{file}: `.{other}(` is not allowed in a public file's `routes()`; only \
                     `.route(..)`, `.layer(..)`, `.route_layer(..)` and `.with_state(..)` are"
                ));
            }
        }
        pos = close + 1;
    }
    if pos != body.1 {
        return Err(format!(
            "{file}: `routes()` has trailing code after its chain"
        ));
    }
    Ok(mounted)
}

/// The token ranges of the `use` statements (from `use` to `;`).
fn use_ranges(file: &str, toks: &[Tok]) -> Result<Vec<(usize, usize)>, String> {
    let mut ranges = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if matches!(tok, Tok::Ident(n) if n == "use") {
            let Some(end) = (i..toks.len()).find(|&k| is_punct(toks.get(k), ';')) else {
                return Err(format!("{file}: a `use` statement has no `;`"));
            };
            ranges.push((i, end));
        }
    }
    Ok(ranges)
}

/// Bans the constructs that can hide a route from the scanner (see [`mounted_routes`]).
fn check_hiding_places(file: &str, toks: &[Tok], routes_fn: &RoutesFn) -> Result<(), String> {
    let uses = use_ranges(file, toks)?;
    let in_use = |i: usize| uses.iter().any(|&(a, b)| i >= a && i <= b);
    for (a, b) in &uses {
        let span = toks.get(*a..=*b).unwrap_or(&[]);
        for tok in span {
            let Tok::Ident(n) = tok else { continue };
            let shadows = n == "as"
                || (BODY_MACROS.contains(&n.as_str()) && n != "json")
                || n == "MethodRouter"
                || METHODS.contains(&n.as_str()) && !span.contains(&Tok::Ident("routing".into()));
            if shadows {
                return Err(format!(
                    "{file}: a `use` statement that renames (`as`), or imports `{n}` from \
                     outside `axum::routing`, could hide a route from the scanner"
                ));
            }
            if n == "json" && !span.contains(&Tok::Ident("serde_json".into())) {
                return Err(format!(
                    "{file}: `json` may be imported only from `serde_json`"
                ));
            }
        }
    }
    // Brace stack: is each open block (or an enclosing one) a function body?
    let mut stack: Vec<bool> = Vec::new();
    let mut saw_fn = false;
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Ident(n) => {
                match n.as_str() {
                    "fn" => saw_fn = true,
                    "macro_rules" | "macro" => {
                        return Err(format!(
                            "{file}: a public route file may not declare macros (`{n}`): the \
                             scanner cannot see the routes a macro expands to"
                        ));
                    }
                    "mod" => {
                        return Err(format!(
                            "{file}: a public route file may not declare `mod`: the scanner \
                             reads only this file"
                        ));
                    }
                    "include" | "include_str" | "include_bytes" | "env" | "option_env"
                        if is_punct(toks.get(i + 1), '!') =>
                    {
                        return Err(format!(
                            "{file}: `{n}!` is not allowed in a public route file: the scanner \
                             reads only this file's text"
                        ));
                    }
                    "Router" | "MethodRouter" if !in_use(i) => {
                        if !routes_fn.router_idents.contains(&i) {
                            return Err(format!(
                                "{file}: `{n}` appears outside `routes()`: a Router may not be \
                                 passed to or returned from any other function or type"
                            ));
                        }
                    }
                    "get" | "post" | "put" | "delete" | "patch" | "head" | "options" | "on"
                    | "any"
                        if is_ident(toks.get(i.wrapping_sub(1)), "fn") =>
                    {
                        return Err(format!("{file}: a function named `{n}` could hide a route"));
                    }
                    _ => {}
                }
                // A macro invocation: `name!(`, `name![`, `name!{`.
                if is_punct(toks.get(i + 1), '!')
                    && matches!(toks.get(i + 2), Some(Tok::Punct('(' | '[' | '{')))
                    && !KEYWORDS_BEFORE_BANG.contains(&n.as_str())
                {
                    let qualified = is_punct(toks.get(i.wrapping_sub(1)), ':');
                    // `tracing::name!` only, with `tracing` the first segment of the path (so
                    // `crate::evil::tracing::error!` is not read as the logging macro).
                    let tracing = qualified
                        && is_ident(toks.get(i.wrapping_sub(3)), "tracing")
                        && !is_punct(toks.get(i.wrapping_sub(4)), ':')
                        && TRACING_MACROS.contains(&n.as_str());
                    let plain = !qualified && BODY_MACROS.contains(&n.as_str());
                    let (body_start, body_end) = routes_fn.body;
                    if i >= body_start && i < body_end {
                        return Err(format!(
                            "{file}: a macro (`{n}!`) inside `routes()`: the scanner cannot see \
                             what it expands to"
                        ));
                    }
                    if !(plain || tracing) || !stack.contains(&true) {
                        return Err(format!(
                            "{file}: the macro `{n}!` is not one a public route file may \
                             invoke (only {} inside function bodies, and `tracing::error!`-style \
                             logging)",
                            BODY_MACROS.join(", ")
                        ));
                    }
                }
            }
            Tok::Punct('{') => {
                stack.push(saw_fn);
                saw_fn = false;
            }
            Tok::Punct('}') => {
                stack.pop();
                saw_fn = false;
            }
            Tok::Punct(';') => saw_fn = false,
            _ => {}
        }
    }
    Ok(())
}

/// Method-like names a local definition may never take (they would shadow axum's).
fn is_method_name(n: &str) -> bool {
    METHODS.contains(&n) || n == "on" || n == "any"
}

/// Attribute names a public route file may use; every other (`cfg`, `cfg_attr`, `path`,
/// `macro_use`, ...) could compile a decoy in or the real code out.
const ATTRIBUTES: &[&str] = &[
    "doc", "allow", "warn", "deny", "expect", "derive", "must_use", "inline",
];

/// Reads one `use` tree at `pos` (after `use` or after `{`/`,`), appending the full token path of
/// every leaf to `out`; returns the index after the tree. Anything but plain paths and braces
/// (`*`, `as`, a leading `::`, ...) is an error: the check understands nothing else.
fn use_tree(
    file: &str,
    toks: &[Tok],
    mut pos: usize,
    prefix: &[String],
    depth: usize,
    out: &mut Vec<Vec<String>>,
) -> Result<usize, String> {
    let bad =
        |why: &str| format!("{file}: a `use` in a public route file is not understood ({why})");
    let mut path: Vec<String> = prefix.to_vec();
    loop {
        match toks.get(pos) {
            Some(Tok::Ident(n)) if n != "as" => {
                path.push(n.clone());
                pos += 1;
                if is_punct(toks.get(pos), ':') && is_punct(toks.get(pos + 1), ':') {
                    pos += 2;
                } else {
                    out.push(path);
                    return Ok(pos);
                }
            }
            Some(Tok::Punct('{')) => {
                if depth > 32 {
                    return Err(bad("nested too deeply"));
                }
                pos += 1;
                loop {
                    if is_punct(toks.get(pos), '}') {
                        return Ok(pos + 1);
                    }
                    pos = use_tree(file, toks, pos, &path, depth + 1, out)?;
                    if is_punct(toks.get(pos), ',') {
                        pos += 1;
                    } else if !is_punct(toks.get(pos), '}') {
                        return Err(bad("expected `,` or `}`"));
                    }
                }
            }
            Some(Tok::Punct('*')) => return Err(bad("a glob import could supply any name")),
            _ => return Err(bad("renames, `::` roots and other forms are refused")),
        }
    }
}

/// The policy that does not depend on what `routes()` mounts: nothing may stand in for, hide, or
/// replace the checked function or the names it uses (see [`mounted_routes`]).
fn check_policy(file: &str, toks: &[Tok]) -> Result<(), String> {
    // `routes`: exactly one identifier, `pub fn routes` at brace depth 0.
    let named: Vec<usize> = (0..toks.len())
        .filter(|&i| is_ident(toks.get(i), "routes"))
        .collect();
    let [r] = named[..] else {
        return Err(format!(
            "{file}: the identifier `routes` must appear exactly once, as `pub fn routes()`              (no re-export, alias, decoy or nested function)"
        ));
    };
    let depth_at = |upto: usize| {
        let (mut d, mut min) = (0i64, 0i64);
        for t in toks.iter().take(upto) {
            match t {
                Tok::Punct('{') => d += 1,
                Tok::Punct('}') => {
                    d -= 1;
                    min = min.min(d);
                }
                _ => {}
            }
        }
        (d, min)
    };
    if depth_at(r) != (0, 0)
        || !is_ident(toks.get(r.wrapping_sub(1)), "fn")
        || !is_ident(toks.get(r.wrapping_sub(2)), "pub")
    {
        return Err(format!(
            "{file}: `routes` must be `pub fn routes()` at the top level of the file"
        ));
    }
    // Attributes, `extern`, `use` trees.
    let uses = use_ranges(file, toks)?;
    let mut leaves: Vec<Vec<String>> = Vec::new();
    for (a, b) in &uses {
        let end = use_tree(file, toks, a + 1, &[], 0, &mut leaves)?;
        if end != *b {
            return Err(format!(
                "{file}: a `use` in a public route file is not understood"
            ));
        }
    }
    for leaf in &leaves {
        let last = leaf.last().map_or("", String::as_str);
        let axum_routing = leaf.len() == 3 && leaf[0] == "axum" && leaf[1] == "routing";
        let ok = if is_method_name(last) {
            axum_routing
        } else if last == "Router" {
            leaf.len() == 2 && leaf[0] == "axum"
        } else if last == "axum" || last == "tracing" || last == "serde_json" {
            leaf.len() == 1
        } else if last == "json" {
            leaf.len() == 2 && leaf[0] == "serde_json"
        } else {
            true
        };
        if !ok {
            return Err(format!(
                "{file}: `use {}` could shadow a name `routes()` relies on; `{last}` may be \
                 imported only from `axum` (`axum::routing::<method>`, `axum::Router`)",
                leaf.join("::")
            ));
        }
    }
    let in_use = |i: usize| uses.iter().any(|&(a, b)| i >= a && i <= b);
    let mut depth = 0i64;
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Punct('{') => depth += 1,
            Tok::Punct('}') => depth -= 1,
            Tok::Punct('#') => {
                let open = if is_punct(toks.get(i + 1), '!') {
                    i + 2
                } else {
                    i + 1
                };
                let first = match (toks.get(open), toks.get(open + 1)) {
                    (Some(Tok::Punct('[')), Some(Tok::Ident(n))) => n.as_str(),
                    _ => "",
                };
                if !ATTRIBUTES.contains(&first) {
                    return Err(format!(
                        "{file}: the attribute `#[{first}..]` (or a stray `#`) is not allowed \
                         in a public route file; only {} are",
                        ATTRIBUTES.join(", ")
                    ));
                }
            }
            Tok::Ident(n) if n == "extern" => {
                return Err(format!(
                    "{file}: `extern` is not allowed in a public route file: it can shadow `axum`"
                ));
            }
            Tok::Ident(n) if is_method_name(n) && !in_use(i) => {
                let prev = toks.get(i.wrapping_sub(1));
                let next = toks.get(i + 1);
                let defines = matches!(prev, Some(Tok::Ident(k)) if matches!(k.as_str(),
                    "fn" | "const" | "static" | "let" | "struct" | "enum" | "type" | "trait"
                    | "union" | "mut" | "ref" | "mod" | "macro_rules"))
                    || is_punct(prev, '|');
                let used_only = is_punct(prev, '.')
                    || is_punct(prev, ':')
                    || (depth > 0 && is_punct(next, '('));
                if defines || !used_only {
                    return Err(format!(
                        "{file}: the name `{n}` is defined or bound here; a public route file \
                         may not define, bind or import a name that `routes()` uses as a method"
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The (METHOD, normalised path) pairs a public route file mounts, sorted, or why the scanner
/// does not understand the file. Fail closed: a file is accepted only when
/// - `routes()` is `fn routes() -> Router<RouteState> { Router::new() ... }` continuing only with
///   `.route(<string literal>, <chain of get/post/put/delete/patch/head/options>)`, `.layer`,
///   `.route_layer` and `.with_state` calls (so no `.nest`, `.merge`, `.fallback`, `on`, `any`,
///   computed path or `MethodRouter::new()`);
/// - the file declares no `macro_rules!`, `mod`, `include!`, `include_str!`, and invokes no macro
///   outside a function body or inside `routes()`;
/// - `Router` is named nowhere else (a helper cannot take or return one), and no `use` renames.
///
/// Comments and strings are removed by the tokenizer before any of this, so a `.route(` inside
/// one neither counts nor hides a call.
#[allow(dead_code)]
pub fn mounted_routes(file: &str, source: &str) -> Result<Vec<(String, String)>, String> {
    let toks = tokenize(file, source)?;
    check_policy(file, &toks)?;
    let routes_fn = find_routes_fn(file, &toks)?;
    check_hiding_places(file, &toks, &routes_fn)?;
    let mut mounted = parse_chain(file, &toks, routes_fn.body)?;
    mounted.sort();
    Ok(mounted)
}

/// One operation of the contract with its method and path template.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Endpoint {
    /// The `operationId`.
    pub id: String,
    /// Upper-case method.
    pub method: String,
    /// The path template as written under `paths:`.
    pub path: String,
}

/// The operations of the contract text with their method and path: a path is a key at two
/// spaces under `paths:`, a method a key at four, `operationId:` a key at six.
#[allow(dead_code)]
pub fn contract_endpoints(yaml: &str) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let mut in_paths = false;
    let mut path: Option<String> = None;
    let mut method: Option<String> = None;
    for raw in yaml.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_paths = line == "paths:";
            path = None;
            method = None;
            continue;
        }
        if !in_paths {
            continue;
        }
        if indent == 2 {
            path = trimmed
                .strip_suffix(':')
                .map(|p| p.trim_matches(['"', '\'']).to_owned())
                .filter(|p| p.starts_with('/'));
            method = None;
        } else if indent == 4 {
            method = trimmed
                .strip_suffix(':')
                .filter(|m| METHODS.contains(m))
                .map(str::to_ascii_uppercase);
        } else if indent == 6
            && let (Some(p), Some(m), Some(id)) =
                (&path, &method, trimmed.strip_prefix("operationId:"))
        {
            out.push(Endpoint {
                id: id.trim().to_owned(),
                method: m.clone(),
                path: p.clone(),
            });
        }
    }
    out
}

#[allow(dead_code)]
/// `SERVES` as the tokens spell it must be exactly the list the text scan read (`serves_of` reads
/// the first column-0 line, which a block comment or a string can fake): one `SERVES` identifier,
/// as `pub const SERVES: &[&str] = &["id", ..];`, with the same ids in the same order.
pub fn check_serves_tokens(file: &str, source: &str, serves: &[String]) -> Result<(), String> {
    let toks = tokenize(file, source)?;
    let at: Vec<usize> = (0..toks.len())
        .filter(|&i| is_ident(toks.get(i), "SERVES"))
        .collect();
    let bad = || {
        format!(
            "{file}: `SERVES` must be declared exactly once, as `pub const SERVES: &[&str] = \
             &[\"id\", ..];`, and be the list the build read (no decoy in a comment or a string)"
        )
    };
    let [i] = at[..] else {
        return Err(bad());
    };
    let shape = [':', '&', '[', '&'];
    let mut pos = i + 1;
    let head_ok = is_ident(toks.get(i.wrapping_sub(1)), "const")
        && is_ident(toks.get(i.wrapping_sub(2)), "pub")
        && shape.iter().all(|c| {
            let ok = is_punct(toks.get(pos), *c);
            pos += 1;
            ok
        })
        && is_ident(toks.get(pos), "str")
        && is_punct(toks.get(pos + 1), ']')
        && is_punct(toks.get(pos + 2), '=')
        && is_punct(toks.get(pos + 3), '&')
        && is_punct(toks.get(pos + 4), '[');
    if !head_ok {
        return Err(bad());
    }
    pos += 5;
    let mut ids: Vec<&str> = Vec::new();
    loop {
        match toks.get(pos) {
            Some(Tok::Str(id)) => {
                ids.push(id);
                pos += 1;
                if is_punct(toks.get(pos), ',') {
                    pos += 1;
                }
            }
            Some(Tok::Punct(']')) => break,
            _ => return Err(bad()),
        }
    }
    if !is_punct(toks.get(pos + 1), ';')
        || ids != serves.iter().map(String::as_str).collect::<Vec<_>>()
    {
        return Err(bad());
    }
    Ok(())
}

/// The path and method rule for one public route file: it mounts exactly the (method, path) of
/// the contract operations of `serves`, no more, no less. `file` and `contract_file` are only
/// named in messages.
#[allow(dead_code)]
pub fn check_public_routes(
    file: &str,
    source: &str,
    contract_file: &str,
    yaml: &str,
    serves: &[String],
) -> Result<(), String> {
    let mounted = mounted_routes(file, source)?;
    let endpoints = contract_endpoints(yaml);
    let mut expected: Vec<(String, String, &str)> = Vec::new();
    for id in serves {
        let found: Vec<&Endpoint> = endpoints.iter().filter(|e| &e.id == id).collect();
        if found.is_empty() {
            return Err(format!(
                "{file}: operation `{id}` is not an operation of {contract_file}"
            ));
        }
        for e in found {
            expected.push((e.method.clone(), normalize_path(&e.path), id));
        }
    }
    for (method, path) in &mounted {
        if !expected.iter().any(|(m, p, _)| m == method && p == path) {
            return Err(format!(
                "{file}: the public file mounts `{method} {path}`, which is not the method and \
                 path of any of its SERVES operations ({}); a public file mounts exactly the \
                 contract paths of its SERVES",
                serves.join(", ")
            ));
        }
    }
    for (method, path, id) in &expected {
        if !mounted.iter().any(|(m, p)| m == method && p == path) {
            return Err(format!(
                "{file}: operation `{id}` ({method} {path} in {contract_file}) is not mounted \
                 by the public file's `routes()`"
            ));
        }
    }
    Ok(())
}

/// Reads one module file's text. A ports file registers when it declares `pub const KEY:
/// PortKey` at column 0. A routes file registers when it declares `pub const SERVES: &[&str]`,
/// `pub fn routes(` and `pub fn required_ports(` at column 0; declaring only some of them is an
/// error, so a half-written group never compiles into a silent no-op. A file that declares none
/// is a helper and only becomes a module.
#[allow(dead_code)]
pub fn inspect(kind: Kind, file_name: &str, source: &str) -> Result<bool, String> {
    let has = |prefix: &str| source.lines().any(|l| l.starts_with(prefix));
    match kind {
        Kind::Ports => Ok(has("pub const KEY: PortKey")),
        Kind::Routes => {
            let found = [
                ("pub const SERVES: &[&str]", "`pub const SERVES: &[&str]`"),
                ("pub fn routes(", "`pub fn routes()`"),
                ("pub fn required_ports(", "`pub fn required_ports()`"),
            ];
            let present: Vec<bool> = found.iter().map(|(p, _)| has(p)).collect();
            if present.iter().all(|p| *p) {
                return Ok(true);
            }
            if has(PUBLIC_MARKER) && present.iter().any(|p| !*p) {
                return Err(format!(
                    "{file_name}: declares PUBLIC but not SERVES, routes and required_ports \
                     together"
                ));
            }
            if present.iter().any(|p| *p) {
                let missing: Vec<&str> = found
                    .iter()
                    .zip(&present)
                    .filter(|(_, p)| !**p)
                    .map(|((_, label), _)| *label)
                    .collect();
                return Err(format!(
                    "{file_name}: a route module declares SERVES, routes and required_ports \
                     together; missing {}",
                    missing.join(", ")
                ));
            }
            Ok(false)
        }
    }
}

/// Scans the manifest's directory for `kind` in file-name order. A missing directory is empty.
#[allow(dead_code)]
pub fn scan(manifest_dir: &Path, kind: Kind) -> Result<Vec<Module>, String> {
    let dir = manifest_dir.join(kind.dir());
    let mut files: Vec<PathBuf> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files.sort();
    let mut modules = Vec::new();
    for path in files {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: not a UTF-8 file name", path.display()))?
            .to_owned();
        if file_name == "mod.rs" {
            continue;
        }
        let stem = file_name.trim_end_matches(".rs").to_owned();
        let rel = format!("{}/{file_name}", kind.dir());
        if !valid_module_name(&stem) {
            return Err(format!(
                "{rel}: the file name is not a valid module name \
                 (lowercase letters, digits and underscores, not starting with a digit)"
            ));
        }
        let source = std::fs::read_to_string(&path).map_err(|e| format!("{rel}: {e}"))?;
        let registers = inspect(kind, &rel, &source)?;
        let (public, serves) = if kind == Kind::Routes && registers {
            (is_public(&source), serves_of(&rel, &source)?)
        } else {
            (false, Vec::new())
        };
        modules.push(Module {
            path: rel,
            name: stem,
            registers,
            public,
            serves,
        });
    }
    Ok(modules)
}

/// The generated source for `modules`; `manifest_dir` is made forward-slashed because
/// `#[path]` needs a literal.
#[allow(dead_code)]
pub fn generate(manifest_dir: &str, kind: Kind, modules: &[Module]) -> String {
    let dir = manifest_dir.replace('\\', "/");
    let mut out = format!(
        "// @generated by build.rs from {}/*.rs. Do not edit.\n",
        kind.dir()
    );
    for m in modules {
        out.push_str(&format!(
            "#[path = \"{dir}/{}\"]\npub mod {};\n",
            m.path, m.name
        ));
    }
    match kind {
        Kind::Ports => {
            out.push_str("/// Every declared port key, in file-name order.\n");
            out.push_str("pub const REGISTERED: &[PortKey] = &[\n");
            for m in modules.iter().filter(|m| m.registers) {
                out.push_str(&format!("    {}::KEY,\n", m.name));
            }
            out.push_str("];\n");
        }
        Kind::Routes => {
            out.push_str(
                "/// Every route module that declares what it serves, in file-name order.\n",
            );
            out.push_str("pub const REGISTRY: &[Registration] = &[\n");
            for m in modules.iter().filter(|m| m.registers) {
                let public = if m.public {
                    format!("Some({}::PUBLIC)", m.name)
                } else {
                    "None".to_owned()
                };
                out.push_str(&format!(
                    "    Registration {{ name: \"{0}\", serves: {0}::SERVES, required_ports: {0}::required_ports, routes: {0}::routes, public: {1} }},\n",
                    m.name, public
                ));
            }
            out.push_str("];\n");
        }
    }
    out
}

/// Where the contract lives, from the crate's manifest directory.
const CONTRACT: &str = "../../../contracts/native-api/openapi.yaml";

/// Runs the security rule over the scanned route groups against the real contract. A missing
/// contract is an error: the rule cannot be skipped by moving the file.
#[allow(dead_code)]
fn check_contract(manifest_dir: &Path, modules: &[Module]) -> Result<(), String> {
    let path = manifest_dir.join(CONTRACT);
    let yaml = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: cannot read the contract to check which route groups may be public: {e}",
            path.display()
        )
    })?;
    let groups: Vec<GroupFacts<'_>> = modules
        .iter()
        .filter(|m| m.registers)
        .map(|m| GroupFacts {
            name: &m.name,
            public: m.public,
            serves: &m.serves,
        })
        .collect();
    check_security("contracts/native-api/openapi.yaml", &yaml, &groups)?;
    for m in modules.iter().filter(|m| m.registers && m.public) {
        let source = std::fs::read_to_string(manifest_dir.join(&m.path))
            .map_err(|e| format!("{}: {e}", m.path))?;
        check_serves_tokens(&m.path, &source, &m.serves)?;
        check_public_routes(
            &m.path,
            &source,
            "contracts/native-api/openapi.yaml",
            &yaml,
            &m.serves,
        )?;
    }
    Ok(())
}

#[allow(dead_code)]
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into());
    println!("cargo:rerun-if-changed=src/api/ports");
    println!("cargo:rerun-if-changed=src/api/router/routes");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={CONTRACT}");
    for (kind, file) in [
        (Kind::Ports, "ports_registry.rs"),
        (Kind::Routes, "routes_registry.rs"),
    ] {
        let modules = match scan(Path::new(&manifest), kind) {
            Ok(m) => m,
            Err(message) => {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        };
        if kind == Kind::Routes
            && let Err(message) = check_contract(Path::new(&manifest), &modules)
        {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        let source = generate(&manifest, kind, &modules);
        if let Err(e) = std::fs::write(Path::new(&out_dir).join(file), source) {
            eprintln!("error: cannot write {file}: {e}");
            std::process::exit(1);
        }
    }
}
