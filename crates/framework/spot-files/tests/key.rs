//! `FileKey`/`FileKeyPrefix` validation: the accepted/rejected table from the brief,
//! including every Windows reserved device name (ADR-19: Windows is first-class).

use spot_files::error::FileError;
use spot_files::key::{FileKey, FileKeyPrefix, Visibility};

/// Calls [`FileKey::new`], failing the test on an unexpected error (no `unwrap`/`expect`:
/// the workspace denies `unwrap_used`).
#[track_caller]
fn built(visibility: Visibility, rest: &str) -> FileKey {
    match FileKey::new(visibility, rest) {
        Ok(key) => key,
        Err(e) => panic!("{visibility}/{rest} must be accepted, was {e}"),
    }
}

/// Calls [`FileKeyPrefix::new`], failing the test on an unexpected error.
#[track_caller]
fn prefix_built(text: &str) -> FileKeyPrefix {
    match FileKeyPrefix::new(text) {
        Ok(prefix) => prefix,
        Err(e) => panic!("prefix {text:?} must be accepted, was {e}"),
    }
}

/// Asserts `rest` is rejected with [`FileError::InvalidKey`] and a non-empty reason.
#[track_caller]
fn rejected(rest: &str) {
    match FileKey::new(Visibility::Private, rest) {
        Err(FileError::InvalidKey { reason }) => {
            assert!(
                !reason.is_empty(),
                "the rejection of {rest:?} must carry a reason"
            );
        }
        other => panic!("{rest:?} must be rejected, got {other:?}"),
    }
}

#[test]
fn accepts_well_formed_keys() {
    for (visibility, rest) in [
        (Visibility::Private, "2026/09/report.pdf"),
        (Visibility::Public, "logos/acme.png"),
        (Visibility::Private, "a"),
        (Visibility::Private, "a_1-b/0.9/dots.in.middle"),
    ] {
        let key = built(visibility, rest);
        assert_eq!(
            key.as_str(),
            format!("{}/{rest}", visibility.as_str()),
            "the full text is visibility/rest"
        );
        assert_eq!(key.visibility(), visibility);
        assert_eq!(key.rest(), rest);
        let (parsed_visibility, parsed_key) = match FileKey::parse(key.as_str()) {
            Ok(pair) => pair,
            Err(e) => panic!("parsing {rest:?}: {e}"),
        };
        assert_eq!(parsed_visibility, visibility);
        assert_eq!(parsed_key, key);
    }
}

#[test]
fn accepts_length_boundaries() {
    let segment = "a".repeat(255);
    // Exactly 255 bytes for one segment is the limit.
    built(Visibility::Public, &segment);
    rejected(&"a".repeat(256));
    // The 1024-byte cap counts the full key text (`visibility/` included): a `private/`
    // key's `rest` maxes at 1016 bytes. Build exactly that from full segments —
    // 255 + 1 + 255 + 1 + 255 + 1 + 248 = 1016 — and assert the 1024-byte key is stored.
    let rest = format!("{s}/{s}/{s}/{tail}", s = segment, tail = "a".repeat(248));
    assert_eq!(rest.len(), 1016);
    assert_eq!(built(Visibility::Private, &rest).as_str().len(), 1024);
    // One byte over the cap is rejected, whatever the segment shapes.
    rejected(&format!(
        "{s}/{s}/{s}/{tail}",
        s = segment,
        tail = "a".repeat(249)
    ));
}

#[test]
fn rejects_bad_shapes() {
    for rest in [
        "",              // empty rest
        "/abs",          // leading slash
        "a//b",          // empty middle segment
        ".",             // current directory
        "..",            // parent traversal
        "a/../b",        // traversal
        "a/./b",         // traversal
        ".hidden",       // hidden in the first segment
        "dir/.name",     // hidden deeper
        "trail.",        // trailing dot
        "dir/trail.",    // trailing dot deeper
        "trail ",        // trailing whitespace
        "mid space",     // whitespace anywhere
        "caf\u{e9}.txt", // non-ASCII
        "a/b:c",         // punctuation outside `a-z _ - .`
        "UPPER",         // upper case is rejected, never folded
        "Mixed/Case",    // upper case in any segment
        "lower/UP.txt",  // upper case with an extension
        "UPPER/lower",   // upper case in the first rest segment
        "a/CON/b",       // reserved device name, bare, nested
        "a/NuL.txt",     // reserved device name, with extension, any case
    ] {
        rejected(rest);
    }
}

#[test]
fn rejects_every_windows_reserved_name() {
    let mut stems: Vec<String> = vec![
        "CON".to_owned(),
        "PRN".to_owned(),
        "AUX".to_owned(),
        "NUL".to_owned(),
    ];
    for index in 1..10 {
        stems.push(format!("COM{index}"));
        stems.push(format!("LPT{index}"));
    }
    assert_eq!(
        stems.len(),
        4 + 9 + 9,
        "the brief names 22 reserved device names"
    );
    for stem in &stems {
        let lower = stem.to_ascii_lowercase();
        // Bare and `.`-suffixed forms, in the exact and the lowercase spelling: reserved
        // names are rejected with or without an extension, in any case. (The uppercase
        // spellings are, additionally, dead on arrival under the lower-case-only charset.)
        let forms = [
            stem.clone(),
            format!("{stem}.txt"),
            lower.clone(),
            format!("{lower}.txt"),
        ];
        for rest in forms {
            rejected(&rest);
        }
    }
    // Nested in the middle as well, with separators on both sides.
    rejected("a/CON.txt/b");
}

#[test]
fn prefix_rules() {
    assert!(
        FileKeyPrefix::new("").is_ok(),
        "an empty prefix is the `all` prefix"
    );
    assert_eq!(FileKeyPrefix::all().as_str(), "");
    assert_eq!(prefix_built("contract/2026").as_str(), "contract/2026");
    for text in [".hidden", "CON", "trail.", "a/../b", " "] {
        assert!(
            FileKeyPrefix::new(text).is_err(),
            "prefix {text:?} must be rejected"
        );
    }

    let key = built(Visibility::Private, "2026/09/x");
    assert!(key.starts_with(&prefix_built("private")));
    assert!(key.starts_with(&prefix_built("private/2026")));
    let exact = prefix_built("private/2026/09/x");
    assert!(key.starts_with(&exact));
    assert!(key.starts_with(&FileKeyPrefix::all()));
    // `2026` must not match `20269`.
    assert!(!key.starts_with(&prefix_built("private/20269")));
    // A prefix on the other visibility never matches.
    assert!(!key.starts_with(&prefix_built("public/2026")));
}
