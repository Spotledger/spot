//! Where a credential may go: the host of a credentialed request must be written in the node's
//! configuration.
//!
//! A URL template may take parts of itself from Case data (`https://api.test/orders/{input.id}`).
//! If the *host* could come from data, a data value could steer the node's credential to a host
//! the author never named. So when a node carries a credential reference, the rendered request's
//! host must equal, exactly, a host that appears **literally** in the configuration: the host of
//! the `url` template (when its authority holds no placeholder) or an entry of `allowed_hosts`.
//!
//! Hosts are compared after the same normalisation the sender applies (the WHATWG URL parser:
//! lower case, IDNA to ASCII), minus one trailing dot. There is no suffix, substring or
//! wildcard matching, and a request URL carrying userinfo is refused outright.

use std::collections::BTreeSet;

use url::Url;

use crate::error::IntegrationError;

/// The normalised form of a host: as the URL parser writes it, without one trailing dot.
fn normalise(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_owned()
}

/// The normalised host of a parsed URL, when it has one.
fn host_of(url: &Url) -> Option<String> {
    url.host_str().map(normalise)
}

/// The literal host of a URL template, when its authority holds no placeholder.
///
/// The template is read as text: only what comes before the first `/`, `?`, `#` or `\` after
/// `scheme://` is the authority. An authority with a `{` or `}`, with userinfo, or a template
/// whose scheme is not literal `http`/`https` has no literal host.
fn template_host(template: &str) -> Option<String> {
    let lower = template.to_ascii_lowercase();
    let (scheme, rest) = if lower.starts_with("https://") {
        ("https", template.get("https://".len()..)?)
    } else if lower.starts_with("http://") {
        ("http", template.get("http://".len()..)?)
    } else {
        return None;
    };
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let authority = rest.get(..end)?;
    if authority.is_empty() || authority.contains(['{', '}', '@']) {
        return None;
    }
    let parsed = Url::parse(&format!("{scheme}://{authority}/")).ok()?;
    host_of(&parsed)
}

/// A configured `allowed_hosts` entry as a normalised host. An entry is a bare host: no scheme,
/// port, path or userinfo.
fn allowed_host(entry: &str) -> Option<String> {
    if entry.is_empty() || entry.contains(['/', '\\', '@', '?', '#', '{', '}', ' ']) {
        return None;
    }
    let parsed = Url::parse(&format!("https://{entry}/")).ok()?;
    if parsed.port().is_some() || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    host_of(&parsed)
}

/// The hosts the configuration writes down literally: the `url` template's host and every
/// `allowed_hosts` entry.
///
/// # Errors
///
/// [`IntegrationError::ConfigInvalid`] when an `allowed_hosts` entry is not a bare host.
pub(crate) fn literal_hosts(
    node: &str,
    url_template: &str,
    allowed_hosts: &[String],
) -> Result<BTreeSet<String>, IntegrationError> {
    let mut hosts = BTreeSet::new();
    if let Some(host) = template_host(url_template) {
        hosts.insert(host);
    }
    for entry in allowed_hosts {
        let host = allowed_host(entry).ok_or_else(|| IntegrationError::ConfigInvalid {
            node: node.to_owned(),
            detail: String::from(
                "an allowed host must be a bare host name (no scheme, port or path)",
            ),
        })?;
        hosts.insert(host);
    }
    Ok(hosts)
}

/// Refuses unless `rendered_url` has no userinfo and its host is one of the configuration's
/// literal hosts. Nothing here names the host the data steered the request to.
///
/// # Errors
///
/// [`IntegrationError::CredentialHostMismatch`], [`IntegrationError::UrlInvalid`] or
/// [`IntegrationError::ConfigInvalid`].
pub(crate) fn ensure_credential_host(
    node: &str,
    url_template: &str,
    allowed_hosts: &[String],
    rendered_url: &str,
) -> Result<(), IntegrationError> {
    let literal = literal_hosts(node, url_template, allowed_hosts)?;
    let parsed = Url::parse(rendered_url).map_err(|e| IntegrationError::UrlInvalid {
        detail: e.to_string(),
    })?;
    let mismatch = || IntegrationError::CredentialHostMismatch {
        node: node.to_owned(),
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(mismatch());
    }
    match host_of(&parsed) {
        Some(host) if literal.contains(&host) => Ok(()),
        _ => Err(mismatch()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn check(template: &str, allowed: &[&str], rendered: &str) -> Result<(), IntegrationError> {
        let allowed: Vec<String> = allowed.iter().map(|a| (*a).to_owned()).collect();
        ensure_credential_host("n1", template, &allowed, rendered)
    }

    #[test]
    fn a_literal_template_host_passes_in_any_case_and_with_a_trailing_dot() {
        assert!(check("https://api.test/x", &[], "https://api.test/x").is_ok());
        assert!(check("https://API.test/x", &[], "https://api.TEST./x").is_ok());
        assert!(check("https://api.test:8443/x", &[], "https://api.test:9/y").is_ok());
    }

    #[test]
    fn a_host_from_data_is_refused() {
        let template = "https://{input.host}/x";
        assert!(matches!(
            check(template, &[], "https://evil.test/x"),
            Err(IntegrationError::CredentialHostMismatch { .. })
        ));
        // A partly mapped authority has no literal host either.
        assert!(
            check(
                "https://api.{input.r}.test/x",
                &[],
                "https://api.evil.test/x"
            )
            .is_err()
        );
        assert!(check("{input.url}", &[], "https://evil.test/x").is_err());
    }

    #[test]
    fn an_allowed_host_makes_a_mapped_host_pass_only_when_it_equals_one() {
        let template = "https://{input.host}/x";
        assert!(
            check(
                template,
                &["api.test", "eu.api.test"],
                "https://eu.api.test/x"
            )
            .is_ok()
        );
        assert!(check(template, &["api.test"], "https://eu.api.test/x").is_err());
    }

    #[test]
    fn userinfo_and_look_alike_hosts_are_refused() {
        for rendered in [
            "https://api.test@evil.test/x",
            "https://evil.test@api.test/x",
            "https://api.test.evil.test/x",
            "https://evil-api.test/x",
            "https://api.test.evil/x",
            "https://xapi.test/x",
            "https://api.test%2eevil.test/x",
        ] {
            assert!(
                check("https://api.test/x", &[], rendered).is_err(),
                "{rendered} passed"
            );
        }
    }

    #[test]
    fn a_backslash_ends_the_authority_exactly_as_the_sender_reads_it() {
        // The sender parses with the same WHATWG rules: this request goes to api.test, whose path
        // is `/@evil.test/x`, so it is not a steered host.
        assert!(check("https://api.test/x", &[], "https://api.test\\@evil.test/x").is_ok());
    }

    #[test]
    fn no_wildcard_or_suffix_matching() {
        assert!(check("https://x/", &["*.api.test"], "https://a.api.test/").is_err());
        assert!(check("https://x/", &["api.test"], "https://sub.api.test/").is_err());
    }

    #[test]
    fn a_bad_allowed_host_is_a_config_error() {
        for entry in [
            "https://api.test",
            "api.test/x",
            "u@api.test",
            "api.test:99",
            "",
        ] {
            assert!(matches!(
                literal_hosts("n1", "https://x/", &[entry.to_owned()]),
                Err(IntegrationError::ConfigInvalid { .. })
            ));
        }
    }

    #[test]
    fn idna_hosts_compare_in_their_ascii_form() {
        assert!(check("https://bücher.test/", &[], "https://xn--bcher-kva.test/").is_ok());
    }
}
