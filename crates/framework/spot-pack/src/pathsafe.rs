//! The path rules an app-relative name must pass, exactly the bundle's own rules.
//!
//! Kept local so this crate states its guarantee without depending on a `pub(crate)` item:
//! a name that escapes here would fail in [`spot_bundle::Bundle::open`] anyway, but the
//! packer refuses it with its own code and its own test, before any bytes are written.

/// Whether `rel` is a plain, relative, `/`-separated path that cannot escape the app
/// directory: not absolute, no backslash, no colon, no empty, `.` or `..` segment, and valid
/// UTF-8 (the input is a `String`, so the last one holds).
#[must_use]
pub fn is_safe_rel_path(rel: &str) -> bool {
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') {
        return false;
    }
    if rel.contains('\\') || rel.contains(':') || rel.contains('\0') {
        return false;
    }
    if rel
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_pass() {
        assert!(is_safe_rel_path("manifest.toml"));
        assert!(is_safe_rel_path("doctypes/party.toml"));
        assert!(is_safe_rel_path("assets/dir/x.css"));
    }

    #[test]
    fn escapes_fail() {
        assert!(!is_safe_rel_path("../steal.toml"));
        assert!(!is_safe_rel_path("assets/../../host.json"));
        assert!(!is_safe_rel_path("assets/./x"));
        assert!(!is_safe_rel_path("assets//x"));
        assert!(!is_safe_rel_path("assets/trailing/"));
        assert!(!is_safe_rel_path("/abs"));
        assert!(!is_safe_rel_path("C:\\x"));
        assert!(!is_safe_rel_path("a\\b"));
        assert!(!is_safe_rel_path("has:colon"));
        assert!(!is_safe_rel_path(""));
    }
}
