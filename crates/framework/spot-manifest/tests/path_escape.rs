//! Property test for the path rule (ADR-17 / ADR-19): no path the manifest accepts can
//! escape the app directory when it is joined onto it — on Linux or Windows semantics.

use proptest::prelude::*;
use spot_manifest::FilePath;

/// Arbitrary strings built from the pieces a hostile path is made of: separators, dots,
/// dot-dot, drive letters, and a few safe segments.
fn path_pieces() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("..".to_owned()),
        Just(".".to_owned()),
        Just("".to_owned()),
        Just("C:".to_owned()),
        "[a-zA-Z0-9_-]{0,6}",
    ];
    // Join up to 8 pieces with any of `/`, `\\`, or nothing between.
    (
        prop::collection::vec(piece, 1..8),
        prop::sample::select(vec!["/", "\\", "", "//"]),
    )
        .prop_map(|(pieces, sep)| pieces.join(sep))
}

proptest! {
    /// Whatever `FilePath::parse` accepts, joining it under a root keeps the result inside
    /// the root: no `..` survives canonicalisation-by-hand, no absolute prefix takes over,
    /// and no `\\` or `:` smuggles a Windows path out.
    #[test]
    fn accepted_paths_cannot_escape(raw in path_pieces()) {
        let Ok(path) = FilePath::parse(&raw) else { return Ok(()) };
        let text = path.as_str();

        // The joined path, normalised the way any of our hosts resolves it: split on both
        // separators, apply `.`/`..` by hand. If a `..` could climb out, depth goes < 0.
        let mut depth = 0usize;
        for segment in text.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => depth = depth.saturating_sub(1),
                _ => depth += 1,
            }
        }
        prop_assert!(depth > 0, "path {text:?} resolves to the root itself");
        prop_assert!(!raw.starts_with('/'), "absolute on Linux: {raw:?}");
        prop_assert!(!raw.contains(':'), "absolute on Windows: {raw:?}");
        prop_assert!(!raw.contains('\\'), "native separator: {raw:?}");

        // Joining must be plain containment: root + "/" + path, and `..` never in segments.
        let joined = format!("/srv/apps/erp_selling/{text}");
        prop_assert!(!joined.contains("/../"), "{raw:?} escapes via ..");
    }
}
