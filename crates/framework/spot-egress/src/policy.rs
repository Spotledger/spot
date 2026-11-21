use crate::{AddrClass, EgressError, Purpose};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// An entry in the egress allow list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowEntry {
    /// Scheme: http or https
    pub scheme: Option<String>,

    /// Host: exact match or *.suffix wildcard
    pub host: String,

    /// Ports: ranges or single values
    pub ports: Option<Vec<u16>>,

    /// Purpose: optional; if specified, only applies to this purpose
    pub purpose: Option<String>,

    /// Allow private addresses by explicit CIDR/address
    pub allow_private: Option<Vec<String>>,
}

/// A deny entry that blocks specific destinations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenyEntry {
    pub scheme: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

/// Egress policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressPolicy {
    /// Default policy: deny_non_public or allow
    #[serde(default = "default_deny")]
    pub default: String,

    /// Dangerous ports to deny: 22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250
    #[serde(default = "default_dangerous_ports")]
    pub dangerous_ports: Vec<u16>,

    /// Per-tenant configurations
    #[serde(default)]
    pub tenants: std::collections::HashMap<String, TenantPolicy>,
}

fn default_deny() -> String {
    "deny_non_public".to_string()
}

fn default_dangerous_ports() -> Vec<u16> {
    vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250]
}

/// Per-tenant egress policy.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TenantPolicy {
    /// Allow list entries
    #[serde(default)]
    pub allow: Vec<AllowEntry>,

    /// Deny list entries
    #[serde(default)]
    pub deny: Vec<DenyEntry>,

    /// Connect timeout in seconds
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,

    /// TLS timeout in seconds
    #[serde(default = "default_tls_timeout")]
    pub tls_timeout_secs: u64,

    /// Total request timeout in seconds
    #[serde(default = "default_total_timeout")]
    pub total_timeout_secs: u64,

    /// Max response body size in bytes
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,

    /// Max redirect hops
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
}

fn default_connect_timeout() -> u64 {
    10
}

fn default_tls_timeout() -> u64 {
    10
}

fn default_total_timeout() -> u64 {
    30
}

fn default_max_response_bytes() -> u64 {
    100 * 1024 * 1024 // 100 MB
}

fn default_max_redirects() -> u32 {
    5
}

impl EgressPolicy {
    /// Create a new policy from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, EgressError> {
        toml::from_str(toml_str)
            .map_err(|e| EgressError::InvalidConfig(format!("TOML parsing failed: {}", e)))
    }

    /// Check if a destination is allowed by policy.
    pub fn is_allowed(
        &self,
        tenant: &str,
        purpose: Purpose,
        scheme: &str,
        host: &str,
        port: u16,
    ) -> bool {
        let tenant_policy = match self.tenants.get(tenant) {
            Some(p) => p,
            None => {
                // Tenant not in policy; check global default
                return self.default == "allow";
            }
        };

        // Check deny list first - deny wins
        for deny in &tenant_policy.deny {
            if self.matches_entry(deny, None, scheme, host, port) {
                return false;
            }
        }

        // Check allow list
        for allow in &tenant_policy.allow {
            if !self.entry_applies(allow, purpose, scheme, host) {
                continue;
            }

            // Check port
            if let Some(ref ports) = allow.ports {
                if !ports.contains(&port) {
                    continue;
                }
            } else {
                // If no ports specified, match any port
            }

            return true;
        }

        // No allow entry matched
        self.default == "allow"
    }

    /// Check if an address is allowed by policy.
    pub fn is_address_allowed(
        &self,
        tenant: &str,
        _purpose: Purpose,
        addr: IpAddr,
        port: u16,
        class: AddrClass,
    ) -> bool {
        // Public addresses are always allowed (unless denied by other rules)
        if class == AddrClass::Public {
            return true;
        }

        // Cloud metadata is never reachable, whatever a tenant lists.
        if is_platform_denied(addr, class) {
            return false;
        }

        // Non-public addresses need explicit allow by address/CIDR
        let tenant_policy = match self.tenants.get(tenant) {
            Some(p) => p,
            None => {
                return false;
            }
        };

        // Check if address is explicitly allowed
        for allow in &tenant_policy.allow {
            if let Some(ref allow_private) = allow.allow_private {
                for entry in allow_private {
                    if self.address_matches(entry, addr, port) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Whether `addr` may be reached for this exact destination: the same decision as
    /// [`Self::is_address_allowed`], except that a tenant's `allow_private` entry counts only when
    /// the entry itself covers the destination (purpose, scheme, host and, if the entry lists
    /// ports, the port). An entry for one host or purpose never opens a private address for
    /// another. Cloud metadata addresses are refused whatever the tenant lists.
    #[allow(clippy::too_many_arguments)]
    pub fn is_address_allowed_for(
        &self,
        tenant: &str,
        purpose: Purpose,
        scheme: &str,
        host: &str,
        addr: IpAddr,
        port: u16,
        class: AddrClass,
    ) -> bool {
        if class == AddrClass::Public {
            return true;
        }
        if is_platform_denied(addr, class) {
            return false;
        }
        let Some(tenant_policy) = self.tenants.get(tenant) else {
            return false;
        };
        tenant_policy.allow.iter().any(|allow| {
            self.entry_applies(allow, purpose, scheme, host)
                && allow
                    .ports
                    .as_ref()
                    .is_none_or(|ports| ports.contains(&port))
                && allow.allow_private.as_ref().is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|entry| self.address_matches(entry, addr, port))
                })
        })
    }

    /// Whether `port` may be used for this destination: any port outside the dangerous list, and
    /// a dangerous one only when an allow entry covering the destination lists that port
    /// explicitly (an entry without `ports` never opens a dangerous port).
    pub fn is_port_allowed_for(
        &self,
        tenant: &str,
        purpose: Purpose,
        scheme: &str,
        host: &str,
        port: u16,
    ) -> bool {
        if self.is_port_allowed(port) {
            return true;
        }
        let Some(tenant_policy) = self.tenants.get(tenant) else {
            return false;
        };
        tenant_policy.allow.iter().any(|allow| {
            self.entry_applies(allow, purpose, scheme, host)
                && allow
                    .ports
                    .as_ref()
                    .is_some_and(|ports| ports.contains(&port))
        })
    }

    /// Whether the policy has an entry for this tenant.
    pub fn has_tenant(&self, tenant: &str) -> bool {
        self.tenants.contains_key(tenant)
    }

    /// Whether an allow entry covers this purpose, scheme and host (ports are checked by the
    /// caller). An entry without a scheme covers every non-HTTP destination and `https`, never
    /// plain `http`.
    fn entry_applies(
        &self,
        allow: &AllowEntry,
        purpose: Purpose,
        scheme: &str,
        host: &str,
    ) -> bool {
        if let Some(ref allow_purpose) = allow.purpose {
            if allow_purpose != purpose.as_str() {
                return false;
            }
        }
        if let Some(ref allow_scheme) = allow.scheme {
            if allow_scheme != scheme {
                return false;
            }
        } else if scheme == "http" {
            // Default to https only
            return false;
        }
        self.host_matches(&allow.host, host)
    }

    /// Check if a port is allowed.
    pub fn is_port_allowed(&self, port: u16) -> bool {
        !self.dangerous_ports.contains(&port)
    }

    /// Check if a host matches a pattern (exact or wildcard).
    fn host_matches(&self, pattern: &str, host: &str) -> bool {
        if pattern == host {
            return true;
        }

        if let Some(suffix) = pattern.strip_prefix("*.") {
            // Must end with the suffix AND have a dot before it (proper subdomain)
            if host.ends_with(suffix) && host != suffix && host.len() > suffix.len() {
                let prefix = &host[..host.len() - suffix.len()];
                return prefix.ends_with('.');
            }
        }

        false
    }

    /// Whether `pattern` names `addr` and `port`: an address (`10.20.0.5`), a CIDR
    /// (`10.20.0.0/16`), or either with a port (`10.20.0.5:5432`, `[fd00::5]:443`). With a
    /// port in the entry only that port matches.
    fn address_matches(&self, pattern: &str, addr: IpAddr, port: u16) -> bool {
        let (target, wanted_port) = split_port(pattern);
        if wanted_port.is_some_and(|p| p != port) {
            return false;
        }
        if let Ok(ip) = target.parse::<IpAddr>() {
            return ip == addr;
        }
        target
            .parse::<ipnet::IpNet>()
            .is_ok_and(|net| net.contains(&addr))
    }

    /// Check if a deny entry matches a request.
    fn matches_entry(
        &self,
        deny: &DenyEntry,
        _purpose: Option<Purpose>,
        scheme: &str,
        host: &str,
        port: u16,
    ) -> bool {
        if let Some(ref deny_scheme) = deny.scheme {
            if deny_scheme != scheme {
                return false;
            }
        }

        if let Some(ref deny_host) = deny.host {
            if !self.host_matches(deny_host, host) {
                return false;
            }
        }

        if let Some(deny_port) = deny.port {
            if deny_port != port {
                return false;
            }
        }

        true
    }
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            default: default_deny(),
            dangerous_ports: default_dangerous_ports(),
            tenants: Default::default(),
        }
    }
}

/// Addresses no tenant entry can open: the cloud metadata endpoints, including when they are
/// written as an IPv4-mapped IPv6 address.
fn is_platform_denied(addr: IpAddr, class: AddrClass) -> bool {
    class == AddrClass::CloudMetadata
        || crate::classify(addr.to_canonical()) == AddrClass::CloudMetadata
}

/// Splits an optional trailing port off an address or CIDR entry.
fn split_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some((ip, tail)) = rest.split_once(']') {
            return (ip, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
        }
    }
    if entry.matches(':').count() == 1 {
        if let Some((ip, port)) = entry.split_once(':') {
            if let Ok(p) = port.parse() {
                return (ip, Some(p));
            }
        }
    }
    (entry, None)
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_host_matches_exact() {
        let policy = EgressPolicy::default();
        assert!(policy.host_matches("example.com", "example.com"));
        assert!(!policy.host_matches("example.com", "other.com"));
    }

    #[test]
    fn test_host_matches_wildcard() {
        let policy = EgressPolicy::default();
        assert!(policy.host_matches("*.example.com", "api.example.com"));
        assert!(policy.host_matches("*.example.com", "www.example.com"));
        assert!(!policy.host_matches("*.example.com", "example.com"));
        assert!(!policy.host_matches("*.example.com", "notexample.com"));
    }

    #[test]
    fn test_is_port_allowed() {
        let policy = EgressPolicy::default();
        assert!(!policy.is_port_allowed(22)); // SSH
        assert!(!policy.is_port_allowed(25)); // SMTP
        assert!(!policy.is_port_allowed(3306)); // MySQL
        assert!(policy.is_port_allowed(80)); // HTTP
        assert!(policy.is_port_allowed(443)); // HTTPS
    }

    #[test]
    fn test_from_toml() {
        let toml = r#"
default = "deny_non_public"

[[tenants.tenant1.allow]]
scheme = "https"
host = "api.example.com"
ports = [443]
"#;
        let policy = EgressPolicy::from_toml(toml);
        if let Err(e) = &policy {
            eprintln!("TOML parse error: {}", e);
        }
        assert!(policy.is_ok());
    }

    #[test]
    fn test_deny_beats_allow() {
        let mut policy = EgressPolicy::default();
        let mut tenant_policy = TenantPolicy::default();

        tenant_policy.allow.push(AllowEntry {
            scheme: Some("https".to_string()),
            host: "example.com".to_string(),
            ports: Some(vec![443]),
            purpose: None,
            allow_private: None,
        });

        tenant_policy.deny.push(DenyEntry {
            scheme: Some("https".to_string()),
            host: Some("example.com".to_string()),
            port: Some(443),
        });

        policy.tenants.insert("tenant1".to_string(), tenant_policy);

        let allowed = policy.is_allowed("tenant1", Purpose::Webhook, "https", "example.com", 443);
        assert!(!allowed);
    }

    #[test]
    fn private_allow_is_by_explicit_address_cidr_and_port_only() {
        let p = EgressPolicy::from_toml(
            r#"
[tenants.t]
[[tenants.t.allow]]
host = "db.test"
allow_private = ["10.20.0.5:5432", "192.168.0.0/16", "[fd00::5]:443"]
"#,
        )
        .unwrap();
        let ok = |ip: &str, port: u16| {
            let addr: IpAddr = ip.parse().unwrap();
            p.is_address_allowed("t", Purpose::Connector, addr, port, crate::classify(addr))
        };
        assert!(ok("10.20.0.5", 5432));
        assert!(!ok("10.20.0.5", 5433), "another port");
        assert!(!ok("10.20.0.6", 5432), "another address");
        assert!(ok("192.168.7.9", 80), "inside the CIDR, any port");
        assert!(!ok("172.16.0.1", 80), "private but outside every entry");
        assert!(ok("fd00::5", 443));
        assert!(!ok("fd00::5", 80));
        assert!(!p.is_address_allowed(
            "other",
            Purpose::Connector,
            "10.20.0.5".parse().unwrap(),
            5432,
            crate::AddrClass::Private
        ));
    }
}
