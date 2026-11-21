use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Classification of an IP address for egress policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddrClass {
    Public,
    Loopback,
    Unspecified,
    Private,
    CarrierGradeNat,
    LinkLocal,
    CloudMetadata,
    UniqueLocal,
    Multicast,
    Broadcast,
    Documentation,
    Benchmarking,
    Ipv4Mapped,
    Ipv4Compatible,
    Other,
}

impl AddrClass {
    pub fn is_denied_by_default(&self) -> bool {
        matches!(
            self,
            AddrClass::Loopback
                | AddrClass::Unspecified
                | AddrClass::Private
                | AddrClass::CarrierGradeNat
                | AddrClass::LinkLocal
                | AddrClass::CloudMetadata
                | AddrClass::UniqueLocal
                | AddrClass::Multicast
                | AddrClass::Broadcast
                | AddrClass::Documentation
                | AddrClass::Benchmarking
                | AddrClass::Ipv4Mapped
                | AddrClass::Ipv4Compatible
                | AddrClass::Other
        )
    }
}

/// Classify an IPv4 address.
fn classify_ipv4(addr: Ipv4Addr) -> AddrClass {
    let octets = addr.octets();

    // Loopback: 127.0.0.0/8
    if octets[0] == 127 {
        return AddrClass::Loopback;
    }

    // Unspecified: 0.0.0.0/8
    if octets[0] == 0 {
        return AddrClass::Unspecified;
    }

    // Private RFC 1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
    if octets[0] == 10 {
        return AddrClass::Private;
    }
    if octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31 {
        return AddrClass::Private;
    }
    if octets[0] == 192 && octets[1] == 168 {
        return AddrClass::Private;
    }

    // Cloud metadata endpoints (check before ranges that might overlap)
    // AWS: 169.254.169.254
    if octets == [169, 254, 169, 254] {
        return AddrClass::CloudMetadata;
    }
    // GCP (secondary): 169.254.170.2
    if octets == [169, 254, 170, 2] {
        return AddrClass::CloudMetadata;
    }
    // Alibaba: 100.100.100.200
    if octets == [100, 100, 100, 200] {
        return AddrClass::CloudMetadata;
    }
    // Azure: 168.63.129.16
    if octets == [168, 63, 129, 16] {
        return AddrClass::CloudMetadata;
    }
    // Google Cloud alternative: 192.0.0.192
    if octets == [192, 0, 0, 192] {
        return AddrClass::CloudMetadata;
    }

    // Carrier-grade NAT: 100.64.0.0/10
    if octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127 {
        return AddrClass::CarrierGradeNat;
    }

    // Link-local: 169.254.0.0/16
    if octets[0] == 169 && octets[1] == 254 {
        return AddrClass::LinkLocal;
    }

    // Multicast: 224.0.0.0/4
    if octets[0] >= 224 && octets[0] <= 239 {
        return AddrClass::Multicast;
    }

    // Broadcast: 255.255.255.255/32
    if octets == [255, 255, 255, 255] {
        return AddrClass::Broadcast;
    }

    // Documentation: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
        return AddrClass::Documentation;
    }
    if octets[0] == 198 && octets[1] == 51 && octets[2] == 100 {
        return AddrClass::Documentation;
    }
    if octets[0] == 203 && octets[1] == 0 && octets[2] == 113 {
        return AddrClass::Documentation;
    }

    // Benchmarking: 198.18.0.0/15
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return AddrClass::Benchmarking;
    }

    AddrClass::Public
}

/// Classify an IPv6 address.
fn classify_ipv6(addr: Ipv6Addr) -> AddrClass {
    // Loopback: ::1
    if addr == Ipv6Addr::LOCALHOST {
        return AddrClass::Loopback;
    }

    // Unspecified: ::
    if addr == Ipv6Addr::UNSPECIFIED {
        return AddrClass::Unspecified;
    }

    let segments = addr.segments();

    // Link-local: fe80::/10
    if segments[0] >= 0xfe80 && segments[0] <= 0xfebf {
        return AddrClass::LinkLocal;
    }

    // Unique-local: fc00::/7
    if segments[0] >= 0xfc00 && segments[0] <= 0xfdff {
        return AddrClass::UniqueLocal;
    }

    // Cloud metadata: fd00:ec2::254 (AWS)
    if segments[0] == 0xfd00
        && segments[1] == 0xec2
        && segments[2..].iter().all(|&s| s == 0)
        && addr.segments()[7] == 254
    {
        return AddrClass::CloudMetadata;
    }

    // Multicast: ff00::/8
    if segments[0] >= 0xff00 {
        return AddrClass::Multicast;
    }

    // Documentation: 2001:db8::/32
    if segments[0] == 0x2001 && segments[1] == 0xdb8 {
        return AddrClass::Documentation;
    }

    // IPv4-mapped IPv6: ::ffff:0:0/96
    if segments[0..6] == [0, 0, 0, 0, 0, 0xffff] {
        let ipv4 = Ipv4Addr::new(
            ((segments[6] >> 8) & 0xff) as u8,
            (segments[6] & 0xff) as u8,
            ((segments[7] >> 8) & 0xff) as u8,
            (segments[7] & 0xff) as u8,
        );
        let ipv4_class = classify_ipv4(ipv4);
        return if ipv4_class == AddrClass::Public {
            AddrClass::Public
        } else {
            AddrClass::Ipv4Mapped
        };
    }

    // IPv4-compatible IPv6: ::/96 (deprecated)
    if segments[0..6] == [0, 0, 0, 0, 0, 0] {
        let ipv4 = Ipv4Addr::new(
            ((segments[6] >> 8) & 0xff) as u8,
            (segments[6] & 0xff) as u8,
            ((segments[7] >> 8) & 0xff) as u8,
            (segments[7] & 0xff) as u8,
        );
        let ipv4_class = classify_ipv4(ipv4);
        return if ipv4_class == AddrClass::Public {
            AddrClass::Public
        } else {
            AddrClass::Ipv4Compatible
        };
    }

    // 6to4: 2002::/16
    if segments[0] == 0x2002 {
        return AddrClass::Benchmarking;
    }

    // Teredo: 2001::/32
    if segments[0] == 0x2001 && segments[1] == 0 {
        return AddrClass::Benchmarking;
    }

    AddrClass::Public
}

/// The cloud metadata addresses, from `metadata_hosts.toml` (one entry per provider).
fn metadata_hosts() -> &'static [IpAddr] {
    #[derive(serde::Deserialize)]
    struct Entry {
        addr: IpAddr,
    }
    #[derive(serde::Deserialize)]
    struct File {
        host: Vec<Entry>,
    }
    static HOSTS: std::sync::OnceLock<Vec<IpAddr>> = std::sync::OnceLock::new();
    HOSTS.get_or_init(|| {
        toml::from_str::<File>(include_str!("metadata_hosts.toml"))
            .map(|f| f.host.into_iter().map(|e| e.addr).collect())
            .unwrap_or_default()
    })
}

/// The IPv4 address embedded in an IPv6 form that carries one, if any: IPv4-mapped
/// (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`), NAT64 (`64:ff9b::/96`) and 6to4
/// (`2002:aabb:ccdd::/48`). They are classified by what they point at.
fn embedded_ipv4(addr: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = addr.segments();
    let v4 = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return Some(v4(s[6], s[7]));
    }
    if s[..6] == [0; 6] && (s[6] != 0 || s[7] > 1) {
        return Some(v4(s[6], s[7]));
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return Some(v4(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    None
}

/// Classify an IP address according to its nature and security implications. Anything not
/// provably public is denied by default; embedded-IPv4 forms are judged by the embedded address.
pub fn classify(addr: IpAddr) -> AddrClass {
    if metadata_hosts().contains(&addr) {
        return AddrClass::CloudMetadata;
    }
    match addr {
        IpAddr::V4(ipv4) => classify_ipv4(ipv4),
        IpAddr::V6(ipv6) => match embedded_ipv4(ipv6) {
            Some(v4) => match classify(IpAddr::V4(v4)) {
                // A public IPv4 behind a transition form is still not a plain public host.
                AddrClass::Public => AddrClass::Other,
                class => class,
            },
            None => classify_ipv6(ipv6),
        },
    }
}

/// Attempt to parse numeric IPv4 spellings (decimal, octal, hex, shortened).
#[allow(dead_code)]
pub fn parse_numeric_ipv4(s: &str) -> Option<IpAddr> {
    // Decimal: 2130706433
    if let Ok(num) = s.parse::<u32>() {
        return Some(IpAddr::V4(Ipv4Addr::from(num)));
    }

    // Octal: 017700000001
    if s.starts_with('0') && s.len() > 1 && !s.contains('.') {
        if let Ok(num) = u32::from_str_radix(s, 8) {
            return Some(IpAddr::V4(Ipv4Addr::from(num)));
        }
    }

    // Hex: 0x7f.1 or 0x7f000001
    if s.starts_with("0x") || s.starts_with("0X") {
        let hex_part = &s[2..];
        // Try as full hex
        if let Ok(num) = u32::from_str_radix(&hex_part.replace('.', ""), 16) {
            return Some(IpAddr::V4(Ipv4Addr::from(num)));
        }
    }

    None
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::if_same_then_else)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_loopback() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            AddrClass::Loopback
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(127, 1, 2, 3))),
            AddrClass::Loopback
        );
        assert_eq!(
            classify(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            AddrClass::Loopback
        );
    }

    #[test]
    fn test_classify_private() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            AddrClass::Private
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))),
            AddrClass::Private
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))),
            AddrClass::Private
        );
    }

    #[test]
    fn test_classify_cloud_metadata() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))),
            AddrClass::CloudMetadata
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(100, 100, 100, 200))),
            AddrClass::CloudMetadata
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(168, 63, 129, 16))),
            AddrClass::CloudMetadata
        );
    }

    #[test]
    fn test_classify_public() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))),
            AddrClass::Public
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))),
            AddrClass::Public
        );
    }

    #[test]
    fn test_classify_link_local() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))),
            AddrClass::LinkLocal
        );
    }

    #[test]
    fn test_classify_multicast() {
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))),
            AddrClass::Multicast
        );
        assert_eq!(
            classify(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 255))),
            AddrClass::Multicast
        );
    }

    #[test]
    fn test_parse_numeric_ipv4_decimal() {
        let result = parse_numeric_ipv4("2130706433");
        assert_eq!(result, Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn test_parse_numeric_ipv4_octal() {
        let result = parse_numeric_ipv4("017700000001");
        assert_eq!(result, Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn test_parse_numeric_ipv4_hex() {
        let result = parse_numeric_ipv4("0x7f000001");
        assert_eq!(result, Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn test_addr_class_is_denied() {
        assert!(AddrClass::Loopback.is_denied_by_default());
        assert!(AddrClass::Private.is_denied_by_default());
        assert!(AddrClass::CloudMetadata.is_denied_by_default());
        assert!(!AddrClass::Public.is_denied_by_default());
    }

    #[test]
    fn metadata_list_comes_from_the_data_file() {
        for a in [
            "169.254.169.254",
            "169.254.170.2",
            "168.63.129.16",
            "100.100.100.200",
            "192.0.0.192",
            "fd00:ec2::254",
        ] {
            assert_eq!(
                classify(a.parse().unwrap()),
                AddrClass::CloudMetadata,
                "{a}"
            );
        }
    }

    #[test]
    fn embedded_ipv4_forms_are_judged_by_the_embedded_address() {
        for (a, want) in [
            ("::ffff:127.0.0.1", AddrClass::Loopback),
            ("::ffff:10.0.0.1", AddrClass::Private),
            ("::ffff:169.254.169.254", AddrClass::CloudMetadata),
            ("::10.0.0.1", AddrClass::Private),
            ("64:ff9b::7f00:1", AddrClass::Loopback),
            ("2002:a00:1::1", AddrClass::Private), // 6to4 of 10.0.0.1
        ] {
            assert_eq!(classify(a.parse().unwrap()), want, "{a}");
        }
        // A public IPv4 inside a transition form is never plain `Public`.
        assert!(classify("::ffff:8.8.8.8".parse().unwrap()).is_denied_by_default());
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn classify_never_panics_and_is_total(a in any::<u128>(), b in any::<u32>()) {
                let _ = classify(IpAddr::V6(Ipv6Addr::from(a)));
                let _ = classify(IpAddr::V4(Ipv4Addr::from(b)));
            }

            #[test]
            fn every_address_is_public_or_denied_by_default(b in any::<u32>()) {
                let class = classify(IpAddr::V4(Ipv4Addr::from(b)));
                prop_assert!(class == AddrClass::Public || class.is_denied_by_default());
            }
        }
    }
}
