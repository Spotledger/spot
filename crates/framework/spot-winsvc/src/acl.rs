//! ACL rules for the two directories the host owns (brief section "Directories"):
//! `%ProgramFiles%\Spotledger` (binaries) and `%ProgramData%\Spotledger` (data/config).
//!
//! The actual `SetNamedSecurityInfo` call is `cfg(windows)` (see `crate::service::real`,
//! where present); this module is the portable, Linux-tested part: which trustee gets
//! which right, and the textual access-control-entry list a real Windows apply (or an
//! `icacls`-style diagnostic) renders from it. No ACL is ever applied here.

use std::fmt;

/// A trustee an access-control entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trustee {
    /// The built-in `Administrators` alias.
    Administrators,
    /// The `SYSTEM` account.
    System,
    /// The built-in `Users` alias.
    Users,
    /// The service's own virtual account, `NT SERVICE\SpotledgerServer`.
    ServiceAccount,
}

impl Trustee {
    /// The well-known Windows account name an ACE names this trustee with.
    #[must_use]
    pub fn account_name(&self) -> &'static str {
        match self {
            Trustee::Administrators => "BUILTIN\\Administrators",
            Trustee::System => "NT AUTHORITY\\SYSTEM",
            Trustee::Users => "BUILTIN\\Users",
            Trustee::ServiceAccount => "NT SERVICE\\SpotledgerServer",
        }
    }
}

impl fmt::Display for Trustee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.account_name())
    }
}

/// What an access-control entry grants (or, for [`Right::None`], explicitly denies by
/// omission — the brief's "Users none").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    /// Full control (take ownership, change permissions, read, write, delete, execute).
    FullControl,
    /// Read, write, delete-own-files, execute, but not take ownership or change ACLs.
    Modify,
    /// Read, write, execute.
    Write,
    /// Read and execute only.
    Read,
    /// No access at all: the trustee is never listed, so it inherits nothing.
    None,
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Right::FullControl => "FullControl",
            Right::Modify => "Modify",
            Right::Write => "Write",
            Right::Read => "Read",
            Right::None => "None",
        };
        write!(f, "{s}")
    }
}

/// One access-control entry: `trustee` gets `right`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclRule {
    /// Who the rule applies to.
    pub trustee: Trustee,
    /// What it grants.
    pub right: Right,
}

/// `%ProgramFiles%\Spotledger` (brief: "Administrators/SYSTEM write, service account
/// read"). `Right::None` rules are never emitted by [`to_acl_text`] — the directory's
/// inherited protection (no inheritance from a world-writable parent) is assumed, which
/// the real apply enforces by first disabling inheritance (see `crate::service::real`).
#[must_use]
pub fn program_files_acl() -> Vec<AclRule> {
    vec![
        AclRule {
            trustee: Trustee::Administrators,
            right: Right::FullControl,
        },
        AclRule {
            trustee: Trustee::System,
            right: Right::FullControl,
        },
        AclRule {
            trustee: Trustee::ServiceAccount,
            right: Right::Read,
        },
    ]
}

/// `%ProgramData%\Spotledger` (brief: "service account modify, Administrators full,
/// Users none").
#[must_use]
pub fn program_data_acl() -> Vec<AclRule> {
    vec![
        AclRule {
            trustee: Trustee::ServiceAccount,
            right: Right::Modify,
        },
        AclRule {
            trustee: Trustee::Administrators,
            right: Right::FullControl,
        },
        AclRule {
            trustee: Trustee::Users,
            right: Right::None,
        },
    ]
}

/// Renders `rules` as the `icacls`-style text a real apply logs (never the ACL binary
/// form, so a log line can safely carry it — there is no secret in a permission grant).
/// A [`Right::None`] rule is omitted: the trustee simply does not appear, which is how an
/// explicit "no access" is expressed on a protected (non-inheriting) directory.
#[must_use]
pub fn to_acl_text(rules: &[AclRule]) -> String {
    rules
        .iter()
        .filter(|rule| rule.right != Right::None)
        .map(|rule| format!("{}:({})", rule.trustee, rule.right))
        .collect::<Vec<_>>()
        .join(" ")
}

/// SHA-1 of `data` (FIPS 180-4). Only used to derive a virtual service account's SID
/// (below), where Windows itself mandates SHA-1; not a security primitive here.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The string SID (`S-1-5-80-...`) of the virtual service account `NT SERVICE\<name>`.
///
/// Windows derives it from the service name alone: SHA-1 of the upper-cased name in
/// UTF-16LE, read as five little-endian 32-bit sub-authorities under `S-1-5-80`. It
/// therefore exists, and can be put in an ACE, before the service is installed. Naming
/// the account instead fails with `ERROR_NONE_MAPPED` (1332) whenever the service is
/// not registered on the machine, e.g. on a test host.
#[must_use]
pub fn service_sid_string(service_name: &str) -> String {
    let bytes: Vec<u8> = service_name
        .to_uppercase()
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let digest = sha1(&bytes);
    let parts: Vec<String> = digest
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c).to_string())
        .collect();
    format!("S-1-5-80-{}", parts.join("-"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn program_files_grants_write_to_admins_and_system_and_read_to_the_service() {
        let rules = program_files_acl();
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::Administrators)
                .map(|r| r.right),
            Some(Right::FullControl)
        );
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::System)
                .map(|r| r.right),
            Some(Right::FullControl)
        );
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::ServiceAccount)
                .map(|r| r.right),
            Some(Right::Read)
        );
        assert!(!rules.iter().any(|r| r.trustee == Trustee::Users));
    }

    #[test]
    fn program_data_grants_modify_to_the_service_full_to_admins_and_nothing_to_users() {
        let rules = program_data_acl();
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::ServiceAccount)
                .map(|r| r.right),
            Some(Right::Modify)
        );
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::Administrators)
                .map(|r| r.right),
            Some(Right::FullControl)
        );
        assert_eq!(
            rules
                .iter()
                .find(|r| r.trustee == Trustee::Users)
                .map(|r| r.right),
            Some(Right::None)
        );
    }

    #[test]
    fn to_acl_text_omits_none_rights_and_names_the_service_account() {
        let text = to_acl_text(&program_data_acl());
        assert!(text.contains("NT SERVICE\\SpotledgerServer:(Modify)"));
        assert!(text.contains("BUILTIN\\Administrators:(FullControl)"));
        assert!(!text.contains("Users"));
    }

    #[test]
    fn program_files_acl_never_grants_users_or_the_service_account_write() {
        let rules = program_files_acl();
        assert!(
            rules
                .iter()
                .all(|r| r.trustee != Trustee::ServiceAccount || r.right == Right::Read)
        );
    }

    #[test]
    fn sha1_matches_the_published_test_vectors() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn service_sid_matches_the_well_known_trustedinstaller_sid() {
        assert_eq!(
            service_sid_string("TrustedInstaller"),
            "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        );
    }

    #[test]
    fn service_sid_ignores_the_case_of_the_name() {
        assert_eq!(
            service_sid_string("SpotledgerServer"),
            service_sid_string("SPOTLEDGERSERVER")
        );
    }
}
