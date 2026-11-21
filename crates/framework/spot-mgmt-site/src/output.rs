//! What a command printed, as capped and redacted JSON. A handler result is at most 64 KiB; this
//! keeps well under it, never carries a connection string, password or key, and says when it
//! cut something off.

use serde_json::{Map, Value, json};

use crate::ports::CliOutput;

/// The most lines kept of one stream.
pub const MAX_LINES: usize = 200;
/// The most characters kept of one line.
pub const MAX_LINE_CHARS: usize = 400;
/// The most characters kept of one stream.
pub const MAX_STREAM_CHARS: usize = 24 * 1024;
/// The most warning lines (standard error) kept.
pub const MAX_WARNINGS: usize = 50;

/// What a result may keep verbatim: the one field a later operation needs (the preflight's
/// single-use token) when the operation is the preflight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Redact every secret-looking field.
    Nothing,
    /// Keep `confirm_token`: the preflight hands it to its caller on purpose.
    ConfirmToken,
}

const SECRET_WORDS: [&str; 6] = [
    "password",
    "secret",
    "token",
    "passphrase",
    "key",
    "credential",
];

/// Redacts a line: credentials inside a URL (`scheme://user:secret@host`) and the value of any
/// `name=value` word whose name looks like a secret.
#[must_use]
pub fn scrub(line: &str, keep: Keep) -> String {
    let words: Vec<String> = line.split(' ').map(|word| scrub_word(word, keep)).collect();
    words.join(" ")
}

fn scrub_word(word: &str, keep: Keep) -> String {
    if let Some((scheme_end, _)) = word.match_indices("://").next() {
        let start = scheme_end + 3;
        let rest = &word[start..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            return format!("{}***@{}", &word[..start], &rest[at + 1..]);
        }
        return word.to_owned();
    }
    let Some((name, value)) = word.split_once('=') else {
        return word.to_owned();
    };
    let lowered = name.trim_start_matches('-').to_ascii_lowercase();
    let secretish = SECRET_WORDS.iter().any(|w| lowered.contains(w));
    let allowed = keep == Keep::ConfirmToken && lowered == "confirm_token";
    if secretish && !allowed && !value.is_empty() {
        format!("{name}=***")
    } else {
        word.to_owned()
    }
}

/// The lines of `text` after redaction and the caps, and whether anything was cut.
#[must_use]
pub fn lines(text: &str, keep: Keep, max_lines: usize) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut used = 0usize;
    let mut cut = false;
    for raw in text.lines() {
        let raw = raw.trim_end();
        if raw.is_empty() {
            continue;
        }
        let mut line = scrub(raw, keep);
        if line.chars().count() > MAX_LINE_CHARS {
            line = line.chars().take(MAX_LINE_CHARS).collect();
            cut = true;
        }
        used += line.chars().count();
        if kept.len() >= max_lines || used > MAX_STREAM_CHARS {
            cut = true;
            break;
        }
        kept.push(line);
    }
    (kept, cut)
}

/// The `name=value` words of the lines: the CLI prints its facts that way. A later line wins.
#[must_use]
pub fn fields(lines: &[String]) -> Map<String, Value> {
    let mut map = Map::new();
    for line in lines {
        for word in line.split(' ') {
            let Some((name, value)) = word.split_once('=') else {
                continue;
            };
            let name_ok = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if name_ok && !value.is_empty() {
                map.insert(name.to_owned(), Value::String(value.to_owned()));
            }
        }
    }
    map
}

/// The backups of `spot backup list`: a header line `target NAME (N backups of site S)` and one
/// line per backup: `ID TAKEN bytes=N verified|unverified encrypted|unencrypted digest=HEX`.
#[must_use]
pub fn backups(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .filter(|line| !line.starts_with("target "))
        .filter_map(|line| {
            let words: Vec<&str> = line.split(' ').collect();
            let (id, taken) = (words.first()?, words.get(1)?);
            let bytes = words
                .iter()
                .find_map(|w| w.strip_prefix("bytes="))
                .and_then(|n| n.parse::<u64>().ok())?;
            let verified = words.contains(&"verified");
            let sealed = words.contains(&"encrypted");
            let digest = words
                .iter()
                .find_map(|w| w.strip_prefix("digest="))
                .unwrap_or_default();
            Some(json!({
                "id": id,
                "taken_at": taken,
                "bytes": bytes,
                "verified": verified,
                "encrypted": sealed,
                "digest": digest,
            }))
        })
        .collect()
}

/// The result of a command: its exit code, its output lines, the facts they carry, warnings from
/// standard error, whether anything was cut, and the equivalent CLI line.
#[must_use]
pub fn result(output: &CliOutput, cli_line: &str, keep: Keep) -> Value {
    let (out, out_cut) = lines(&output.stdout, keep, MAX_LINES);
    let (warnings, warn_cut) = lines(&output.stderr, Keep::Nothing, MAX_WARNINGS);
    json!({
        "exit": output.exit,
        "output": out,
        "fields": Value::Object(fields(&out)),
        "warnings": warnings,
        "truncated": out_cut || warn_cut,
        "cli": cli_line,
    })
}
