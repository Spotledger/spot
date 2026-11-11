//! The 'show equivalent command' path: `render` writes the CLI line, `parse` reads it
//! back. The round trip `parse(render(x)) == x` is the contract; the property test
//! proves it.

use crate::model::{ParamType, ParamValue, kebab};
use crate::validated::{AcceptedParam, SECRET_PLACEHOLDER, Validated};

/// The shell a rendered command line targets, deciding the quoting rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    /// A POSIX shell (`sh`, `bash`): single-quoted strings with the `'\''` escape.
    Posix,
    /// Windows PowerShell: single-quoted strings with `'` doubled.
    PowerShell,
}

/// Renders the validated invocation as the equivalent CLI line for `shell`.
///
/// The shape both shells agree on: `spot ops <segments...> <flags...>`, where the
/// operation id's segments become subcommands and each parameter becomes a
/// `--kebab-case` flag. A boolean is a bare `--flag` when true and `--no-flag` when
/// false; a list repeats its flag once per element; parameters ride in declaration
/// order, so the same invocation always renders byte-identically.
///
/// A secret parameter's value is never rendered: the flag is written with the
/// [`SECRET_PLACEHOLDER`] (`--password <prompt>`), and the caller is prompted at
/// execution time. Parsing back such a line yields the placeholder as the value — the
/// value itself was never on the command line to read.
#[must_use]
pub fn render(validated: &Validated, shell: Shell) -> String {
    let argv = argv(validated);
    let mut line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&quote(arg, shell));
    }
    line
}

/// The argument vector of the equivalent command: `spot ops <segments> <flags>`.
///
/// The parser consumes exactly this shape; the property test builds it directly and
/// checks the rendered string is its shell-quoted join, which is what makes
/// `parse(render(x)) == x` hold for awkward values.
#[must_use]
pub fn argv(validated: &Validated) -> Vec<String> {
    let mut out = vec!["spot".to_owned(), "ops".to_owned()];
    out.extend(validated.op.segments().map(str::to_owned));
    for param in &validated.params {
        push_param(&mut out, param);
    }
    out
}

/// One parameter's contribution to the argv, in declaration order.
fn push_param(out: &mut Vec<String>, param: &AcceptedParam) {
    let flag = kebab(&param.name);
    // A secret value never rides the command line: the placeholder stands in, so the
    // printed line is safe to show, log and copy.
    match (&param.ty, &param.value) {
        (ParamType::Bool, ParamValue::Bool(true)) => out.push(format!("--{flag}")),
        (ParamType::Bool, ParamValue::Bool(false)) => out.push(format!("--no-{flag}")),
        (ParamType::List(_), ParamValue::List(items)) => {
            if param.secret {
                // A secret list keeps its element count but not its values.
                for _ in items {
                    out.push(format!("--{flag}"));
                    out.push(SECRET_PLACEHOLDER.to_owned());
                }
            } else {
                for item in items {
                    out.push(format!("--{flag}"));
                    out.push(value_text(item));
                }
            }
        }
        (_, value) => {
            out.push(format!("--{flag}"));
            if param.secret {
                out.push(SECRET_PLACEHOLDER.to_owned());
            } else {
                out.push(value_text(value));
            }
        }
    }
}

/// The plain-text spelling of one value on the command line.
fn value_text(value: &ParamValue) -> String {
    match value {
        ParamValue::Bool(true) => "true".to_owned(),
        ParamValue::Bool(false) => "false".to_owned(),
        ParamValue::Int(number) => number.to_string(),
        ParamValue::Text(text) => text.clone(),
        ParamValue::Duration(seconds) => format!("{seconds}s"),
        ParamValue::List(items) => {
            // A nested list cannot ride a command line; the catalogue refuses the type.
            // Rendering the flattened text keeps the function total.
            items.iter().map(value_text).collect::<Vec<_>>().join(",")
        }
    }
}

/// Quotes one argument for `shell`, so a value with spaces, quotes, `$`, backticks,
/// newlines or non-ASCII survives a real shell as a single argument.
///
/// POSIX: wrap in single quotes, with `'` spelled `'\''` inside. PowerShell: wrap in
/// single quotes, with `'` doubled. An argument that needs no quoting stays bare in both
/// shells; the empty argument is quoted in both.
#[must_use]
pub fn quote(arg: &str, shell: Shell) -> String {
    match shell {
        Shell::Posix => {
            if needs_quoting(arg) {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.to_owned()
            }
        }
        Shell::PowerShell => {
            // PowerShell's bare-token rules are stricter than POSIX: whitespace, quotes,
            // `$`, backtick, comma, semicolon, parentheses, braces, redirects and a few
            // punctuation breaks a bare token, so quote on any of them.
            if needs_quoting(arg) || shell_only_breaks(arg) {
                format!("'{}'", arg.replace('\'', "''"))
            } else {
                arg.to_owned()
            }
        }
    }
}

/// Whether an unquoted token would change meaning in a POSIX shell.
fn needs_quoting(arg: &str) -> bool {
    arg.is_empty()
        || arg.chars().any(|c| {
            matches!(
                c,
                ' ' | '\t'
                    | '\n'
                    | '\r'
                    | '\''
                    | '"'
                    | '\\'
                    | '$'
                    | '`'
                    | '&'
                    | '|'
                    | ';'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '*'
                    | '?'
                    | '['
                    | '#'
                    | '~'
                    | '='
                    | '!'
                    | '{'
                    | '}'
                    | ','
            )
        })
}

/// Characters that break a bare token in PowerShell but not in POSIX. A leading dash is
/// not one of them: a `--name` token is a legal literal argument in PowerShell (a token
/// is a parameter name only when a single dash is followed by a letter), and the
/// equivalent-command goldens require the flags bare in both shells.
fn shell_only_breaks(arg: &str) -> bool {
    arg.chars()
        .any(|c| matches!(c, '@' | '%' | '^' | '+' | '.' | '/' | ':'))
}
