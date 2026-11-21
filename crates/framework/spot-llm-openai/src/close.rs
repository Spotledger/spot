//! Partial-JSON repair for the tool-call arguments local models emit — opt-in per profile.
//!
//! The profile's `tool_args_may_be_partial_json` says the endpoint's models may cut a
//! tool-call arguments string short (Ollama and other small local models do). [`close_arguments`]
//! closes what the wire left open — bracket depth tracked by a hand-rolled scanner, no
//! float parsing anywhere — and returns the parsed JSON value, or `None` when the text does
//! not close into something parseable (the caller then raises `llm.protocol`).

/// Closes `text` — a tool-call argument string cut short mid-object — into complete JSON
/// and returns the parsed value.
///
/// What it does: tracks string and escape state character-wise, remembers the open `{` /
/// `[` stack, drops whitespace outside strings, closes a string literal left open at the
/// cut, trims a dangling trailing `,` or `:`, strips a dangling complete key (a string
/// that closed on the wire but whose `:` or value never came), and appends the missing
/// closers in reverse. An already-complete arguments string (the common case: the model
/// closed its object and the endpoint merely cut at a chunk boundary) passes through and
/// parses unchanged. Returns `None` when the repaired text still is not JSON — a string
/// cut mid-word closing into a keyless object (`{"a`), a truncated literal (`false` cut
/// to `fals`) — which the caller reports as `llm.protocol`.
pub fn close_arguments(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // An argument-less call.
        return Some(serde_json::Value::Object(serde_json::Map::new()));
    }
    let mut out = String::with_capacity(trimmed.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    // The byte index of the opening quote of the string being scanned, and the spans
    // `(open, end)` of the strings that closed on the wire, latest last.
    let mut string_open: Option<usize> = None;
    let mut strings: Vec<(usize, usize)> = Vec::new();
    for ch in trimmed.chars() {
        match ch {
            '\\' if in_string => {
                escaped = !escaped;
                out.push(ch);
            }
            '"' if !escaped => {
                if in_string {
                    // The string closes: record its span (the closing quote is pushed
                    // right after, so the end is one past the current length).
                    let open = string_open.take().unwrap_or_default();
                    strings.push((open, out.len() + 1));
                } else {
                    string_open = Some(out.len());
                }
                in_string = !in_string;
                escaped = false;
                out.push(ch);
            }
            '{' | '[' if !in_string => {
                stack.push(closing_of(ch));
                out.push(ch);
            }
            '}' | ']' if !in_string => {
                stack.pop();
                out.push(ch);
            }
            _ if !in_string && ch.is_whitespace() => {
                // Whitespace between tokens is formatting, not content; dropped.
            }
            _ => {
                escaped = false;
                out.push(ch);
            }
        }
    }
    if in_string {
        // The cut fell inside a string literal; close it. That string never completed on
        // the wire, so it is not a candidate for the dangling-key strip.
        strings.clear();
        out.push('"');
    }
    // A dangling trailing separator (a `key:1,` or `key:` shape) cannot head the closer
    // append; a dangling complete key (the `:` or the `,` before its value was cut)
    // comes off with its separator, until nothing dangles.
    pop_separators(&mut out);
    while strip_dangling_key(&mut out, &mut strings) {
        pop_separators(&mut out);
    }
    // Append every missing closer, in reverse. The canonical single-object cut (a `key:1`
    // shape) closes to the complete object; an already-complete string parses with an
    // empty appended set.
    for closer in stack.into_iter().rev() {
        out.push(closer);
    }
    serde_json::from_str(&out).ok()
}

/// Pops trailing out-of-string separators (`,` and `:`) from `out`.
fn pop_separators(out: &mut String) {
    while out.ends_with(',') || out.ends_with(':') {
        out.pop();
    }
}

/// Whether `out` ends with a wire-completed string that is a dangling key — its bytes run
/// to the end of `out` and everything before them ends at `{` or `,` (after trimming) —
/// and strips it when so. Returns whether it stripped; the caller re-runs the separator
/// pop. `strings` holds the spans of the wire-completed strings, latest last; a string
/// with further bytes after it is a value, not a dangling key, and ends the walk.
fn strip_dangling_key(out: &mut String, strings: &mut Vec<(usize, usize)>) -> bool {
    let Some((start, end)) = strings.pop() else {
        return false;
    };
    if end != out.len() {
        // Bytes follow the string: it is a value (or a key whose separator went), not a
        // dangling key. The walk stops; earlier strings are buried below it.
        strings.clear();
        return false;
    }
    let before = out[..start].trim_end().to_owned();
    if !(before.ends_with('{') || before.ends_with(',')) {
        strings.clear();
        return false;
    }
    out.truncate(start);
    true
}

/// The closer a bracket opener expects.
fn closing_of(open: char) -> char {
    if open == '{' { '}' } else { ']' }
}
