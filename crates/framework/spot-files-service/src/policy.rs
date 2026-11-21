//! The site's file policy: the size limit, the allowed and denied content types, which types
//! are always served as attachments, whether inline display is allowed at all, whether guests
//! may read a public file, and how a display name is sanitised.
//!
//! The policy loads from TOML (the site settings), so an administrator can narrow what a site
//! accepts without a code change. The defaults are deliberately strict about *display*: even
//! when inline display is turned on, HTML, SVG, scripts and XML are always sent as
//! attachments, so active content never renders in the caller's origin.

use std::fmt;

/// The media types that are always served as attachments, whatever the policy says: HTML,
/// SVG, scripts and XML can execute or embed active content in a browser.
pub const DEFAULT_ACTIVE_TYPES: [&str; 8] = [
    "text/html",
    "application/xhtml+xml",
    "image/svg+xml",
    "application/javascript",
    "text/javascript",
    "application/xml",
    "text/xml",
    "application/x-shockwave-flash",
];

/// The default upload limit when a site sets none: 25 MiB.
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// The longest display name in characters (the contract's `name.maxLength`).
pub const DEFAULT_MAX_NAME_CHARS: usize = 255;

/// How a download presents its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The browser may render it in place.
    Inline,
    /// The browser must save it; active content never renders.
    Attachment,
}

impl Disposition {
    /// The header value's first token.
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Inline => "inline",
            Disposition::Attachment => "attachment",
        }
    }
}

/// A site's file settings.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilePolicy {
    /// The hard upload limit, enforced while streaming.
    pub max_upload_bytes: u64,
    /// Media type patterns an upload may declare; empty means every type is allowed unless
    /// denied. Patterns may use `*` as the whole subtype (`image/*`, `*/*`).
    pub allowed_types: Vec<String>,
    /// Media type patterns an upload may never declare.
    pub denied_types: Vec<String>,
    /// Media types always served as attachments, whatever `allow_inline` says.
    pub always_attachment: Vec<String>,
    /// Whether a type that is not always-attachment may be served inline.
    pub allow_inline: bool,
    /// Whether a guest may read a public (`private = false`) file.
    pub allow_guest_public: bool,
    /// The longest display name, in characters.
    pub max_name_chars: usize,
}

impl Default for FilePolicy {
    fn default() -> Self {
        FilePolicy {
            max_upload_bytes: DEFAULT_MAX_UPLOAD_BYTES,
            allowed_types: Vec::new(),
            denied_types: Vec::new(),
            always_attachment: DEFAULT_ACTIVE_TYPES
                .iter()
                .map(|t| (*t).to_owned())
                .collect(),
            allow_inline: false,
            allow_guest_public: false,
            max_name_chars: DEFAULT_MAX_NAME_CHARS,
        }
    }
}

impl FilePolicy {
    /// Parses a policy from its TOML form.
    ///
    /// # Errors
    ///
    /// A TOML syntax error or an unknown key.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Whether an upload may declare `content_type`.
    pub fn type_allowed(&self, content_type: &str) -> bool {
        let content_type = normalise(content_type);
        if self
            .denied_types
            .iter()
            .any(|pattern| media_type_matches(pattern, &content_type))
        {
            return false;
        }
        self.allowed_types.is_empty()
            || self
                .allowed_types
                .iter()
                .any(|pattern| media_type_matches(pattern, &content_type))
    }

    /// Whether `content_type` must be served as an attachment.
    pub fn is_always_attachment(&self, content_type: &str) -> bool {
        let content_type = normalise(content_type);
        self.always_attachment
            .iter()
            .any(|pattern| media_type_matches(pattern, &content_type))
    }

    /// How `content_type` is presented on download.
    pub fn disposition(&self, content_type: &str) -> Disposition {
        if self.is_always_attachment(content_type) || !self.allow_inline {
            Disposition::Attachment
        } else {
            Disposition::Inline
        }
    }

    /// The longest display name, in characters.
    pub fn max_name_chars(&self) -> usize {
        self.max_name_chars
    }
}

/// Normalises a media type: lower-case, without parameters or surrounding space.
pub fn normalise(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase()
}

/// Whether `actual` matches the media type `pattern`; `*` matches any subtype.
pub fn media_type_matches(pattern: &str, actual: &str) -> bool {
    let pattern = normalise(pattern);
    let actual = normalise(actual);
    if pattern == "*/*" {
        return true;
    }
    match pattern.split_once('/') {
        Some((ptype, "*")) => actual
            .split_once('/')
            .is_some_and(|(atype, _)| atype == ptype),
        Some((ptype, psub)) => actual == format!("{ptype}/{psub}"),
        None => false,
    }
}

/// The media type a byte prefix looks like, when it is one of the common types with a known
/// magic number; `None` otherwise.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    const MAGIC: [(&[u8], &str); 10] = [
        (b"%PDF-", "application/pdf"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b", "application/gzip"),
        (b"7z\xBC\xAF\x27\x1C", "application/x-7z-compressed"),
        (b"OggS", "application/ogg"),
        (b"ID3", "audio/mpeg"),
    ];
    for (prefix, kind) in MAGIC {
        if head.starts_with(prefix) {
            return Some(kind);
        }
    }
    if head.starts_with(b"RIFF") && head.len() >= 12 && head[8..].starts_with(b"WEBP") {
        return Some("image/webp");
    }
    if head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    None
}

/// The magic-number check for a declared type: `Ok(())` when the bytes are consistent with it,
/// `Err(sniffed)` naming what they look like otherwise.
///
/// Only the common types with a magic number are checked; a type the table does not know
/// (`text/plain`, `application/octet-stream`, an app's own type) is accepted as declared.
///
/// # Errors
///
/// The media type the bytes look like, when it disagrees with `declared`.
pub fn check_declared(declared: &str, head: &[u8]) -> Result<(), String> {
    let declared = normalise(declared);
    let Some(expected) = expected_magic(&declared) else {
        return Ok(());
    };
    match sniff(head) {
        Some(found) if expected.contains(&found) => Ok(()),
        Some(found) => Err(found.to_owned()),
        None => Err("unknown".to_owned()),
    }
}

/// The sniffed types a declared type accepts, when the declared type has a magic number.
fn expected_magic(declared: &str) -> Option<&'static [&'static str]> {
    const ZIP_LIKE: &[&str] = &["application/zip"];
    match declared {
        "application/pdf" => Some(&["application/pdf"]),
        "image/png" => Some(&["image/png"]),
        "image/jpeg" => Some(&["image/jpeg"]),
        "image/gif" => Some(&["image/gif"]),
        "image/webp" => Some(&["image/webp"]),
        "image/bmp" => Some(&["image/bmp"]),
        "application/zip" => Some(ZIP_LIKE),
        "application/gzip" => Some(&["application/gzip"]),
        "application/x-7z-compressed" => Some(&["application/x-7z-compressed"]),
        "application/ogg" => Some(&["application/ogg"]),
        "audio/mpeg" => Some(&["audio/mpeg"]),
        _ => {
            // The ZIP container types (Office Open XML and OpenDocument) share the zip magic.
            if declared.starts_with("application/vnd.openxmlformats-officedocument.")
                || declared.starts_with("application/vnd.oasis.opendocument.")
            {
                Some(ZIP_LIKE)
            } else {
                None
            }
        }
    }
}

/// The display name for a client-supplied file name: the base name only (never a path), with
/// control characters removed, trimmed and cut to `max_chars` characters. An empty result
/// becomes `file`. The name is used for display and `Content-Disposition` only; a storage key
/// is never derived from it.
pub fn sanitise_display_name(raw: &str, max_chars: usize) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .collect();
    let trimmed = cleaned.trim();
    let cut: String = trimmed.chars().take(max_chars).collect();
    if cut.is_empty() {
        "file".to_owned()
    } else {
        cut
    }
}

/// The `Content-Disposition` header for a download: `inline`/`attachment`, an ASCII fallback
/// `filename`, and the RFC 5987 `filename*` form for a non-ASCII name. Quotes, backslashes and
/// control characters never reach the header verbatim.
pub fn content_disposition(disposition: Disposition, name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        disposition.as_str(),
        fallback,
        percent_encode(name)
    )
}

/// RFC 5987 percent-encoding: everything outside the `attr-char` set becomes `%XX`.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        let safe = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
            );
        if safe {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// Upper-case hex digits for percent-encoding.
const HEX: [u8; 16] = *b"0123456789ABCDEF";

impl fmt::Display for Disposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Disposition, FilePolicy, check_declared, content_disposition, media_type_matches,
        normalise, sanitise_display_name, sniff,
    };

    #[test]
    fn defaults_are_strict_about_active_content() {
        let policy = FilePolicy::default();
        assert_eq!(policy.max_upload_bytes, super::DEFAULT_MAX_UPLOAD_BYTES);
        assert!(!policy.allow_inline);
        assert!(!policy.allow_guest_public);
        for active in [
            "text/html",
            "image/svg+xml",
            "application/javascript",
            "application/xml",
        ] {
            assert!(policy.is_always_attachment(active), "{active}");
            assert_eq!(policy.disposition(active), Disposition::Attachment);
        }
        assert!(policy.type_allowed("application/pdf"));
    }

    #[test]
    fn policy_loads_from_toml_and_narrows_types() {
        let policy = FilePolicy::from_toml(
            r#"
            max_upload_bytes = 1024
            allowed_types = ["image/*", "application/pdf"]
            denied_types = ["image/svg+xml"]
            allow_inline = true
            allow_guest_public = true
            "#,
        )
        .unwrap_or_else(|e| panic!("policy: {e:?}"));
        assert_eq!(policy.max_upload_bytes, 1024);
        assert!(policy.type_allowed("image/png"));
        assert!(policy.type_allowed("application/pdf"));
        assert!(!policy.type_allowed("image/svg+xml"));
        assert!(!policy.type_allowed("text/plain"));
        assert!(policy.allow_guest_public);
        // Inline is allowed for a type that is not always-attachment.
        assert_eq!(policy.disposition("image/png"), Disposition::Inline);
        assert_eq!(policy.disposition("text/html"), Disposition::Attachment);
    }

    #[test]
    fn unknown_policy_keys_are_rejected() {
        assert!(FilePolicy::from_toml("surprise = 1").is_err());
    }

    #[test]
    fn media_type_matching_handles_wildcards() {
        assert!(media_type_matches("*/*", "image/png"));
        assert!(media_type_matches("image/*", "image/png"));
        assert!(!media_type_matches("image/*", "text/plain"));
        assert!(media_type_matches(
            "application/pdf",
            "application/pdf; charset=binary"
        ));
        assert!(!media_type_matches("application/pdf", "application/zip"));
        assert_eq!(normalise(" Text/HTML ; charset=utf-8"), "text/html");
    }

    #[test]
    fn sniffing_and_declared_checks() {
        assert_eq!(sniff(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n\x00"), Some("image/png"));
        assert_eq!(sniff(b"PK\x03\x04rest"), Some("application/zip"));
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"plain text"), None);

        assert!(check_declared("application/pdf", b"%PDF-1.7").is_ok());
        assert!(check_declared("application/pdf", b"\x89PNG\r\n\x1a\n").is_err());
        assert!(check_declared("image/png", b"not a png").is_err());
        // Unknown declared types are not checked.
        assert!(check_declared("text/plain", b"anything").is_ok());
        // Office documents share the zip magic.
        assert!(
            check_declared(
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                b"PK\x03\x04"
            )
            .is_ok()
        );
    }

    #[test]
    fn display_names_are_sanitised() {
        assert_eq!(sanitise_display_name("/etc/passwd", 255), "passwd");
        assert_eq!(sanitise_display_name("C:\\temp\\a.txt", 255), "a.txt");
        assert_eq!(sanitise_display_name("bad\r\nname", 255), "badname");
        assert_eq!(sanitise_display_name("   ", 255), "file");
        assert_eq!(sanitise_display_name("", 255), "file");
        assert_eq!(sanitise_display_name("abcdef", 3), "abc");
    }

    #[test]
    fn content_disposition_encodes_unsafe_characters() {
        let header = content_disposition(Disposition::Attachment, "spéc \"x\".pdf");
        assert!(header.starts_with("attachment; filename=\"sp_c__x_.pdf\""));
        assert!(header.contains("filename*=UTF-8''sp%C3%A9c%20%22x%22.pdf"));
        assert!(!header.contains('\r'));
        assert!(!header.contains('\n'));
    }
}
