//! Which requests may be written to the host's storage (feature `storage`).
//!
//! The persisted queue must never hold a credential, a token or a one-time lease, so the
//! decision is made by path class and fails closed: anything that cannot be understood is
//! not persisted. The path is normalised before it is judged (query and fragment removed,
//! percent escapes decoded, `\` read as `/`, empty and `.` segments dropped, `..` resolved,
//! case folded, `;parameters` and trailing dots and blanks removed from each segment).
//!
//! Denied classes:
//!
//! * `/api/v1/auth/`: login, logout, session, sessions, password, permissions and anything
//!   added under it (credentials, passwords, tokens);
//! * `/api/v1/resource/{doctype}/{id}/reveal`: a protected field's real value, audited per
//!   call and never replayed;
//! * `/api/v1/events/`: consumer pull, ack and restart carry a lease token and positions that
//!   mean nothing after a restart.
//!
//! A write under `/api/v1/resource/` that carries a body is judged by DocType as well (see
//! [`crate::meta_seen`]): [`target_of`] names the DocType the path addresses, read
//! strictly (each segment decoded once, no `.`, `..`, empty, `;`, `\\` or trailing-dot segment,
//! exact lower-case `api/v1/resource` prefix), and refuses everything else.
//!
//! The request is still sent from memory exactly as before; only the stored copy omits it.

/// How many rounds of percent-decoding are judged. A path that is still encoded after this
/// many rounds is refused.
const MAX_DECODE_ROUNDS: usize = 4;

/// True when a request with this method and path may be written to storage.
pub(crate) fn may_persist(method: &str, path: &str) -> bool {
    if !matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    ) {
        return false;
    }
    allowed(path)
}

fn allowed(path: &str) -> bool {
    // Judge the path as the host sends it and after each further round of decoding, so a
    // double-encoded name is judged both ways; any denied form denies the request.
    let mut text = strip_query(path).to_owned();
    for _ in 0..MAX_DECODE_ROUNDS {
        if !judged_ok(&text) {
            return false;
        }
        match decode(&text) {
            Decoded::Same => return true,
            Decoded::Changed(next) => {
                // A decoded `?` or `#` may start a query or fragment for a server that decodes
                // twice, so the path is judged both with and without what follows it.
                if !judged_ok(&next) {
                    return false;
                }
                text = strip_query(&next).to_owned();
            }
            Decoded::Invalid => return false,
        }
    }
    false
}

fn strip_query(path: &str) -> &str {
    let end = path.find(['?', '#']).unwrap_or(path.len());
    path.get(..end).unwrap_or("")
}

enum Decoded {
    /// No percent escape in the text.
    Same,
    Changed(String),
    /// A malformed escape or bytes that are not UTF-8.
    Invalid,
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

fn decode(text: &str) -> Decoded {
    let bytes = text.as_bytes();
    if !bytes.contains(&b'%') {
        return Decoded::Same;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&byte) = bytes.get(i) {
        if byte == b'%' {
            let pair = bytes.get(i + 1).zip(bytes.get(i + 2));
            let value = pair.and_then(|(h, l)| Some(hex(*h)? * 16 + hex(*l)?));
            match value {
                Some(v) => out.push(v),
                None => return Decoded::Invalid,
            }
            i += 3;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    String::from_utf8(out).map_or(Decoded::Invalid, Decoded::Changed)
}

/// The normalised segments of a path, or `None` when it cannot be normalised.
fn segments(text: &str) -> Option<Vec<String>> {
    if text.chars().any(char::is_control) || !(text.starts_with('/') || text.starts_with('\\')) {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(['/', '\\']) {
        let before_params = raw.split(';').next().unwrap_or("");
        let trimmed = before_params.trim_end_matches(['.', ' ', '\t']);
        match (before_params, trimmed) {
            ("", _) | (".", _) => {}
            ("..", _) => {
                out.pop()?;
            }
            // Only dots or blanks: a name a lenient server could read as a dot segment.
            (_, "") => return None,
            (_, name) => out.push(name.to_lowercase()),
        }
    }
    Some(out)
}

/// True when the (single-decoded) path is acceptable on its own.
fn judged_ok(text: &str) -> bool {
    let Some(parts) = segments(text) else {
        return false;
    };
    let name = |i: usize| parts.get(i).map(String::as_str);
    if name(0) != Some("api") || name(1) != Some("v1") {
        return false;
    }
    let Some(group) = name(2) else {
        return false;
    };
    // A look-alike (non-ASCII) in a fixed position could be read as a denied name by a server
    // that folds characters; it is refused rather than guessed.
    if !group.is_ascii() {
        return false;
    }
    match group {
        "auth" | "events" => false,
        // Index 3 is the DocType and 4 the id, both free text; what follows the id names an
        // action and must be plain ASCII, so no look-alike of `reveal` slips through.
        "resource" => {
            !parts.iter().skip(4).any(|s| s == "reveal")
                && parts.iter().skip(5).all(|s| s.is_ascii())
        }
        _ => true,
    }
}

/// What a write path addresses, for the per-DocType rule.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Nothing under `/api/v1/resource/` in any decoded form of the path.
    Other,
    /// A resource path of this (decoded, case-kept) DocType.
    Resource(String),
    /// A resource path whose DocType cannot be read strictly, or a path that cannot be read.
    Refused,
}

/// True (or false) when some decoded form of the path is under `/api/v1/resource/`; `None`
/// when the path cannot be normalised.
fn under_resource(path: &str) -> Option<bool> {
    let is_resource = |text: &str| -> Option<bool> {
        let parts = segments(text)?;
        let name = |i: usize| parts.get(i).map(String::as_str);
        Some(name(0) == Some("api") && name(1) == Some("v1") && name(2) == Some("resource"))
    };
    let mut text = strip_query(path).to_owned();
    let mut found = false;
    for _ in 0..MAX_DECODE_ROUNDS {
        found |= is_resource(&text)?;
        match decode(&text) {
            Decoded::Same => return Some(found),
            Decoded::Changed(next) => {
                found |= is_resource(&next)?;
                text = strip_query(&next).to_owned();
            }
            Decoded::Invalid => return None,
        }
    }
    None
}

/// The segments of a path, each decoded exactly once, or `None` when any segment is not plain.
fn plain_segments(path: &str) -> Option<Vec<String>> {
    let rest = strip_query(path).strip_prefix('/')?;
    if rest.contains('\\') {
        return None;
    }
    let mut out = Vec::new();
    for raw in rest.split('/') {
        let name = match decode(raw) {
            Decoded::Same => raw.to_owned(),
            Decoded::Changed(text) => text,
            Decoded::Invalid => return None,
        };
        let odd = name.is_empty()
            || name == "."
            || name == ".."
            || name.ends_with(['.', ' ', '\t'])
            || name.contains(['/', '\\', '%', '?', '#', ';'])
            || name.chars().any(char::is_control);
        if odd {
            return None;
        }
        out.push(name);
    }
    Some(out)
}

/// The DocType a write path addresses under `/api/v1/resource/`, judged strictly.
pub(crate) fn target_of(path: &str) -> Target {
    match under_resource(path) {
        None => Target::Refused,
        Some(false) => Target::Other,
        Some(true) => match plain_segments(path).as_deref() {
            Some([api, v1, resource, doctype, ..])
                if api == "api" && v1 == "v1" && resource == "resource" =>
            {
                Target::Resource(doctype.clone())
            }
            _ => Target::Refused,
        },
    }
}

/// The DocType of an exact `/api/v1/meta/{doctype}` path (the query is ignored), else `None`.
pub(crate) fn meta_doctype(path: &str) -> Option<String> {
    match plain_segments(path)?.as_slice() {
        [api, v1, meta, doctype] if api == "api" && v1 == "v1" && meta == "meta" => {
            Some(doctype.clone())
        }
        _ => None,
    }
}

/// True when the request path carries a query or a fragment.
pub(crate) fn has_query(path: &str) -> bool {
    path.contains(['?', '#'])
}
