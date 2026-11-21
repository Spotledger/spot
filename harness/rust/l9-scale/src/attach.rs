//! `--attach URL`: load against a deployment that is already running.
//!
//! The harness must never be pointed at production by accident, so the host must be a
//! loopback address, or be named in an allow-list file, unless the operator passes
//! `--i-know-this-is-a-test-system`. Only plain `http` is spoken (reach a TLS deployment
//! through a tunnel to a loopback port). User credentials come from a file of
//! `email<whitespace>password` lines that is read into memory and never echoed.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use crate::http::Target;
use crate::session::{Secret, SimUser};

/// A parsed `http://host[:port]` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachUrl {
    /// The host name or IP literal.
    pub host: String,
    /// The port (80 when absent).
    pub port: u16,
}

/// Parses `http://host[:port][/]`; anything else is refused with the reason.
pub fn parse_url(url: &str) -> Result<AttachUrl, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        "--attach takes an http:// URL (TLS is not supported; use a tunnel to a loopback port)"
            .to_owned()
    })?;
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return Err("--attach takes a bare http://host[:port] URL".to_owned());
    }
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (host, tail) = stripped
            .split_once(']')
            .ok_or_else(|| "unterminated [ in the --attach URL".to_owned())?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p
                .parse()
                .map_err(|_| "bad port in the --attach URL".to_owned())?,
            None if tail.is_empty() => 80,
            None => return Err("bad --attach URL".to_owned()),
        };
        (host.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (
                host.to_owned(),
                port.parse()
                    .map_err(|_| "bad port in the --attach URL".to_owned())?,
            ),
            None => (authority.to_owned(), 80),
        }
    };
    if host.is_empty() {
        return Err("the --attach URL has no host".to_owned());
    }
    Ok(AttachUrl { host, port })
}

/// Whether `host` is allowed as a target: loopback, listed in `allow`, or `override_flag`.
pub fn check_host(host: &str, allow: &[String], override_flag: bool) -> Result<(), String> {
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if loopback || override_flag || allow.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        return Ok(());
    }
    Err(format!(
        "refusing to load `{host}`: it is not a loopback address and not in the allow-list. \
         Add it to an --allow-hosts file, or pass --i-know-this-is-a-test-system if it is one"
    ))
}

/// Reads an allow-list: one host per line, `#` starts a comment.
pub fn parse_allow_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Reads user credentials: `email password` per line, `#` comments and blank lines skipped.
pub fn parse_users(text: &str) -> Result<Vec<SimUser>, String> {
    let mut users = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (email, password) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("users file line {}: expected `email password`", number + 1))?;
        users.push(SimUser {
            email: email.to_owned(),
            password: Secret::new(password.trim()),
        });
    }
    Ok(users)
}

/// Resolves the URL to a socket address and the `Host` header for `site` (or the URL's own
/// host when no site is given).
pub async fn resolve(url: &AttachUrl, site: Option<&str>) -> Result<Target, String> {
    let addr: SocketAddr = tokio::net::lookup_host((url.host.as_str(), url.port))
        .await
        .map_err(|e| format!("cannot resolve {}: {e}", url.host))?
        .next()
        .ok_or_else(|| format!("{} resolves to no address", url.host))?;
    let host = site.map_or_else(
        || {
            if url.port == 80 {
                url.host.clone()
            } else {
                format!("{}:{}", url.host, url.port)
            }
        },
        str::to_owned,
    );
    Ok(Target { addr, host })
}

/// Loads the credentials file.
pub fn load_users(path: &Path) -> Result<Vec<SimUser>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the users file {}: {e}", path.display()))?;
    parse_users(&text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn urls_are_parsed_and_odd_ones_refused() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080/").unwrap(),
            AttachUrl {
                host: "127.0.0.1".into(),
                port: 8080
            }
        );
        assert_eq!(parse_url("http://example.test").unwrap().port, 80);
        assert_eq!(parse_url("http://[::1]:9000").unwrap().host, "::1");
        for bad in [
            "https://x",
            "http://",
            "http://u@h",
            "http://h/path",
            "http://h:x",
            "ftp://h",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_loopback_or_listed_hosts_are_allowed_without_the_flag() {
        let allow = vec!["staging.example.test".to_owned()];
        for ok in ["localhost", "LOCALHOST", "127.0.0.1", "127.3.4.5", "::1"] {
            assert!(check_host(ok, &[], false).is_ok(), "{ok}");
        }
        assert!(check_host("staging.example.test", &allow, false).is_ok());
        assert!(check_host("STAGING.example.test", &allow, false).is_ok());
        for bad in [
            "erp.example.com",
            "10.0.0.5",
            "localhost.evil.test",
            "0.0.0.0",
        ] {
            let err = check_host(bad, &allow, false).unwrap_err();
            assert!(err.contains("--i-know-this-is-a-test-system"), "{err}");
        }
        assert!(check_host("erp.example.com", &[], true).is_ok());
    }

    #[test]
    fn allow_lists_and_user_files_ignore_comments() {
        assert_eq!(
            parse_allow_list("# hosts\nstaging.test # one\n\n  qa.test  \n"),
            ["staging.test", "qa.test"]
        );
        let users = parse_users("# who\na@x.test  pass word 1\n\nb@x.test p2\n").unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].email, "a@x.test");
        assert_eq!(users[0].password.expose(), "pass word 1");
        assert!(parse_users("only-one-field\n").is_err());
        assert!(!format!("{users:?}").contains("p2"));
    }

    #[tokio::test]
    async fn a_loopback_url_resolves_with_the_site_as_host_header() {
        let url = parse_url("http://127.0.0.1:8123").unwrap();
        let target = resolve(&url, Some("demo")).await.unwrap();
        assert_eq!(target.addr, "127.0.0.1:8123".parse().unwrap());
        assert_eq!(target.host, "demo");
        assert_eq!(resolve(&url, None).await.unwrap().host, "127.0.0.1:8123");
    }
}
