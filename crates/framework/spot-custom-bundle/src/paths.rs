//! The path safety net every entry name passes before anything else looks at it.
//!
//! [`normalise`] is **total**: it returns an error for every name that could escape the
//! bundle root or misbehave on another filesystem, so the rest of the crate can trust the
//! names it hands out. The property tests throw arbitrary names at it and assert nothing
//! panics and nothing escapes.

/// A name that has passed path safety: a plain relative path with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SafeName(String);

impl SafeName {
    /// The name, with `/` separators.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a name does not pass path safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathError {
    /// The name is empty, or holds only `.` and `/` segments.
    Empty,
    /// A `..` segment, a leading `/`, a drive letter, a backslash, or a NUL.
    NotRelative(String),
    /// The name is not valid UTF-8.
    NotUtf8,
}

/// Validates an entry name and normalises it: `.` segments collapse, `//` collapses,
/// everything that could escape the bundle root is an error.
///
/// Normalised: `sections/meta/doctype.toml` stays as it is; `sections/./x` becomes `sections/x`;
/// `sections//x` becomes `sections/x`; `sections/x/` becomes `sections/x` (directory entries are
/// stored without their trailing slash). Rejected: `..` segments, absolute paths (leading
/// `/`), backslashes, drive letters (`C:`), NUL, non-UTF-8, and names empty after
/// normalisation.
pub(crate) fn normalise(raw: &[u8]) -> Result<SafeName, PathError> {
    let raw = std::str::from_utf8(raw).map_err(|_| PathError::NotUtf8)?;
    if raw.contains('\\') {
        return Err(PathError::NotRelative(
            "it must use `/`, not `\\`".to_owned(),
        ));
    }
    if raw.contains('\0') {
        return Err(PathError::NotRelative("it contains a NUL".to_owned()));
    }
    if raw.starts_with('/') {
        return Err(PathError::NotRelative(
            "it must not be absolute (it starts with `/`)".to_owned(),
        ));
    }
    // Windows treats a leading `<letter>:` as a drive and a colon anywhere as an alternate
    // data stream; the manifest forbids colons for the same reason. Any colon means the
    // name is not a plain relative path.
    if raw.contains(':') {
        return Err(PathError::NotRelative(
            "a colon looks absolute on Windows, so it is not allowed".to_owned(),
        ));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in raw.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(PathError::NotRelative(
                    "a `..` segment would escape the bundle".to_owned(),
                ));
            }
            other => segments.push(other),
        }
    }
    let name = segments.join("/");
    if name.is_empty() {
        return Err(PathError::Empty);
    }
    Ok(SafeName(name))
}

/// The case-folded form of a normalised name, for the duplicate check: names that differ
/// only by ASCII case normalise to the same key (they would collide on Windows).
pub(crate) fn case_key(name: &SafeName) -> String {
    name.as_str().to_ascii_lowercase()
}

/// Turns a raw name into a plain token usable as one path segment: every character that is
/// not an ASCII letter, digit, `_` or `-` becomes `_`. Used for file names built from a
/// DocType, a layer scope or a script name.
pub(crate) fn sanitize(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Unwrap in tests without tripping the workspace `unwrap_used` lint.
    fn ok(raw: &str) -> SafeName {
        match normalise(raw.as_bytes()) {
            Ok(name) => name,
            Err(fault) => panic!("{raw:?} must normalise, got {fault:?}"),
        }
    }

    /// Asserts a name is rejected.
    fn rejected(raw: &str) {
        if let Ok(name) = normalise(raw.as_bytes()) {
            panic!("{raw:?} must be rejected, got {:?}", name.as_str());
        }
    }

    /// Arbitrary strings built from the pieces a hostile path is made of: separators, dots,
    /// dot-dot, drive letters, and a few safe segments.
    fn raw_names() -> impl proptest::strategy::Strategy<Value = String> {
        let piece = prop_oneof![
            Just("..".to_owned()),
            Just(".".to_owned()),
            Just("".to_owned()),
            Just("C:".to_owned()),
            "[a-zA-Z0-9_ -]{0,6}",
        ];
        (
            proptest::collection::vec(piece, 0..8),
            prop::sample::select(vec!["/", "\\", "", "//"]),
        )
            .prop_map(|(pieces, join)| {
                if pieces.is_empty() {
                    join.to_owned()
                } else {
                    pieces.join(join)
                }
            })
    }

    #[test]
    fn plain_names_stay() {
        assert_eq!(ok("bundle.toml").as_str(), "bundle.toml");
        assert_eq!(ok("digest").as_str(), "digest");
        assert_eq!(
            ok("sections/meta/doctype.toml").as_str(),
            "sections/meta/doctype.toml"
        );
        assert_eq!(ok("a/b/c.txt").as_str(), "a/b/c.txt");
    }

    #[test]
    fn dot_and_empty_segments_collapse() {
        assert_eq!(ok("sections/./x").as_str(), "sections/x");
        assert_eq!(ok("sections//x").as_str(), "sections/x");
        assert_eq!(ok("sections/").as_str(), "sections");
        assert_eq!(ok("./sections/x").as_str(), "sections/x");
        assert_eq!(ok("sections/x/").as_str(), "sections/x");
    }

    #[test]
    fn escapes_are_errors() {
        rejected("../secret.toml");
        rejected("sections/../../etc/passwd");
        rejected("/absolute/path");
        rejected("sections\\x.toml");
        rejected("\0");
        rejected("a\0b");
        rejected("");
        rejected(".");
        rejected("./");
        rejected("a/..");
    }

    #[test]
    fn drive_letters_reject() {
        rejected("C:bundle.toml");
        rejected("C:/bundle.toml");
        rejected("c:x");
        rejected("Z:/a/b");
        // Any colon, not only drive letters.
        rejected("a:b/c");
    }

    #[test]
    fn non_utf8_rejects() {
        assert!(normalise(&[0x68, 0xC3, 0x28, 0x2E, 0x74, 0x6F, 0x6D, 0x6C]).is_err());
    }

    #[test]
    fn sanitize_keeps_plain_tokens() {
        assert_eq!(sanitize("sales_invoice"), "sales_invoice");
        assert_eq!(sanitize("a/b"), "a_b");
        assert_eq!(sanitize("weird.name"), "weird_name");
    }

    proptest! {
        /// Arbitrary names never panic; a normalised name is a plain relative path: no
        /// empty, `.` or `..` segments, no leading `/`, no backslash, no NUL, no colon.
        #[test]
        fn normalisation_never_panics_or_escapes(raw in raw_names()) {
            if let Ok(name) = normalise(raw.as_bytes()) {
                let name = name.as_str();
                prop_assert!(!name.is_empty());
                prop_assert!(!name.starts_with('/'));
                prop_assert!(!name.contains('\\'));
                prop_assert!(!name.contains('\0'));
                prop_assert!(!name.contains(':'));
                prop_assert!(
                    !name.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
                );
            }
        }
    }
}
