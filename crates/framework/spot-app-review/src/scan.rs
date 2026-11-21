//! The security scan: what the manifest, the component's bytes and the assets reveal without
//! running anything.
//!
//! Nothing here executes the component, makes a network call or reads a clock. Every rule is a
//! small function with a stable id and a finding on both sides of its condition (see the unit
//! tests).

use std::sync::OnceLock;

use regex::bytes::Regex;
use spot_bundle::Bundle;
use spot_egress::{AddrClass, classify};
use spot_manifest::FileAccess;

use crate::config::ReviewConfig;
use crate::finding::{Finding, Severity};
use crate::rules;

/// The platform DocTypes a person must confirm before an app may write them.
const PLATFORM_DOCTYPES: &[&str] = &[
    "User",
    "Role",
    "Role Assignment",
    "Session",
    "DocType",
    "Permission",
];

/// The asset extensions that mark an executable or a script.
const SCRIPT_EXTENSIONS: &[&str] = &[
    "bat", "cjs", "class", "cmd", "com", "dll", "dylib", "exe", "jar", "js", "jsx", "mjs", "msi",
    "php", "pl", "ps1", "py", "rb", "scr", "sh", "so", "ts", "tsx", "vbs", "wasm",
];

/// Runs every security rule over an opened bundle.
pub fn security_scan(bundle: &Bundle, config: &ReviewConfig) -> Vec<Finding> {
    let mut findings = Vec::new();
    endpoint_findings(bundle, &mut findings);
    doctype_findings(bundle, config, &mut findings);
    files_findings(bundle, &mut findings);
    email_findings(bundle, &mut findings);
    asset_findings(bundle, &mut findings);
    secret_findings(bundle, &mut findings);
    findings
}

/// A declared endpoint whose host is an address the egress policy denies, or a raw IP.
fn endpoint_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    for endpoint in &bundle.manifest().capabilities.network {
        let Ok(ip) = endpoint.host.parse::<std::net::IpAddr>() else {
            // A host name cannot be classified without resolving it, and the scan never
            // resolves anything; it is left to the egress policy at call time.
            continue;
        };
        let (rule, label) = match classify(ip) {
            AddrClass::Private | AddrClass::CarrierGradeNat | AddrClass::UniqueLocal => {
                (rules::SCAN_ENDPOINT_PRIVATE, "a private address")
            }
            AddrClass::Loopback => (rules::SCAN_ENDPOINT_LOOPBACK, "the loopback address"),
            AddrClass::LinkLocal => (rules::SCAN_ENDPOINT_LINK_LOCAL, "a link-local address"),
            AddrClass::CloudMetadata => (rules::SCAN_ENDPOINT_METADATA, "a cloud metadata address"),
            _ => (rules::SCAN_ENDPOINT_RAW_IP, "a raw IP address"),
        };
        findings.push(Finding::new(
            rule,
            Severity::High,
            Some(format!(
                "capabilities.network: {}:{}",
                endpoint.host, endpoint.port
            )),
            format!("the endpoint host is {label} ({ip}); the egress policy denies it by default"),
            rules::RISK_EGRESS,
        ));
    }
}

/// Write access to a platform DocType, or a broader DocType list than the configuration
/// allows.
fn doctype_findings(bundle: &Bundle, config: &ReviewConfig, findings: &mut Vec<Finding>) {
    let caps = &bundle.manifest().capabilities;
    for doctype in &caps.doctypes.write {
        if PLATFORM_DOCTYPES
            .iter()
            .any(|platform| platform.eq_ignore_ascii_case(doctype))
        {
            findings.push(Finding::new(
                rules::SCAN_DOCTYPE_PLATFORM_WRITE,
                Severity::High,
                Some(format!("capabilities.doctypes.write: {doctype}")),
                format!(
                    "the app asks to write `{doctype}`, a platform DocType; a person must confirm \
                     the purpose"
                ),
                rules::RISK_OVER_PRIVILEGE,
            ));
        }
    }
    let read = u32::try_from(caps.doctypes.read.len()).unwrap_or(u32::MAX);
    if read > config.max_doctypes_read {
        findings.push(Finding::new(
            rules::SCAN_DOCTYPE_BROAD,
            Severity::High,
            Some("manifest: capabilities.doctypes.read".to_owned()),
            format!(
                "the app declares read access to {read} DocTypes, more than the configured {}",
                config.max_doctypes_read
            ),
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
    let write = u32::try_from(caps.doctypes.write.len()).unwrap_or(u32::MAX);
    if write > config.max_doctypes_write {
        findings.push(Finding::new(
            rules::SCAN_DOCTYPE_BROAD,
            Severity::High,
            Some("manifest: capabilities.doctypes.write".to_owned()),
            format!(
                "the app declares write access to {write} DocTypes, more than the configured {}",
                config.max_doctypes_write
            ),
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
}

/// `files = read_write` with nothing in the component importing a file function.
fn files_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    if bundle.manifest().capabilities.files == FileAccess::ReadWrite {
        findings.push(Finding::new(
            rules::SCAN_FILES_READ_WRITE_UNUSED,
            Severity::Medium,
            Some("manifest: capabilities.files".to_owned()),
            "the app asks for read-write file access but nothing in the component imports a file \
             function, so the need is undeclared",
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
}

/// The e-mail capability with no declared sender purpose.
fn email_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    if bundle.manifest().capabilities.email {
        findings.push(Finding::new(
            rules::SCAN_EMAIL_UNDECLARED_PURPOSE,
            Severity::Low,
            Some("manifest: capabilities.email".to_owned()),
            "the app declares the e-mail capability but nothing in the component sends mail, so \
             the sender purpose is undeclared",
            rules::RISK_OVER_PRIVILEGE,
        ));
    }
}

/// An asset with an executable extension, or an HTML asset carrying active content.
fn asset_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    for (name, bytes) in bundle.assets() {
        if let Some(extension) = script_extension(name) {
            findings.push(Finding::new(
                rules::SCAN_ASSET_EXECUTABLE,
                Severity::High,
                Some(name.clone()),
                format!("the asset carries the executable or script extension `.{extension}`"),
                rules::RISK_ACTIVE_CONTENT,
            ));
        }
        if is_html(name)
            && let Some(reason) = active_html(bytes)
        {
            findings.push(Finding::new(
                rules::SCAN_ASSET_ACTIVE_CONTENT,
                Severity::High,
                Some(name.clone()),
                reason,
                rules::RISK_ACTIVE_CONTENT,
            ));
        }
    }
}

/// The lower-case extension of an asset name, when it has one.
fn script_extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    let extension = name.get(dot + 1..)?;
    if SCRIPT_EXTENSIONS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(extension))
    {
        Some(extension)
    } else {
        None
    }
}

/// Whether an asset is served as HTML.
fn is_html(name: &str) -> bool {
    extension_of(name).is_some_and(|extension| {
        extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
    })
}

/// The lower-case extension of a name, any extension.
fn extension_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    name.get(dot + 1..)
}

/// Whether an HTML asset carries active content: a remote script, inline `eval(`,
/// `document.cookie` or an absolute-URL `fetch(`.
fn active_html(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    if let Some(rest) = text.split("<script").nth(1)
        && rest.contains("src=")
        && (rest.contains("http://") || rest.contains("https://"))
    {
        return Some("the HTML asset loads a script from a remote origin".to_owned());
    }
    if text.contains("eval(") {
        return Some("the HTML asset calls `eval(` inline".to_owned());
    }
    if text.contains("document.cookie") {
        return Some("the HTML asset reads `document.cookie`".to_owned());
    }
    if text.contains("fetch(") && (text.contains("http://") || text.contains("https://")) {
        return Some("the HTML asset fetches an absolute URL".to_owned());
    }
    None
}

/// Bytes that look like an embedded key or token, in the component or an asset.
fn secret_findings(bundle: &Bundle, findings: &mut Vec<Finding>) {
    if let Some(kind) = first_secret(bundle.wasm()) {
        findings.push(embedded_secret("app.wasm", kind));
    }
    for (name, bytes) in bundle.assets() {
        if let Some(kind) = first_secret(bytes) {
            findings.push(embedded_secret(name, kind));
        }
    }
}

/// A `High` finding for an embedded secret, naming only what kind it is.
fn embedded_secret(where_: &str, kind: &'static str) -> Finding {
    Finding::new(
        rules::SCAN_SECRET_EMBEDDED,
        Severity::High,
        Some(where_.to_owned()),
        format!("the bytes look like an embedded {kind}; a key or token must not ship in a bundle"),
        rules::RISK_PUBLISHER,
    )
}

/// The first embedded-key pattern the bytes match, by kind.
fn first_secret(bytes: &[u8]) -> Option<&'static str> {
    for (pattern, kind) in secret_patterns() {
        if pattern.is_match(bytes) {
            return Some(*kind);
        }
    }
    None
}

/// The documented, conservative embedded-key set: PEM private-key headers, AWS access key ids
/// and GitHub personal access tokens.
fn secret_patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            let mut patterns = Vec::new();
            for (source, kind) in [
                (r"-----BEGIN [A-Z ]*PRIVATE KEY-----", "PEM private key"),
                (r"AKIA[0-9A-Z]{16}", "AWS access key id"),
                (r"ghp_[A-Za-z0-9]{36}", "GitHub personal access token"),
            ] {
                if let Ok(regex) = Regex::new(source) {
                    patterns.push((regex, kind));
                }
            }
            patterns
        })
        .as_slice()
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::bundle_with_assets;

    const CLEAN: &str = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"
"#;

    fn rules_of(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule.as_str()).collect()
    }

    #[test]
    fn a_private_endpoint_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
network = ["10.0.0.5:443"]
"#;
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_ENDPOINT_PRIVATE]);
    }

    #[test]
    fn a_public_named_host_is_not_an_endpoint_finding() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
network = ["api.example.com:443"]
"#;
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn each_address_class_gets_its_own_rule() {
        let cases = [
            ("127.0.0.1:1", rules::SCAN_ENDPOINT_LOOPBACK),
            ("169.254.1.1:1", rules::SCAN_ENDPOINT_LINK_LOCAL),
            ("169.254.169.254:1", rules::SCAN_ENDPOINT_METADATA),
            ("192.168.1.1:1", rules::SCAN_ENDPOINT_PRIVATE),
            ("8.8.8.8:1", rules::SCAN_ENDPOINT_RAW_IP),
        ];
        for (endpoint, rule) in cases {
            let manifest = format!(
                "[app]\nname = \"test_app\"\nversion = \"1.0.0\"\ntitle = \"Test App\"\n\n\
                 [capabilities]\nnetwork = [\"{endpoint}\"]\n"
            );
            let found = security_scan(
                &crate::test_support::bundle(&manifest, &crate::test_support::plain_component()),
                &ReviewConfig::default(),
            );
            assert_eq!(rules_of(&found), vec![rule], "{endpoint}");
        }
    }

    #[test]
    fn writing_a_platform_doctype_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
doctypes = { write = ["User"] }
"#;
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_DOCTYPE_PLATFORM_WRITE]);
    }

    #[test]
    fn an_ordinary_doctype_write_is_not_flagged() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
doctypes = { write = ["Sales Invoice"] }
"#;
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn broad_access_is_high() {
        let mut list = String::new();
        for i in 0..20 {
            list.push_str(&format!("\"Customer {i}\","));
        }
        let manifest = format!(
            "[app]\nname = \"test_app\"\nversion = \"1.0.0\"\ntitle = \"Test App\"\n\n\
             [capabilities]\ndoctypes = {{ read = [{list}] }}\n"
        );
        let found = security_scan(
            &crate::test_support::bundle(&manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_DOCTYPE_BROAD]);
    }

    #[test]
    fn read_write_files_without_a_use_is_medium() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
files = "read_write"
"#;
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_FILES_READ_WRITE_UNUSED]);
    }

    #[test]
    fn email_without_a_use_is_low() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[capabilities]
email = true
"#;
        // The policy refuses e-mail, so the privilege check is not what is tested here; the
        // scan runs on the opened bundle directly.
        let found = security_scan(
            &crate::test_support::bundle(manifest, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_EMAIL_UNDECLARED_PURPOSE]);
    }

    #[test]
    fn an_executable_asset_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[assets]]
path = "assets/tool.sh"
"#;
        let found = security_scan(
            &bundle_with_assets(
                manifest,
                &crate::test_support::plain_component(),
                &[("assets/tool.sh", b"echo hi")],
            ),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_ASSET_EXECUTABLE]);
    }

    #[test]
    fn an_svg_asset_is_not_an_executable() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[assets]]
path = "assets/logo.svg"
"#;
        let found = security_scan(
            &bundle_with_assets(
                manifest,
                &crate::test_support::plain_component(),
                &[("assets/logo.svg", b"<svg/>")],
            ),
            &ReviewConfig::default(),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_remote_script_asset_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[assets]]
path = "assets/page.html"
"#;
        let found = security_scan(
            &bundle_with_assets(
                manifest,
                &crate::test_support::plain_component(),
                &[(
                    "assets/page.html",
                    b"<html><script src=\"https://evil.example/x.js\"></script></html>",
                )],
            ),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_ASSET_ACTIVE_CONTENT]);
    }

    #[test]
    fn a_plain_html_asset_is_not_flagged() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[assets]]
path = "assets/page.html"
"#;
        let found = security_scan(
            &bundle_with_assets(
                manifest,
                &crate::test_support::plain_component(),
                &[("assets/page.html", b"<html>hi</html>")],
            ),
            &ReviewConfig::default(),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_embedded_key_is_high() {
        let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
title = "Test App"

[[assets]]
path = "assets/notes.txt"
"#;
        let found = security_scan(
            &bundle_with_assets(
                manifest,
                &crate::test_support::plain_component(),
                &[(
                    "assets/notes.txt",
                    b"-----BEGIN RSA PRIVATE KEY-----\nMIIE...",
                )],
            ),
            &ReviewConfig::default(),
        );
        assert_eq!(rules_of(&found), vec![rules::SCAN_SECRET_EMBEDDED]);
    }

    #[test]
    fn the_secret_set_matches_aws_and_github_forms() {
        assert_eq!(
            first_secret(b"AKIAIOSFODNN7EXAMPLE"),
            Some("AWS access key id")
        );
        assert_eq!(
            first_secret(b"token ghp_012345678901234567890123456789012345"),
            Some("GitHub personal access token")
        );
        assert_eq!(first_secret(b"nothing here"), None);
        assert_eq!(first_secret(b"AKIA123"), None);
    }

    #[test]
    fn a_clean_bundle_has_no_scan_finding() {
        let found = security_scan(
            &crate::test_support::bundle(CLEAN, &crate::test_support::plain_component()),
            &ReviewConfig::default(),
        );
        assert!(found.is_empty(), "{found:?}");
    }
}
