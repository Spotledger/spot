//! The `Origin` rule of the MCP endpoint (contract `mcp.md` section 4): DNS-rebinding protection.
//!
//! A request without an `Origin` header (a non-browser agent) is not affected. A request that
//! carries one is allowed only when it is the site's own origin (the host the request was served
//! for) or one the operator listed in `services.mcp.allowed_origins`. `null`, a malformed value
//! and every other origin are refused.

/// Splits `scheme://authority` (no path, query or fragment). `None` for anything else.
fn authority_of(origin: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = origin.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    Some((scheme, rest))
}

/// Whether `value` is a well-formed origin an operator may list: `http(s)://host[:port]`.
#[must_use]
pub fn is_valid_origin(value: &str) -> bool {
    value.len() <= 255 && authority_of(value).is_some()
}

/// Whether a request carrying `origin` may reach the endpoint.
///
/// `host` is the effective host the request was served for (`host[:port]`, as received);
/// `allowed` is the operator's list. Comparison ignores ASCII case and a trailing dot.
#[must_use]
pub fn allowed(origin: &str, host: Option<&str>, allowed: &[String]) -> bool {
    let Some((_, authority)) = authority_of(origin.trim()) else {
        return false;
    };
    let norm = |s: &str| s.trim_end_matches('.').to_ascii_lowercase();
    let authority = norm(authority);
    if host.is_some_and(|h| norm(h) == authority) {
        return true;
    }
    allowed.iter().any(|entry| {
        authority_of(entry.trim()).is_some_and(|(_, a)| norm(a) == authority)
            && entry_scheme_matches(entry.trim(), origin.trim())
    })
}

fn entry_scheme_matches(entry: &str, origin: &str) -> bool {
    match (authority_of(entry), authority_of(origin)) {
        (Some((a, _)), Some((b, _))) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_site_s_own_origin_is_allowed_and_a_foreign_one_is_not() {
        assert!(allowed("https://acme.example", Some("acme.example"), &[]));
        assert!(allowed(
            "http://ACME.example:8080",
            Some("acme.example:8080"),
            &[]
        ));
        assert!(!allowed("https://evil.example", Some("acme.example"), &[]));
        assert!(!allowed(
            "https://acme.example:9",
            Some("acme.example"),
            &[]
        ));
    }

    #[test]
    fn null_malformed_and_path_carrying_origins_are_refused() {
        for bad in [
            "null",
            "",
            "acme.example",
            "ftp://acme.example",
            "https://a.example/x",
            "https://u@a.example",
        ] {
            assert!(
                !allowed(bad, Some("acme.example"), &["https://a.example".into()]),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_listed_origin_is_allowed_with_its_scheme() {
        let list = vec!["https://app.example".to_owned()];
        assert!(allowed("https://app.example", Some("acme.example"), &list));
        assert!(!allowed("http://app.example", Some("acme.example"), &list));
        assert!(!allowed("https://app.example", None, &[]));
    }

    #[test]
    fn only_a_bare_origin_may_be_listed() {
        assert!(is_valid_origin("https://app.example:8443"));
        assert!(!is_valid_origin("https://app.example/"));
        assert!(!is_valid_origin("app.example"));
        assert!(!is_valid_origin(&format!("https://{}", "a".repeat(300))));
    }
}
