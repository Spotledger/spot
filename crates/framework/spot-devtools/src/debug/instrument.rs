//! The line-hook shim: the debugger's only foothold in the engine.
//!
//! `spot-ts` exposes no debug hooks (see the task's open question): it makes a fresh QuickJS
//! runtime per execution and offers an interrupt handler but no line callback. What it *does*
//! offer is a closed host object (`host`) and a source-carrying compile step. This module
//! therefore rewrites the transpiled JavaScript before it is compiled:
//!
//! - before every statement-start line it inserts `host.__dap_line({"line":N,"locals":{...}})`:
//!   the breakpoint and step hook, carrying a snapshot of the frame's locals;
//! - around every `function` body it inserts `host.__dap_enter(...)` and a `try/finally` that
//!   calls `host.__dap_exit(...)`: the frame the stack view shows and the depth stepping uses.
//!
//! The inserted calls take **one** argument (the engine's host functions take one), never add a
//! line (so the source map's line numbers still hold) and call nothing that can observe or change
//! the script's state. The host side ([`crate::debug`]) blocks the script's own thread inside the
//! hook; no other execution is affected.
//!
//! # What it cannot see
//!
//! Class methods and concise arrow bodies are not wrapped in a frame (their lines still hook when
//! they sit in a block), and a local whose name is not a plain `let`/`const`/`var` binding is not
//! in the snapshot. Both are listed as open questions: the real fix is `JS_SetDebugTraceHandler`
//! exposed by `spot-ts`, not a smarter rewriter.

/// The result of instrumenting one script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instrumented {
    /// The JavaScript with hooks in place.
    pub source: String,
    /// The generated lines that now start a hook (1-based, source-map coordinates).
    pub hook_lines: Vec<u32>,
    /// The functions whose bodies were framed, outermost first.
    pub framed_functions: Vec<String>,
}

/// One function body the rewriter frames.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FunctionBody {
    name: String,
    body_open: usize,
    body_close: usize,
    params: Vec<String>,
    line: u32,
}

/// The prefix of the three hook functions on the script's `host` object.
pub const HOOK_LINE: &str = "__dap_line";
/// The function called on entering a framed function.
pub const HOOK_ENTER: &str = "__dap_enter";
/// The function called on leaving a framed function.
pub const HOOK_EXIT: &str = "__dap_exit";

/// Instruments `js`. The input is the transpiled JavaScript; the line numbers in the result's
/// [`Instrumented::hook_lines`] refer to that input, which the source map describes.
#[must_use]
pub fn instrument(js: &str) -> Instrumented {
    let masked = mask_js(js);
    let line_starts = line_starts(js);
    let line_of = |offset: usize| -> u32 {
        let index = line_starts.partition_point(|start| *start <= offset);
        u32::try_from(index).unwrap_or(u32::MAX)
    };
    let pairs = brace_pairs(&masked);
    let functions = find_functions(js, &masked, &pairs, &line_of);

    let mut insertions: Vec<(usize, String)> = Vec::new();
    let mut hook_lines = Vec::new();
    for start in statement_hook_offsets(&masked, &line_starts) {
        let index = line_starts.partition_point(|line| *line <= start);
        let line = u32::try_from(index).unwrap_or(u32::MAX);
        let end = line_starts.get(index).copied().unwrap_or(js.len());
        let locals = locals_for(start, &functions, js, &masked);
        hook_lines.push(line);
        insertions.push((
            first_code_offset(&masked, start, end),
            line_call(line, &locals),
        ));
    }

    for function in &functions {
        let name = json_string(&function.name);
        insertions.push((
            function.body_open + 1,
            format!(
                "host.{HOOK_ENTER}({{\"name\":{name},\"line\":{}}});try{{",
                function.line
            ),
        ));
        insertions.push((
            function.body_close,
            format!("}}finally{{host.{HOOK_EXIT}(null);}}"),
        ));
    }

    insertions.sort_by_key(|a| a.0);
    let mut source = String::with_capacity(js.len() + insertions.len() * 48);
    let mut at = 0;
    for (offset, text) in insertions {
        let offset = offset.min(js.len());
        if offset < at {
            // Two insertions at one offset keep their accumulated order: never rewind.
            source.push_str(&text);
            continue;
        }
        source.push_str(&js[at..offset]);
        source.push_str(&text);
        at = offset;
    }
    source.push_str(&js[at.min(js.len())..]);
    hook_lines.sort_unstable();
    hook_lines.dedup();
    Instrumented {
        source,
        hook_lines,
        framed_functions: functions.into_iter().map(|f| f.name).collect(),
    }
}

fn line_call(line: u32, locals: &str) -> String {
    format!("host.{HOOK_LINE}({{\"line\":{line},\"locals\":{locals}}});")
}

fn first_code_offset(masked: &[u8], start: usize, end: usize) -> usize {
    let mut at = start;
    while at < end && (masked[at] == b' ' || masked[at] == b'\t' || masked[at] == b'\r') {
        at += 1;
    }
    at
}

/// The lines that begin a statement, as offsets, found in one pass that also tracks the structural
/// frame stack: a hook is only valid where a statement may begin (inside a block, not inside an
/// object literal, an array, an argument list or a class body).
fn statement_hook_offsets(masked: &[u8], line_starts: &[usize]) -> Vec<usize> {
    let mut hooks = Vec::new();
    let mut blocks: Vec<bool> = Vec::new();
    let (mut paren, mut bracket) = (0_i32, 0_i32);
    let mut previous: Option<u8> = None;
    for (index, start) in line_starts.iter().enumerate() {
        let end = line_starts
            .get(index + 1)
            .map_or(masked.len(), |next| next.saturating_sub(1));
        let at = first_code_offset(masked, *start, end);
        let in_block = blocks.last().copied().unwrap_or(true);
        let follows_statement = previous.is_none_or(|byte| matches!(byte, b';' | b'{' | b'}'));
        let do_while = previous == Some(b'}') && starts_with_word(&masked[at..end], b"while");
        if at < end
            && paren == 0
            && bracket == 0
            && in_block
            && follows_statement
            && !do_while
            && line_shape_ok(&masked[at..end])
        {
            hooks.push(*start);
        }
        for (offset, byte) in masked.iter().enumerate().take(end).skip(*start) {
            match byte {
                b'(' => paren += 1,
                b')' => paren -= 1,
                b'[' => bracket += 1,
                b']' => bracket -= 1,
                b'{' => blocks.push(is_block_brace(masked, offset)),
                b'}' => {
                    blocks.pop();
                }
                _ => {}
            }
            if !byte.is_ascii_whitespace() {
                previous = Some(*byte);
            }
        }
    }
    hooks
}

/// Whether a `{` opens a statement block (`true`) or an object literal / class body (`false`).
fn is_block_brace(masked: &[u8], at: usize) -> bool {
    let Some(previous) = previous_significant(masked, at) else {
        return true;
    };
    match previous {
        b')' | b'}' | b';' | b'{' => true,
        b'>' => {
            let before = previous_significant(masked, at.saturating_sub(1));
            before == Some(b'=')
        }
        byte if is_ident(byte) => {
            let mut start = at;
            while start > 0 && is_ident(masked[start - 1]) {
                start -= 1;
            }
            let word = &masked[start..at];
            matches!(word, b"else" | b"try" | b"finally" | b"do")
        }
        _ => false,
    }
}

/// Whether the text of a line looks like a statement start rather than a continuation or a label.
fn line_shape_ok(text: &[u8]) -> bool {
    let Some(first) = text.first().copied() else {
        return false;
    };
    if matches!(
        first,
        b'}' | b')'
            | b']'
            | b'.'
            | b','
            | b':'
            | b'?'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'&'
            | b'|'
            | b'^'
            | b'='
            | b'<'
            | b'>'
            | b'!'
            | b';'
    ) {
        return false;
    }
    for keyword in [
        b"else".as_slice(),
        b"catch",
        b"finally",
        b"case",
        b"default",
    ] {
        if starts_with_word(text, keyword) {
            return false;
        }
    }
    true
}

fn starts_with_word(text: &[u8], word: &[u8]) -> bool {
    if !text.starts_with(word) {
        return false;
    }
    text.get(word.len()).is_none_or(|next| !is_ident(*next))
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn previous_significant(masked: &[u8], at: usize) -> Option<u8> {
    let mut index = at;
    while index > 0 {
        index -= 1;
        let byte = masked[index];
        if !byte.is_ascii_whitespace() {
            return Some(byte);
        }
    }
    None
}

fn brace_pairs(masked: &[u8]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; masked.len()];
    let mut stack = Vec::new();
    for (index, byte) in masked.iter().enumerate() {
        match byte {
            b'{' => stack.push(index),
            b'}' => {
                if let Some(open) = stack.pop() {
                    pairs[open] = Some(index);
                }
            }
            _ => {}
        }
    }
    pairs
}

fn find_functions(
    js: &str,
    masked: &[u8],
    pairs: &[Option<usize>],
    line_of: &impl Fn(usize) -> u32,
) -> Vec<FunctionBody> {
    let mut functions = Vec::new();
    let bytes = masked;
    let mut at = 0;
    while at + 8 <= bytes.len() {
        if &bytes[at..at + 8] != b"function" {
            at += 1;
            continue;
        }
        let before_ok = at == 0 || !is_ident(bytes[at - 1]);
        let after = bytes.get(at + 8).copied().unwrap_or(b' ');
        if !before_ok || is_ident(after) {
            at += 1;
            continue;
        }
        let mut cursor = at + 8;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'*') {
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
        let name_start = cursor;
        while cursor < bytes.len() && is_ident(bytes[cursor]) {
            cursor += 1;
        }
        let name = if cursor > name_start {
            js.get(name_start..cursor).unwrap_or("").to_owned()
        } else {
            "anonymous".to_owned()
        };
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'(') {
            at += 8;
            continue;
        }
        let params_open = cursor;
        let Some(params_close) = matching(bytes, params_open, b'(', b')') else {
            at += 8;
            continue;
        };
        let params = split_params(js.get(params_open + 1..params_close).unwrap_or(""));
        cursor = params_close + 1;
        while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b':')
        {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'{') {
            at += 8;
            continue;
        }
        let Some(close) = pairs.get(cursor).copied().flatten() else {
            at += 8;
            continue;
        };
        functions.push(FunctionBody {
            name,
            body_open: cursor,
            body_close: close,
            params,
            line: line_of(at),
        });
        at = cursor + 1;
    }
    functions
}

fn matching(bytes: &[u8], open: usize, opening: u8, closing: u8) -> Option<usize> {
    let mut depth = 0_i32;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        if *byte == opening {
            depth += 1;
        } else if *byte == closing {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn split_params(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for part in split_top_level(text, ',') {
        let part = part.trim().trim_start_matches("...");
        let mut chars = part.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
            continue;
        }
        let mut name = String::from(first);
        for c in chars {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                name.push(c);
            } else {
                break;
            }
        }
        names.push(name);
    }
    names
}

fn split_top_level(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if c == separator && depth == 0 => {
                parts.push(&text[start..index]);
                start = index + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// The locals a hook at `offset` can show: the enclosing function's parameters and the plain
/// `let`/`const`/`var` names declared before the hook in the same body (module-level names when no
/// function encloses it).
fn locals_for(offset: usize, functions: &[FunctionBody], js: &str, masked: &[u8]) -> String {
    let enclosing = functions
        .iter()
        .filter(|function| function.body_open < offset && offset < function.body_close)
        .min_by_key(|function| function.body_close - function.body_open);
    let (scope_start, params): (usize, &[String]) = match enclosing {
        Some(function) => (function.body_open + 1, function.params.as_slice()),
        None => (0, &[]),
    };
    let mut names: Vec<String> = params.to_vec();
    let mut at = scope_start;
    while at + 3 <= offset.min(masked.len()) {
        let byte = masked[at];
        if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' {
            let start = at;
            while at < masked.len() && is_ident(masked[at]) {
                at += 1;
            }
            let word = &masked[start..at];
            if matches!(word, b"let" | b"const" | b"var") {
                let mut cursor = at;
                while cursor < masked.len() && masked[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let name_start = cursor;
                while cursor < masked.len() && is_ident(masked[cursor]) {
                    cursor += 1;
                }
                if cursor > name_start
                    && let Some(name) = js.get(name_start..cursor)
                {
                    names.push(name.to_owned());
                }
            }
            continue;
        }
        at += 1;
    }
    names.retain(|name| name != "host" && !name.is_empty());
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|name| seen.insert(name.clone()));
    names.truncate(48);
    let mut object = String::from("{");
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            object.push(',');
        }
        object.push_str(&json_string(name));
        // A `catch` makes a not-yet-initialised (TDZ) or out-of-scope name `undefined` instead of
        // throwing in the hook itself.
        object.push_str(":(()=>{try{return ");
        object.push_str(name);
        object.push_str("}catch(e){return undefined}})()");
    }
    object.push('}');
    object
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// Blanks strings, comments and regular-expression literals (keeping newlines), so structure
/// scanning never counts a brace in text.
fn mask_js(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        let end = to.min(out.len());
        for byte in out.iter_mut().take(end).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let end = bytes[at..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |offset| at + offset);
                blank(&mut out, at, end);
                at = end;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let mut end = at + 2;
                while end + 1 < bytes.len() && !(bytes[end] == b'*' && bytes[end + 1] == b'/') {
                    end += 1;
                }
                end = (end + 2).min(bytes.len());
                blank(&mut out, at, end);
                at = end;
            }
            b'"' | b'\'' => {
                let quote = bytes[at];
                let mut end = at + 1;
                while end < bytes.len() {
                    if bytes[end] == b'\\' {
                        end += 2;
                        continue;
                    }
                    if bytes[end] == quote {
                        end += 1;
                        break;
                    }
                    end += 1;
                }
                blank(&mut out, at, end);
                at = end.min(bytes.len());
            }
            b'`' => {
                let mut end = at + 1;
                while end < bytes.len() {
                    if bytes[end] == b'\\' {
                        end += 2;
                        continue;
                    }
                    if bytes[end] == b'`' {
                        end += 1;
                        break;
                    }
                    end += 1;
                }
                blank(&mut out, at, end);
                at = end.min(bytes.len());
            }
            b'/' if regex_allowed(bytes, at) => {
                let mut end = at + 1;
                let mut in_class = false;
                while end < bytes.len() {
                    match bytes[end] {
                        b'\\' => end += 2,
                        b'[' => {
                            in_class = true;
                            end += 1;
                        }
                        b']' => {
                            in_class = false;
                            end += 1;
                        }
                        b'/' if !in_class => {
                            end += 1;
                            break;
                        }
                        b'\n' => break,
                        _ => end += 1,
                    }
                }
                blank(&mut out, at, end);
                at = end.min(bytes.len());
            }
            _ => at += 1,
        }
    }
    out
}

fn regex_allowed(bytes: &[u8], at: usize) -> bool {
    let mut index = at;
    while index > 0 {
        index -= 1;
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            continue;
        }
        return matches!(
            byte,
            b'(' | b'['
                | b'{'
                | b','
                | b';'
                | b':'
                | b'='
                | b'!'
                | b'&'
                | b'|'
                | b'?'
                | b'+'
                | b'-'
                | b'*'
                | b'%'
                | b'^'
                | b'~'
                | b'<'
                | b'>'
        );
    }
    true
}

/// The lines after the first that begin a statement are found in [`statement_hook_offsets`].
#[cfg(test)]
mod tests {
    use super::instrument;

    const SCRIPT: &str = r#"export function on_validate(doc) {
    const customer = host.get_doc({"doctype": "Customer", "id": "c-1"});
    const total = doc.total;
    return {"total": total, "customer": customer};
}
"#;

    #[test]
    fn every_statement_line_gets_one_hook() {
        let result = instrument(SCRIPT);
        assert_eq!(result.hook_lines, vec![1, 2, 3, 4]);
        assert_eq!(result.source.matches("host.__dap_line(").count(), 4);
        assert_eq!(result.framed_functions, vec!["on_validate"]);
        assert!(result.source.contains("host.__dap_enter("));
        assert!(result.source.contains("host.__dap_exit(null);"));
        assert_eq!(
            SCRIPT.lines().count(),
            result.source.lines().count(),
            "instrumentation never adds a line"
        );
    }

    #[test]
    fn the_hook_carries_the_parameters_and_the_declarations_before_it() {
        let result = instrument(SCRIPT);
        let second = result
            .source
            .lines()
            .find(|line| line.contains("host.__dap_line({\"line\":2"))
            .expect("line 2 hook");
        assert!(second.contains("\"doc\":"), "{second}");
        let third = result
            .source
            .lines()
            .find(|line| line.contains("host.__dap_line({\"line\":3"))
            .expect("line 3 hook");
        assert!(third.contains("\"doc\":"), "{third}");
        assert!(third.contains("\"customer\":"), "{third}");
        assert!(!third.contains("\"total\":"), "not yet declared: {third}");
    }

    #[test]
    fn object_literals_and_continuations_are_not_hooked() {
        let source = "const x = {\n    \"a\": 1,\n    \"b\": [2, 3]\n};\nconst y = call(\n    x,\n    4\n);\n";
        let result = instrument(source);
        assert_eq!(result.hook_lines, vec![1, 5]);
        assert_eq!(result.source.matches("host.__dap_line(").count(), 2);
    }

    #[test]
    fn strings_comments_and_regex_literals_do_not_confuse_the_scanner() {
        let source = "const a = \"}\"; // }\nconst b = /[}]/;\nconst c = 1;\n";
        let result = instrument(source);
        assert_eq!(result.hook_lines, vec![1, 2, 3]);
        assert_eq!(result.source.matches("host.__dap_line(").count(), 3);
    }

    #[test]
    fn a_nested_function_is_framed_inside_its_parent() {
        let source = "function outer(a) {\n    function inner(b) {\n        return a + b;\n    }\n    return inner(1);\n}\n";
        let result = instrument(source);
        assert_eq!(result.framed_functions, vec!["outer", "inner"]);
        assert_eq!(result.source.matches("host.__dap_enter(").count(), 2);
        assert_eq!(result.source.matches("host.__dap_exit(null);").count(), 2);
        assert!(result.hook_lines.contains(&5));
    }
}
