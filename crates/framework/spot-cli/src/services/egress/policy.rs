//! Loading and checking the policy file (`[services.egress] policy_file`).
//!
//! The file is the tenant allow-list: the TOML `spot_egress::EgressPolicy::from_toml` reads. This
//! module adds what that parser does not: the file's mode, a size cap, a digest an operator can
//! compare, and a validation that reports **every** problem in one pass (a typo in the third
//! entry must not hide a typo in the fifth).
//!
//! Nothing here reads the network. A problem names the key path and what is wrong with it, and
//! quotes a value only where the value is a host, a port, a purpose or a tenant name (the file
//! holds no secret by design).

use std::net::IpAddr;
use std::path::Path;

use sha2::{Digest, Sha256};
use spot_egress::{EgressPolicy, TenantPolicy};

use super::codes;

/// The largest policy file read, in bytes.
pub const MAX_POLICY_BYTES: u64 = 256 * 1024;

/// The closed set of purposes an allow entry may name (`Purpose::as_str`).
pub const PURPOSES: [&str; 8] = [
    "webhook",
    "script",
    "connector",
    "mail",
    "llm",
    "template",
    "pdf",
    "app",
];

/// The longest tenant name or host pattern accepted.
const NAME_MAX: usize = 253;
/// The longest problem text kept (a parser message can quote a line).
const PROBLEM_MAX: usize = 400;

/// A policy that loaded and passed every check.
#[derive(Clone, Debug)]
pub struct LoadedPolicy {
    /// The policy.
    pub policy: EgressPolicy,
    /// The SHA-256 of the file's bytes, lower-case hex.
    pub digest: String,
    /// The tenant names the policy lists, sorted.
    pub tenants: Vec<String>,
}

/// Why a policy file was refused: a stable code and every problem found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyProblem {
    /// One of `egress_host.policy_unreadable`, `egress_host.policy_unsafe`,
    /// `egress_host.policy_invalid`.
    pub code: &'static str,
    /// The problems, each one line.
    pub problems: Vec<String>,
}

impl PolicyProblem {
    fn one(code: &'static str, text: impl Into<String>) -> Self {
        Self {
            code,
            problems: vec![text.into()],
        }
    }

    /// The problems as one text: the code, then one line each.
    pub fn render(&self) -> String {
        let mut out = String::from(self.code);
        for problem in &self.problems {
            out.push_str("\n  - ");
            out.push_str(problem);
        }
        out
    }
}

/// The digest of `bytes`, lower-case hex.
pub fn digest_of(bytes: &[u8]) -> String {
    let sum = Sha256::digest(bytes);
    let mut out = String::with_capacity(sum.len() * 2);
    for byte in sum {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Reads, checks and parses the policy file at `path`.
///
/// # Errors
///
/// [`PolicyProblem`]: the file is missing, not a regular file or unreadable
/// (`policy_unreadable`); it is writable by everyone (`policy_unsafe`); or it does not parse or
/// holds an entry that fails [`validate`] (`policy_invalid`, every problem listed).
pub fn load_policy_file(path: &Path) -> Result<LoadedPolicy, PolicyProblem> {
    let meta = std::fs::metadata(path).map_err(|e| {
        PolicyProblem::one(
            codes::POLICY_UNREADABLE,
            format!("the policy file cannot be read ({})", kind(&e)),
        )
    })?;
    if !meta.is_file() {
        return Err(PolicyProblem::one(
            codes::POLICY_UNREADABLE,
            "the policy file is not a regular file",
        ));
    }
    if meta.len() > MAX_POLICY_BYTES {
        return Err(PolicyProblem::one(
            codes::POLICY_INVALID,
            format!("the policy file is larger than {MAX_POLICY_BYTES} bytes"),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o002 != 0 {
            return Err(PolicyProblem::one(
                codes::POLICY_UNSAFE,
                "the policy file is writable by everyone; run chmod o-w on it",
            ));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| {
        PolicyProblem::one(
            codes::POLICY_UNREADABLE,
            format!("the policy file cannot be read ({})", kind(&e)),
        )
    })?;
    if bytes.len() as u64 > MAX_POLICY_BYTES {
        return Err(PolicyProblem::one(
            codes::POLICY_INVALID,
            format!("the policy file is larger than {MAX_POLICY_BYTES} bytes"),
        ));
    }
    let text = String::from_utf8(bytes.clone()).map_err(|_| {
        PolicyProblem::one(codes::POLICY_INVALID, "the policy file is not UTF-8 text")
    })?;
    let policy = EgressPolicy::from_toml(&text)
        .map_err(|e| PolicyProblem::one(codes::POLICY_INVALID, clip(&format!("{e}"))))?;
    let problems = validate(&policy);
    if !problems.is_empty() {
        return Err(PolicyProblem {
            code: codes::POLICY_INVALID,
            problems,
        });
    }
    let mut tenants: Vec<String> = policy.tenants.keys().cloned().collect();
    tenants.sort();
    Ok(LoadedPolicy {
        policy,
        digest: digest_of(&bytes),
        tenants,
    })
}

/// The kind of an I/O error, as a word; never its message (which can quote a path).
fn kind(e: &std::io::Error) -> String {
    format!("{:?}", e.kind()).to_lowercase()
}

/// Bounds a text and escapes control characters.
fn clip(text: &str) -> String {
    let escaped: String = text.escape_debug().collect();
    if escaped.chars().count() > PROBLEM_MAX {
        let mut cut: String = escaped.chars().take(PROBLEM_MAX).collect();
        cut.push_str("...");
        cut
    } else {
        escaped
    }
}

/// Every problem in `policy`, in a stable order. Empty means the policy is acceptable.
pub fn validate(policy: &EgressPolicy) -> Vec<String> {
    let mut out = Vec::new();
    if !matches!(policy.default.as_str(), "deny_non_public" | "allow") {
        out.push(format!(
            "default: `{}` is not deny_non_public or allow",
            clip(&policy.default)
        ));
    }
    if policy.dangerous_ports.contains(&0) {
        out.push("dangerous_ports: port 0 is not a port".to_owned());
    }
    let mut names: Vec<&String> = policy.tenants.keys().collect();
    names.sort();
    for name in names {
        let label = clip(name);
        if !valid_tenant(name) {
            out.push(format!(
                "tenants.{label}: a tenant name is 1 to {NAME_MAX} letters, digits, `.`, `_` or `-`"
            ));
        }
        if let Some(tenant) = policy.tenants.get(name) {
            validate_tenant(&label, tenant, &mut out);
        }
    }
    out
}

fn valid_tenant(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn validate_tenant(label: &str, tenant: &TenantPolicy, out: &mut Vec<String>) {
    for (i, allow) in tenant.allow.iter().enumerate() {
        let at = format!("tenants.{label}.allow[{i}]");
        if !valid_host_pattern(&allow.host) {
            out.push(format!(
                "{at}.host: `{}` is not a host name, an address or `*.suffix`",
                clip(&allow.host)
            ));
        }
        if let Some(scheme) = &allow.scheme
            && !matches!(scheme.as_str(), "http" | "https")
        {
            out.push(format!(
                "{at}.scheme: `{}` is not http or https",
                clip(scheme)
            ));
        }
        if let Some(ports) = &allow.ports {
            if ports.is_empty() {
                out.push(format!(
                    "{at}.ports: an empty list; leave the key out to allow every ordinary port"
                ));
            }
            if ports.contains(&0) {
                out.push(format!("{at}.ports: port 0 is not a port"));
            }
        }
        if let Some(purpose) = &allow.purpose
            && !PURPOSES.contains(&purpose.as_str())
        {
            out.push(format!(
                "{at}.purpose: `{}` is not one of {}",
                clip(purpose),
                PURPOSES.join(", ")
            ));
        }
        if let Some(entries) = &allow.allow_private {
            for (j, entry) in entries.iter().enumerate() {
                if !valid_private_entry(entry) {
                    out.push(format!(
                        "{at}.allow_private[{j}]: `{}` is not an address, a CIDR or either with a port",
                        clip(entry)
                    ));
                }
            }
        }
    }
    for (i, deny) in tenant.deny.iter().enumerate() {
        let at = format!("tenants.{label}.deny[{i}]");
        if deny.scheme.is_none() && deny.host.is_none() && deny.port.is_none() {
            out.push(format!(
                "{at}: names no scheme, host or port, so it would match everything"
            ));
        }
        if let Some(scheme) = &deny.scheme
            && !matches!(scheme.as_str(), "http" | "https")
        {
            out.push(format!(
                "{at}.scheme: `{}` is not http or https",
                clip(scheme)
            ));
        }
        if let Some(host) = &deny.host
            && !valid_host_pattern(host)
        {
            out.push(format!(
                "{at}.host: `{}` is not a host name, an address or `*.suffix`",
                clip(host)
            ));
        }
        if deny.port == Some(0) {
            out.push(format!("{at}.port: port 0 is not a port"));
        }
    }
    let timeouts = [
        ("connect_timeout_secs", tenant.connect_timeout_secs),
        ("tls_timeout_secs", tenant.tls_timeout_secs),
        ("total_timeout_secs", tenant.total_timeout_secs),
    ];
    for (key, value) in timeouts {
        if value == 0 || value > 3600 {
            out.push(format!("tenants.{label}.{key}: use 1 to 3600 seconds"));
        }
    }
    if tenant.max_response_bytes == 0 || tenant.max_response_bytes > (1 << 32) {
        out.push(format!(
            "tenants.{label}.max_response_bytes: use 1 to 4294967296"
        ));
    }
    if tenant.max_redirects > 20 {
        out.push(format!("tenants.{label}.max_redirects: use 0 to 20"));
    }
}

/// A host name, an address (an IPv6 literal in brackets, as a URL carries it) or `*.suffix`.
fn valid_host_pattern(host: &str) -> bool {
    if host.is_empty() || host.len() > NAME_MAX {
        return false;
    }
    let body = host.strip_prefix("*.").unwrap_or(host);
    !body.is_empty()
        && !body.contains('*')
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

/// `10.20.0.5`, `10.20.0.0/16`, `[fd00::5]:443`, `10.20.0.5:5432` and the like.
fn valid_private_entry(entry: &str) -> bool {
    let (addr, port_ok) = split_port(entry);
    if !port_ok {
        return false;
    }
    match addr.split_once('/') {
        None => addr.parse::<IpAddr>().is_ok(),
        Some((ip, bits)) => {
            let Ok(ip) = ip.parse::<IpAddr>() else {
                return false;
            };
            let max = if ip.is_ipv4() { 32 } else { 128 };
            bits.parse::<u8>().is_ok_and(|b| b <= max)
        }
    }
}

/// Splits a trailing `:port` off an entry; the bool is false for a port that is not 1 to 65535.
fn split_port(entry: &str) -> (&str, bool) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some((ip, tail)) = rest.split_once(']') {
            return match tail.strip_prefix(':') {
                None => (ip, tail.is_empty()),
                Some(p) => (ip, p.parse::<u16>().is_ok_and(|p| p != 0)),
            };
        }
        return (entry, false);
    }
    if entry.matches(':').count() == 1
        && let Some((ip, port)) = entry.split_once(':')
    {
        return (ip, port.parse::<u16>().is_ok_and(|p| p != 0));
    }
    (entry, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(text: &str) -> EgressPolicy {
        EgressPolicy::from_toml(text).unwrap()
    }

    #[test]
    fn a_good_policy_has_no_problems() {
        let policy = parse(
            r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "smtp.relay.example"
ports = [587]
purpose = "mail"
allow_private = ["10.0.0.0/8", "[fd00::5]:443"]
"#,
        );
        assert!(validate(&policy).is_empty());
    }

    #[test]
    fn every_problem_is_listed_at_once() {
        let policy = parse(
            r#"
default = "permit"
[tenants.acme]
total_timeout_secs = 0
[[tenants.acme.allow]]
host = "a*b"
ports = [0]
purpose = "telnet"
allow_private = ["not-an-address"]
[[tenants.acme.deny]]
"#,
        );
        let problems = validate(&policy);
        assert!(problems.len() >= 7, "{problems:?}");
    }

    #[test]
    fn private_entries_parse_like_the_policy_does() {
        for ok in [
            "10.0.0.5",
            "10.0.0.0/16",
            "10.0.0.5:5432",
            "[::1]:25",
            "fd00::/8",
        ] {
            assert!(valid_private_entry(ok), "{ok}");
        }
        for bad in ["10.0.0.5:0", "10.0.0.0/33", "[::1", "x", "10.0.0.5:99999"] {
            assert!(!valid_private_entry(bad), "{bad}");
        }
    }
}
