//! The advertised record and its mDNS encoding.
//!
//! The record is deliberately small: it says where the application endpoint is and how
//! to talk to it safely, and nothing else. It carries **no tenant name, company name,
//! user name or database detail** unless the administrator sets `reveal_site_names =
//! true` — and then only display names the administrator typed for that purpose
//! (ADR-058: tenants are the unit of placement; a server is discovered anonymously).
//! A test below proves the default encoding contains none of the forbidden fields and
//! stays within the mDNS sizes (255 bytes per TXT entry, 1300 bytes in total).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use crate::error::DiscoveryError;

/// The dotted version of the discovery protocol itself, bumped when the TXT grammar
/// changes in a way an old client cannot read. `product_version` below is the server's
/// own version, kept separate on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProtocolVersion(pub u32);

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The current protocol version every advertisement carries.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

/// The port Spotledger servers use unless configured otherwise. The host wiring task
/// owns the real policy; discovery only needs a default for `parse_manual` inputs
/// without a port.
pub const DEFAULT_APP_PORT: u16 = 8080;

/// The fingerprint of the server's TLS key: the first 16 bytes of SHA-256 over the
/// DER-encoded public key, hex-encoded, lower-case (128 bits defeat preimages long
/// enough; the pairing MAC binds the full key bytes anyway).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub String);

// Trust-store file form: serde on the record types only.
impl serde::Serialize for Fingerprint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Fingerprint {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self(s))
    }
}

impl Fingerprint {
    /// Computes the fingerprint over raw public-key bytes (`sha2` is already a
    /// dependency).
    #[must_use]
    pub fn from_public_key_der(der: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(der);
        let mut hex = String::with_capacity(32);
        for b in &d[..16] {
            hex.push(HEX[(b >> 4) as usize]);
            hex.push(HEX[(b & 0x0f) as usize]);
        }
        Self(hex)
    }

    /// The stored hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

const HEX: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// Whether the endpoint speaks TLS, and of which kind. The trust logic in [`crate::
/// trust`] decides what each mode means for the connection a client makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TlsMode {
    /// No TLS: the connection is plaintext and must be shown as such to the user.
    Off,
    /// TLS with a self-signed certificate: the first connection pairs on the key
    /// fingerprint.
    SelfSigned,
    /// TLS with a certificate a client system trusts (administrator-provided).
    Provided,
}

impl TlsMode {
    /// The TXT value.
    #[must_use]
    pub const fn as_txt(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SelfSigned => "self-signed",
            Self::Provided => "provided",
        }
    }

    /// Parses the TXT value.
    #[must_use]
    pub fn from_txt(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "self-signed" => Some(Self::SelfSigned),
            "provided" => Some(Self::Provided),
            _ => None,
        }
    }

    /// True when the wire is plaintext and the client must surface an
    /// [`Unprotected`](crate::trust::Unprotected) warning.
    #[must_use]
    pub const fn is_unprotected(self) -> bool {
        matches!(self, Self::Off)
    }
}

/// The service instance identity: the mDNS instance name, lowercase, RFC 6763 label.
///
/// **This, not the IP address, is what a client remembers.** Addresses change with
/// DHCP; reinstalling the server on a new box keeps the same instance and therefore the
/// same identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServerId(pub String);

// Trust-store file form: serde on the record types only.
impl serde::Serialize for ServerId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for ServerId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self(s))
    }
}

impl ServerId {
    /// Validates the instance label: 1–63 chars, ASCII letters, digits and hyphen,
    /// beginning and ending with a letter or digit — the shape mDNS host and instance
    /// labels must have.
    ///
    /// # Errors
    /// [`DiscoveryError::RecordInvalid`] when the label is out of shape.
    pub fn new(name: &str) -> Result<Self, DiscoveryError> {
        let ok = !name.is_empty()
            && name.len() <= 63
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            && name
                .bytes()
                .next_back()
                .is_some_and(|b| b.is_ascii_alphanumeric());
        if !ok {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the service instance name must be 1-63 ASCII letters, digits or \
                         hyphens, starting and ending with a letter or digit"
                    .to_string(),
            });
        }
        Ok(Self(name.to_ascii_lowercase()))
    }

    /// The stored label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Display names the administrator may publish beside an otherwise anonymous record,
/// only when `reveal_site_names` is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdvertisedSiteNames {
    /// The site name the administrator wants visible in the client picker, or none.
    pub site: Option<String>,
    /// A longer line (branch, floor) the administrator typed for that purpose, or none.
    pub description: Option<String>,
}

/// The record a server advertises and a client reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    /// The stable service instance (`ServerId`): the identity a client selects and
    /// remembers, independent of addresses.
    pub service_instance: ServerId,
    /// The TCP port of the application endpoint.
    pub port: u16,
    /// The discovery protocol version.
    pub protocol_version: ProtocolVersion,
    /// Whether and which TLS the endpoint speaks.
    pub tls: TlsMode,
    /// The key fingerprint when TLS is not off.
    pub server_key_fingerprint: Option<Fingerprint>,
    /// The server's own product version (informational).
    pub product_version: String,
    /// Administrator-revealed names, present only when `reveal_site_names` is on.
    pub site_names: AdvertisedSiteNames,
}

impl Advertisement {
    /// The TXT keys. The nine keys are all at most 8 characters: mDNS-SD accepts keys
    /// up to 9 (the trailing `=` in the padded form takes the tenth byte), and mdns-sd
    /// rejects longer ones.
    ///
    /// # Errors
    /// [`DiscoveryError::RecordInvalid`] when the record cannot be encoded at all
    /// (fingerprint with TLS off, version too long, revealed name too long).
    pub fn to_txt(&self) -> Result<Vec<(String, String)>, DiscoveryError> {
        if self.tls == TlsMode::Off && self.server_key_fingerprint.is_some() {
            return Err(DiscoveryError::RecordInvalid {
                reason: "a fingerprint is set while TLS is off".to_string(),
            });
        }
        if self.product_version.len() > PRODUCT_VERSION_MAX {
            return Err(DiscoveryError::RecordInvalid {
                reason: format!("the product version must be at most {PRODUCT_VERSION_MAX} bytes"),
            });
        }
        let mut txt = vec![
            ("instance".to_string(), self.service_instance.0.clone()),
            ("port".to_string(), self.port.to_string()),
            ("pv".to_string(), self.protocol_version.0.to_string()),
            ("tls".to_string(), self.tls.as_txt().to_string()),
            ("ver".to_string(), self.product_version.clone()),
        ];
        if let Some(fp) = &self.server_key_fingerprint {
            if fp.0.len() != FINGERPRINT_HEX_LEN || !fp.0.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(DiscoveryError::RecordInvalid {
                    reason: "the fingerprint must be 32 lower-case hex characters".to_string(),
                });
            }
            if !fp
                .0
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            {
                return Err(DiscoveryError::RecordInvalid {
                    reason: "the fingerprint must be lower-case hex".to_string(),
                });
            }
            txt.push(("fp".to_string(), fp.0.clone()));
        }
        if !self.site_names.site.as_ref().is_none_or(String::is_empty) {
            let site = self.site_names.site.as_deref().unwrap_or_default();
            if site.len() > SITE_NAME_MAX {
                return Err(DiscoveryError::RecordInvalid {
                    reason: format!("the revealed site name must be at most {SITE_NAME_MAX} bytes"),
                });
            }
            txt.push(("site".to_string(), site.to_string()));
        }
        if !self
            .site_names
            .description
            .as_ref()
            .is_none_or(String::is_empty)
        {
            let desc = self.site_names.description.as_deref().unwrap_or_default();
            if desc.len() > SITE_NAME_MAX {
                return Err(DiscoveryError::RecordInvalid {
                    reason: format!(
                        "the revealed description must be at most {SITE_NAME_MAX} bytes"
                    ),
                });
            }
            txt.push(("sitedesc".to_string(), desc.to_string()));
        }
        for (k, v) in &txt {
            // The check mdns-sd applies too: key + '=' + value < 256, so the on-wire
            // TXT entry stays within the 255-byte limit of RFC 6763 §6.1.
            if k.len() + 1 + v.len() > 255 {
                return Err(DiscoveryError::RecordInvalid {
                    reason: format!("TXT entry '{k}' exceeds the 255-byte limit"),
                });
            }
        }
        Ok(txt)
    }

    /// Decodes TXT pairs into a record, refusing anything malformed, unknown-versioned
    /// or forbidden. Unknown keys are ignored (forward compatibility).
    ///
    /// # Errors
    /// [`DiscoveryError::RecordInvalid`] with the reason.
    pub fn from_txt(txt: &[(String, String)]) -> Result<Self, DiscoveryError> {
        let mut instance = None;
        let mut port = None;
        let mut pv = None;
        let mut tls = None;
        let mut fingerprint = None;
        let mut version = None;
        let mut site = None;
        let mut sitedesc = None;
        for (k, v) in txt {
            match k.as_str() {
                "instance" => instance = Some(v.clone()),
                "port" => {
                    let p = v
                        .parse::<u16>()
                        .map_err(|_| DiscoveryError::RecordInvalid {
                            reason: "the advertised port is not a number".to_string(),
                        })?;
                    if p == 0 {
                        return Err(DiscoveryError::RecordInvalid {
                            reason: "the advertised port is 0".to_string(),
                        });
                    }
                    port = Some(p);
                }
                "pv" => {
                    let n = v
                        .parse::<u32>()
                        .map_err(|_| DiscoveryError::RecordInvalid {
                            reason: "the advertised protocol version is not a number".to_string(),
                        })?;
                    pv = Some(ProtocolVersion(n));
                }
                "tls" => {
                    tls = Some(TlsMode::from_txt(v).ok_or_else(|| {
                        DiscoveryError::RecordInvalid {
                            reason:
                                "the advertised TLS mode is not one of off, self-signed, provided"
                                    .to_string(),
                        }
                    })?);
                }
                "fp" => fingerprint = Some(Fingerprint(v.clone())),
                "ver" => version = Some(v.clone()),
                "site" => site = Some(v.clone()),
                "sitedesc" => sitedesc = Some(v.clone()),
                _ => {}
            }
        }
        let Some(instance) = instance else {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record has no instance key".to_string(),
            });
        };
        let service_instance = ServerId::new(&instance)?;
        let Some(port) = port else {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record has no port key".to_string(),
            });
        };
        let Some(pv) = pv else {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record has no pv key".to_string(),
            });
        };
        if pv != PROTOCOL_VERSION {
            return Err(DiscoveryError::RecordInvalid {
                reason: format!(
                    "the record speaks protocol version {pv}, this build reads {}",
                    PROTOCOL_VERSION.0
                ),
            });
        }
        let Some(tls) = tls else {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record has no tls key".to_string(),
            });
        };
        if tls == TlsMode::Off && fingerprint.is_some() {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record carries a fingerprint while TLS is off".to_string(),
            });
        }
        if tls != TlsMode::Off && fingerprint.is_none() {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the record advertising TLS carries no fingerprint".to_string(),
            });
        }
        if let Some(fp) = &fingerprint
            && (fp.0.len() != FINGERPRINT_HEX_LEN
                || !fp.0.bytes().all(|b| b.is_ascii_hexdigit())
                || fp.0.bytes().any(|b| b.is_ascii_uppercase()))
        {
            return Err(DiscoveryError::RecordInvalid {
                reason: "the fingerprint must be 32 lower-case hex characters".to_string(),
            });
        }
        if let Some(name) = &site
            && name.len() > SITE_NAME_MAX
        {
            return Err(DiscoveryError::RecordInvalid {
                reason: format!("the revealed site name exceeds {SITE_NAME_MAX} bytes"),
            });
        }
        if let Some(desc) = &sitedesc
            && desc.len() > SITE_NAME_MAX
        {
            return Err(DiscoveryError::RecordInvalid {
                reason: format!("the revealed description exceeds {SITE_NAME_MAX} bytes"),
            });
        }
        Ok(Self {
            service_instance,
            port,
            protocol_version: pv,
            tls,
            server_key_fingerprint: fingerprint,
            product_version: version.unwrap_or_default(),
            site_names: AdvertisedSiteNames {
                site,
                description: sitedesc,
            },
        })
    }

    /// The default-config encoding: a test proves it contains none of the forbidden
    /// content and fits the mDNS size limits.
    ///
    /// # Errors
    /// [`DiscoveryError::RecordInvalid`] — only from the length guards.
    pub fn encode_default_for_test(
        service_instance: &ServerId,
        tls: TlsMode,
        fingerprint: Option<&Fingerprint>,
        product_version: &str,
    ) -> Result<Vec<(String, String)>, DiscoveryError> {
        Self {
            service_instance: service_instance.clone(),
            port: DEFAULT_APP_PORT,
            protocol_version: PROTOCOL_VERSION,
            tls,
            server_key_fingerprint: fingerprint.cloned(),
            product_version: product_version.to_string(),
            site_names: AdvertisedSiteNames::default(),
        }
        .to_txt()
    }
}

/// Maximum bytes of a product version string.
pub const PRODUCT_VERSION_MAX: usize = 40;
/// Maximum bytes of a revealed site name or description.
pub const SITE_NAME_MAX: usize = 80;
/// Hex length of a fingerprint.
pub const FINGERPRINT_HEX_LEN: usize = 32;

/// Configuration for the advertiser: what to advertise and where.
#[derive(Debug, Clone)]
pub struct AdvertiserConfig {
    /// The record to advertise.
    pub advertisement: Advertisement,
    /// Whether administrator-set display names are included. When false (the default
    /// in [`AdvertiserConfig::default_for_test`]) they are stripped before encoding.
    pub reveal_site_names: bool,
    /// Explicit interface addresses to advertise on. Empty means the default filter:
    /// private-range IPv4 plus IPv6 link-local and unique-site-local, never loopback
    /// and never a public address.
    pub interfaces: Vec<IpAddr>,
}

impl AdvertiserConfig {
    /// A config with the defaults the brief sets: no revealed names, default
    /// interface filter.
    #[must_use]
    pub fn default_for_test(advertisement: Advertisement) -> Self {
        Self {
            advertisement,
            reveal_site_names: false,
            interfaces: Vec::new(),
        }
    }

    /// The record as it will actually be encoded: with `reveal_site_names` off, the
    /// revealed names are stripped here, before encoding.
    #[must_use]
    pub fn effective_advertisement(&self) -> Advertisement {
        let mut adv = self.advertisement.clone();
        if !self.reveal_site_names {
            adv.site_names = AdvertisedSiteNames::default();
        }
        adv
    }
}

/// The default interface filter: which addresses may carry the advertisement.
///
/// - IPv4: RFC 1918 private ranges (10/8, 172.16/12, 192.168/16) and 169.254/16
///   link-local (a network with only that still deserves discovery).
/// - IPv6: `fe80::/10` link-local and `fc00::/7` unique-site-local.
/// - Never loopback (an advertisement there is invisible and pointless), never
///   anything else — in particular never a public address: a leak here is exactly the
///   "advertise on a public interface" the brief forbids.
#[must_use]
pub fn is_advertisable_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback() || v4.is_broadcast())
                && (o[0] == 10
                    || (o[0] == 172 && (o[1] & 0xf0) == 16)
                    || (o[0] == 192 && o[1] == 168)
                    || (o[0] == 169 && o[1] == 254))
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            !(v6.is_loopback() || s[0] == 0xff00)
                && (is_v6_link_local(&v6) || (s[0] & 0xfe00) == 0xfc00)
        }
    }
}

fn is_v6_link_local(v6: &Ipv6Addr) -> bool {
    let s = v6.segments();
    (s[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::{
        AdvertisedSiteNames, Advertisement, AdvertiserConfig, DEFAULT_APP_PORT,
        FINGERPRINT_HEX_LEN, Fingerprint, PROTOCOL_VERSION, ServerId, TlsMode,
        is_advertisable_address,
    };
    use crate::error::DiscoveryError;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn fp_hex(seed: u8) -> Fingerprint {
        // A deterministic stand-in for a real fingerprint in encoding tests.
        let hex = format!("{seed:02x}").repeat(16);
        assert_eq!(hex.len(), FINGERPRINT_HEX_LEN);
        Fingerprint(hex)
    }

    fn fp(seed: u8) -> String {
        format!("{seed:02x}").repeat(16)
    }

    fn plain_adv() -> Advertisement {
        Advertisement {
            service_instance: ServerId::new("shop-floor-1")
                .unwrap_or_else(|e| panic!("test instance rejected: {e:?}")),
            port: DEFAULT_APP_PORT,
            protocol_version: PROTOCOL_VERSION,
            tls: TlsMode::Off,
            server_key_fingerprint: None,
            product_version: "0.1.0".to_string(),
            site_names: AdvertisedSiteNames::default(),
        }
    }

    fn tls_adv() -> Advertisement {
        Advertisement {
            tls: TlsMode::SelfSigned,
            server_key_fingerprint: Some(fp_hex(0xab)),
            ..plain_adv()
        }
    }

    const FORBIDDEN_KEYS: [&str; 9] = [
        "tenant",
        "company",
        "user",
        "db",
        "database",
        "pg",
        "companyname",
        "tenantname",
        "sitename",
    ];

    #[test]
    fn default_record_has_no_forbidden_content_and_fits_limits() {
        for adv in [plain_adv(), tls_adv()] {
            let txt = adv
                .to_txt()
                .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
            let mut total = 0usize;
            for (k, v) in &txt {
                for forbidden in FORBIDDEN_KEYS {
                    assert_ne!(k.to_lowercase(), *forbidden, "forbidden key {k}");
                    assert!(
                        !v.to_lowercase().contains(forbidden),
                        "value of {k} contains the forbidden word {forbidden}: {v}"
                    );
                }
                let entry_len = k.len() + 1 + v.len();
                assert!(entry_len <= 255, "TXT entry '{k}' is {entry_len} bytes");
                total += entry_len;
            }
            assert!(
                total <= 1300,
                "the whole TXT record is {total} bytes, over the 1300 limit"
            );
            let defaults = Advertisement::encode_default_for_test(
                &adv.service_instance,
                adv.tls,
                adv.server_key_fingerprint.as_ref(),
                &adv.product_version,
            )
            .unwrap_or_else(|e| panic!("default encode failed: {e:?}"));
            assert_eq!(defaults, txt);
        }
    }

    #[test]
    fn encode_decode_round_trips() {
        let adv = tls_adv();
        let txt = adv
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        let back = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("decode failed: {e:?}"));
        assert_eq!(back, adv);
    }

    #[test]
    fn round_trip_off_mode() {
        let adv = plain_adv();
        let txt = adv
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        let back = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("decode failed: {e:?}"));
        assert_eq!(back, adv);
        assert!(back.tls.is_unprotected());
    }

    #[test]
    fn provided_tls_round_trips() {
        let mut adv = plain_adv();
        adv.tls = TlsMode::Provided;
        adv.server_key_fingerprint = Some(fp_hex(0xcd));
        let txt = adv
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        let back = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("decode failed: {e:?}"));
        assert_eq!(back, adv);
        assert!(!back.tls.is_unprotected());
    }

    #[test]
    fn fingerprint_with_tls_off_is_refused() {
        let mut adv = plain_adv();
        adv.server_key_fingerprint = Some(fp_hex(0x01));
        let res = adv.to_txt();
        assert!(
            matches!(res, Err(DiscoveryError::RecordInvalid { .. })),
            "expected RecordInvalid, got {res:?}"
        );
    }

    #[test]
    fn tls_without_fingerprint_is_refused() {
        let txt = vec![
            ("instance".to_string(), "x".to_string()),
            ("port".to_string(), "8080".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "self-signed".to_string()),
            ("ver".to_string(), "0.1.0".to_string()),
        ];
        let direct = Advertisement::from_txt(&txt);
        assert!(matches!(direct, Err(DiscoveryError::RecordInvalid { .. })));
    }

    #[test]
    fn decode_rejects_wrong_protocol_version() {
        let mut txt = plain_adv()
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        for pair in txt.iter_mut() {
            if pair.0 == "pv" {
                pair.1 = "999".to_string();
            }
        }
        let res = Advertisement::from_txt(&txt);
        assert!(matches!(res, Err(DiscoveryError::RecordInvalid { .. })));
    }

    #[test]
    fn decode_rejects_uppercase_fingerprint_and_short_fingerprint() {
        let mut txt = tls_adv()
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        for pair in txt.iter_mut() {
            if pair.0 == "fp" {
                pair.1 = fp(0xab).to_uppercase();
            }
        }
        assert!(matches!(
            Advertisement::from_txt(&txt),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let mut txt2 = txt.clone();
        for pair in txt2.iter_mut() {
            if pair.0 == "fp" {
                pair.1 = "abcd".to_string();
            }
        }
        assert!(matches!(
            Advertisement::from_txt(&txt2),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
    }

    #[test]
    fn decode_ignores_unknown_keys_and_missing_optional_ones() {
        let txt = vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("port".to_string(), "8123".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
        ];
        let adv = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("decode failed: {e:?}"));
        assert_eq!(adv.service_instance, ServerId("srv-1".to_string()));
        assert_eq!(adv.port, 8123);
        assert_eq!(adv.product_version, "");
        assert_eq!(adv.site_names, AdvertisedSiteNames::default());
    }

    #[test]
    fn decode_rejects_missing_required_keys_and_bad_values() {
        let missing_port = vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
        ];
        assert!(matches!(
            Advertisement::from_txt(&missing_port),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let bad_port = vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("port".to_string(), "notaport".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
        ];
        assert!(matches!(
            Advertisement::from_txt(&bad_port),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let zero_port = vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("port".to_string(), "0".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
        ];
        assert!(matches!(
            Advertisement::from_txt(&zero_port),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let bad_tls = vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("port".to_string(), "8080".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "quantum".to_string()),
        ];
        assert!(matches!(
            Advertisement::from_txt(&bad_tls),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let no_instance = vec![
            ("port".to_string(), "8080".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
        ];
        assert!(matches!(
            Advertisement::from_txt(&no_instance),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
    }

    #[test]
    fn server_id_rules() {
        assert!(matches!(
            ServerId::new(""),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        assert!(matches!(
            ServerId::new("-leading"),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        assert!(matches!(
            ServerId::new(&"a".repeat(64)),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        assert!(matches!(
            ServerId::new("has space"),
            Err(DiscoveryError::RecordInvalid { .. })
        ));
        let ok = ServerId::new("Shop-Floor-1").unwrap_or_else(|e| panic!(" rejected: {e:?}"));
        assert_eq!(ok.as_str(), "shop-floor-1");
    }

    #[test]
    fn reveal_site_names_off_strips_names() {
        let mut adv = plain_adv();
        adv.site_names = AdvertisedSiteNames {
            site: Some("Acme Trading".to_string()),
            description: Some("Front shop bench".to_string()),
        };
        let config = AdvertiserConfig::default_for_test(adv.clone());
        let effective = config.effective_advertisement();
        assert_eq!(effective.site_names, AdvertisedSiteNames::default());
        let txt = effective
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        assert!(txt.iter().all(|(k, _)| k != "site" && k != "sitedesc"));
        assert!(
            txt.iter()
                .all(|(_, v)| !v.contains("Acme") && !v.contains("bench"))
        );
    }

    #[test]
    fn reveal_site_names_on_includes_administrator_typed_names() {
        let mut adv = plain_adv();
        adv.site_names = AdvertisedSiteNames {
            site: Some("Acme Trading".to_string()),
            description: Some("Front shop bench".to_string()),
        };
        let config = AdvertiserConfig {
            reveal_site_names: true,
            ..AdvertiserConfig::default_for_test(adv.clone())
        };
        let txt = config
            .effective_advertisement()
            .to_txt()
            .unwrap_or_else(|e| panic!("encode failed: {e:?}"));
        assert!(txt.contains(&("site".to_string(), "Acme Trading".to_string())));
        assert!(txt.contains(&("sitedesc".to_string(), "Front shop bench".to_string())));
        // And it decodes again:
        let back = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("decode failed: {e:?}"));
        assert_eq!(back.site_names.site.as_deref(), Some("Acme Trading"));
    }

    #[test]
    fn tls_mode_txt_round_trip() {
        for (txt, mode) in [
            ("off", TlsMode::Off),
            ("self-signed", TlsMode::SelfSigned),
            ("provided", TlsMode::Provided),
        ] {
            assert_eq!(TlsMode::from_txt(txt), Some(mode));
            assert_eq!(mode.as_txt(), txt);
        }
        assert_eq!(TlsMode::from_txt("mutual"), None);
    }

    #[test]
    fn address_filter_never_allows_public_or_loopback() {
        let private: Vec<IpAddr> = ["10.1.2.3", "172.16.0.1", "172.31.255.254", "192.168.1.5"]
            .iter()
            .map(|s| s.parse().unwrap_or_else(|e| panic!("{e}")))
            .collect();
        for a in &private {
            assert!(is_advertisable_address(*a), "{a} must be advertisable");
        }
        // The boundary just outside every private range:
        for s in [
            "11.0.0.1",
            "172.32.0.1",
            "192.169.0.1",
            "8.8.8.8",
            "127.0.0.1",
        ] {
            let a: IpAddr = s.parse().unwrap_or_else(|e| panic!("{e}"));
            assert!(!is_advertisable_address(a), "{a} must not be advertisable");
        }
        let link_local: IpAddr = Ipv4Addr::new(169, 254, 9, 9).into();
        assert!(is_advertisable_address(link_local));
        let v6_ll: IpAddr = Ipv6Addr::new(0xfe80, 0, 0, 0, 1, 2, 3, 4).into();
        assert!(is_advertisable_address(v6_ll));
        let v6_ula: IpAddr = Ipv6Addr::new(0xfd00, 0, 0, 0, 1, 2, 3, 4).into();
        assert!(is_advertisable_address(v6_ula));
        let v6_public: IpAddr = Ipv6Addr::new(0x2600, 0, 0, 0, 0, 0, 0, 1).into();
        assert!(!is_advertisable_address(v6_public));
        let v6_lo: IpAddr = Ipv6Addr::LOCALHOST.into();
        assert!(!is_advertisable_address(v6_lo));
        // Multicast is never an interface address, but the guard refuses it anyway.
        let v6_mc: IpAddr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb).into();
        assert!(!is_advertisable_address(v6_mc));
    }

    #[test]
    fn fingerprint_derivation_is_hex_and_stable() {
        let a = Fingerprint::from_public_key_der(b"fake key bytes for the test");
        let b = Fingerprint::from_public_key_der(b"fake key bytes for the test");
        let c = Fingerprint::from_public_key_der(b"different key bytes");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.0.len(), 32);
        assert!(a.0.bytes().all(|ch| ch.is_ascii_digit() || ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }
}
