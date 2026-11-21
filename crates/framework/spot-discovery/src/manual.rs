//! The manual-address fallback: some networks have no multicast (many corporate
//! VLANs, some firewalls, older VPNs), and a user must be able to type the server
//! address. Returns the same `Found`-shaped data the browser produces, with
//! `source = Manual` in the selection results.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::error::DiscoveryError;
use crate::record::{
    AdvertisedSiteNames, Advertisement, DEFAULT_APP_PORT, PROTOCOL_VERSION, ServerId, TlsMode,
};
use crate::select::ManualServer;

/// Parses a manual `host:port` entry into a manual-selection server plus an
/// advertisement with the fields a client can assume.
///
/// Accepted forms:
/// - `192.168.1.10:8080` — IPv4 with port
/// - `192.168.1.10` — IPv4, default port
/// - `[2001:db8::1]:8080` — IPv6 literal in brackets with port
/// - `[2001:db8::1]` — IPv6 literal, default port
/// - `2001:db8::1` — bare IPv6 literal, default port
/// - `shopserver` — DNS host name, default port
/// - `shopserver.local.:8080` — mDNS-style name with trailing dot
/// - `shopserver:8080` — DNS host name with port
///
/// # Errors
/// [`DiscoveryError::ManualAddressInvalid`] for empty input, an empty host, a host
/// starting with `-` or `.` (option/mDNS-label confusion), a port outside 1..=65535 or
/// non-numeric, a bare IPv6 containing a port-like suffix, or a host name carrying
/// whitespace.
pub fn parse_manual(input: &str) -> Result<(ManualServer, Advertisement), DiscoveryError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the address is empty".to_string(),
        });
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the address contains whitespace".to_string(),
        });
    }
    if trimmed.starts_with(['-', '.']) {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the host may not start with '-' or '.'".to_string(),
        });
    }

    // Bracketed IPv6 literal.
    if trimmed.starts_with('[') {
        let Some(end) = trimmed.find(']') else {
            return Err(DiscoveryError::ManualAddressInvalid {
                reason: "the bracketed IPv6 host has no closing bracket".to_string(),
            });
        };
        let host = &trimmed[1..end];
        let host_ip =
            host.parse::<Ipv6Addr>()
                .map_err(|_| DiscoveryError::ManualAddressInvalid {
                    reason: "the bracketed host is not an IPv6 address".to_string(),
                })?;
        let rest = &trimmed[end + 1..];
        let port = match rest.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if rest.is_empty() => DEFAULT_APP_PORT,
            None => {
                return Err(DiscoveryError::ManualAddressInvalid {
                    reason: "only ':port' may follow the bracketed IPv6 host".to_string(),
                });
            }
        };
        return Ok(build(format!("[{host_ip}]"), port));
    }

    // Split host and port at the last colon — but only when what follows is digits and
    // there is at most one colon (a bare IPv6 has several).
    let (host_part, port) = match trimmed.rfind(':') {
        Some(idx)
            if trimmed[..idx].matches(':').count() == 0
                && trimmed[idx + 1..].bytes().all(|b| b.is_ascii_digit())
                && !trimmed[idx + 1..].is_empty() =>
        {
            let p = parse_port(&trimmed[idx + 1..])?;
            (&trimmed[..idx], p)
        }
        _ => (trimmed, DEFAULT_APP_PORT),
    };

    if host_part.is_empty() {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the host is empty".to_string(),
        });
    }

    // IPv4?
    if let Ok(v4) = host_part.parse::<Ipv4Addr>() {
        return Ok(build(v4.to_string(), port));
    }

    // Bare IPv6 literal (no brackets, no port suffix; the multi-colon case).
    if host_part.contains(':') {
        let v6 =
            host_part
                .parse::<Ipv6Addr>()
                .map_err(|_| DiscoveryError::ManualAddressInvalid {
                    reason: "the colon-separated host is not a valid IPv6 address".to_string(),
                })?;
        return Ok(build(format!("[{v6}]"), port));
    }

    // Host name: letters, digits, hyphens, dots.
    let ok = host_part
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    if !ok {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the host name may contain only letters, digits, hyphens and dots".to_string(),
        });
    }
    Ok(build(host_part.to_string(), port))
}

fn parse_port(p: &str) -> Result<u16, DiscoveryError> {
    let port = p
        .parse::<u16>()
        .map_err(|_| DiscoveryError::ManualAddressInvalid {
            reason: format!("the port '{p}' is not a number from 1 to 65535"),
        })?;
    if port == 0 {
        return Err(DiscoveryError::ManualAddressInvalid {
            reason: "the port may not be 0".to_string(),
        });
    }
    Ok(port)
}

fn build(host: String, port: u16) -> (ManualServer, Advertisement) {
    let advertisement = Advertisement {
        service_instance: ServerId("manual".to_string()),
        port,
        protocol_version: PROTOCOL_VERSION,
        // A manual entry knows nothing about the wire security yet: `Off` here means
        // "unknown, not yet negotiated", and the trust flow (Unprotected flag, key
        // fingerprint) is decided by the first connection, not by this parse.
        tls: TlsMode::Off,
        server_key_fingerprint: None,
        product_version: String::new(),
        site_names: AdvertisedSiteNames::default(),
    };
    (ManualServer { host, port }, advertisement)
}

#[cfg(test)]
mod tests {
    use super::parse_manual;
    use crate::error::DiscoveryError;
    use crate::record::DEFAULT_APP_PORT;

    #[test]
    fn ipv4_with_and_without_port() {
        let (m, _) = parse_manual("192.168.1.10:9443").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.host, "192.168.1.10");
        assert_eq!(m.port, 9443);
        let (m2, _) = parse_manual("192.168.1.10").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m2.host, "192.168.1.10");
        assert_eq!(m2.port, DEFAULT_APP_PORT);
    }

    #[test]
    fn ipv6_literal_in_brackets_and_bare() {
        let (m, _) = parse_manual("[fd00::a1]:8080").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.host, "[fd00::a1]");
        assert_eq!(m.port, 8080);
        let (m2, _) = parse_manual("[2001:db8::1]").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m2.host, "[2001:db8::1]");
        assert_eq!(m2.port, DEFAULT_APP_PORT);
        let (m3, _) = parse_manual("2001:db8::1").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m3.host, "[2001:db8::1]");
        assert_eq!(m3.port, DEFAULT_APP_PORT);
    }

    #[test]
    fn host_names_with_and_without_port() {
        let (m, _) = parse_manual("shopserver:8080").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.host, "shopserver");
        assert_eq!(m.port, 8080);
        let (m2, _) = parse_manual("shopserver.local.:9443").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m2.host, "shopserver.local.");
        let (m3, _) = parse_manual("SHOP-SERVER-1.local").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m3.host, "SHOP-SERVER-1.local");
    }

    #[test]
    fn default_port_used_for_every_bare_form() {
        for input in ["shopserver", "192.168.1.10", "[fd00::5]", "fd00::5"] {
            let (m, _) = parse_manual(input).unwrap_or_else(|e| panic!("{input}: {e:?}"));
            assert_eq!(m.port, DEFAULT_APP_PORT, "{input}");
        }
    }

    #[test]
    fn rejections() {
        let cases = [
            "",
            "   ",
            "a b:8080",
            "-flag:8080",
            ".hidden:8080",
            "host:",
            "host:notaport",
            "host:0",
            "host:70000",
            "host:99999999999",
            "host:8080x",
            "[fd00::1",
            "[12345]:8080",
            "host#bad",
            "[fd00::1]junk",
        ];
        for input in cases {
            let res = parse_manual(input);
            assert!(
                matches!(res, Err(DiscoveryError::ManualAddressInvalid { .. })),
                "'{input}' must be refused, got {res:?}"
            );
        }
    }

    #[test]
    fn port_boundaries_accepted() {
        let (m, _) = parse_manual("host:1").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.port, 1);
        let (m2, _) = parse_manual("host:65535").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m2.port, 65535);
    }

    #[test]
    fn whitespace_is_trimmed_but_inner_whitespace_refused() {
        let (m, _) = parse_manual("  shopserver:8080  ").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.host, "shopserver");
        assert!(matches!(
            parse_manual("shop server:8080"),
            Err(DiscoveryError::ManualAddressInvalid { .. })
        ));
    }

    #[test]
    fn manual_advertisement_carries_assumptions_only() {
        let (m, adv) = parse_manual("shopserver:9443").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(adv.service_instance.as_str(), "manual");
        assert_eq!(adv.port, 9443);
        assert!(adv.tls.is_unprotected());
        assert!(adv.server_key_fingerprint.is_none());
        let _ = m;
    }

    #[test]
    fn port_like_suffix_on_bare_ipv6_is_the_address_itself() {
        // "2001:db8::1:8080" is a valid bare IPv6: the address wins, no port is
        // split off. Only a value beyond the hextet limit is refused outright.
        let (m, _) = parse_manual("2001:db8::1:8080").unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.host, "[2001:db8::1:8080]");
        assert_eq!(m.port, DEFAULT_APP_PORT);
        assert!(matches!(
            parse_manual("2001:db8::1:65536"),
            Err(DiscoveryError::ManualAddressInvalid { .. })
        ));
    }
}
