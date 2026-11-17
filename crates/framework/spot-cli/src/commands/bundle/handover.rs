//! The hand-over file, the environment record, the key policy and the direction rule.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spot_bundle_signing::{
    KeyPair, PublicKey, RevocationList, SignedCertificate, TrustAnchor, TrustedRoot, sign_bundle,
};
use spot_custom_bundle::{Bundle, Limits, SectionRegistry};
use spot_environments::Role;

/// The hand-over file format name.
pub const HANDOVER_FORMAT: &str = "spot-bundle-handover";

/// The hand-over header version.
pub const HANDOVER_VERSION: u32 = 1;

/// The largest header line a hand-over file may carry.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The file that records a site's environment kind and key policy.
pub const ENV_FILE: &str = "environment.toml";

/// The directory beside the site file that holds the key policy copies.
pub const ENV_DIR: &str = "environment";

/// The directory beside the site file that holds hand-over files waiting for their plan.
pub const PENDING_DIR: &str = "bundle-pending";

// --- environments ------------------------------------------------------------------------------

/// The stored environment record of a site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvRecord {
    pub kind: String,
    #[serde(default)]
    pub trust_anchor: Option<String>,
    #[serde(default)]
    pub revocations: Option<String>,
}

/// The role a kind name stands for.
#[must_use]
pub fn role_of(name: &str) -> Option<Role> {
    match name {
        "dev" | "development" => Some(Role::Dev),
        "test" => Some(Role::Test),
        "prod" | "production" => Some(Role::Prod),
        _ => None,
    }
}

/// The rung after `role`; `None` for production, the end of the ladder.
#[must_use]
pub fn next_role(role: Role) -> Option<Role> {
    Role::ALL
        .iter()
        .copied()
        .find(|r| r.rank() == role.rank() + 1)
}

/// Why a hand-over is refused. The reason names the check that failed, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused ({}): {}", self.reason, self.message)
    }
}

/// Transport goes one rung up the ladder: dev to test, test to prod. Anything else is refused,
/// a bundle from production to development included (that path is the sandbox's masking).
///
/// # Errors
/// A [`Refusal`] with reason `direction`.
pub fn check_direction(from: Role, to: Role) -> Result<(), Refusal> {
    if next_role(from) == Some(to) {
        return Ok(());
    }
    let hint = if from == Role::Prod {
        "; production data and customisations reach development only through a masked sandbox"
    } else {
        ""
    };
    Err(Refusal::new(
        "direction",
        format!(
            "a bundle moves one rung up the ladder (dev to test, test to prod); {} to {} is not allowed{hint}",
            from.name(),
            to.name()
        ),
    ))
}

pub fn site_dir(root: &Path, site: &str) -> PathBuf {
    root.join("sites").join(site)
}

/// Reads a site's environment record; `None` when the site is not linked.
///
/// # Errors
/// [`spot_cli::Error`] when the file exists and does not parse.
pub fn read_env(dir: &Path) -> spot_cli::Result<Option<EnvRecord>> {
    let path = dir.join(ENV_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|_| spot_cli::Error("the environment record does not parse".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn write_private(path: &Path, bytes: &[u8], overwrite: bool) -> spot_cli::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

// --- hex and key material ----------------------------------------------------------------------

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// The 32 seed bytes of a signing key file (64 hex characters).
///
/// # Errors
/// A message that never contains the file's content.
pub fn read_seed(path: &Path) -> Result<[u8; 32], String> {
    let text = std::fs::read_to_string(path).map_err(|_| "the signing key file cannot be read")?;
    let bytes = hex_decode(&text).ok_or("the signing key file is not 64 hexadecimal characters")?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| "the signing key file is not 64 hexadecimal characters".to_owned())
}

/// The key policy of an environment: who may sign bundles for it and what is revoked.
pub struct Trust {
    pub anchor: TrustAnchor,
    pub revocations: RevocationList,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RootDoc {
    public_key: String,
    valid_from: i64,
    #[serde(default)]
    valid_to: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorDoc {
    roots: Vec<RootDoc>,
}

/// Parses a trust anchor file: `{"roots":[{"public_key":"<64 hex>","valid_from":0,"valid_to":null}]}`.
///
/// # Errors
/// A message naming the problem, never a key.
pub fn parse_anchor(bytes: &[u8]) -> Result<TrustAnchor, String> {
    let doc: AnchorDoc =
        serde_json::from_slice(bytes).map_err(|_| "the trust anchor does not parse")?;
    if doc.roots.is_empty() {
        return Err("the trust anchor names no root".to_owned());
    }
    let mut roots = Vec::new();
    for root in doc.roots {
        let key = hex_decode(&root.public_key).ok_or("a trust anchor key is not hexadecimal")?;
        let public = PublicKey::from_bytes(&key).map_err(|_| "a trust anchor key is malformed")?;
        roots.push(TrustedRoot::new(public, root.valid_from, root.valid_to));
    }
    Ok(TrustAnchor::new(roots))
}

/// Loads the key policy a site's record names. A site without one has no policy, and a bundle
/// cannot be imported into it.
///
/// # Errors
/// A [`Refusal`] with reason `no_key_policy` or `key_policy_invalid`.
pub fn load_trust(dir: &Path, record: &EnvRecord) -> Result<Trust, Refusal> {
    let (Some(anchor), Some(revocations)) = (&record.trust_anchor, &record.revocations) else {
        return Err(Refusal::new(
            "no_key_policy",
            "the site's environment has no trust anchor and revocation list; \
             link it with --trust-anchor and --revocations",
        ));
    };
    let invalid = |what: &str| Refusal::new("key_policy_invalid", format!("{what} cannot be used"));
    let anchor_bytes = std::fs::read(dir.join(anchor)).map_err(|_| invalid("the trust anchor"))?;
    let revocation_bytes =
        std::fs::read(dir.join(revocations)).map_err(|_| invalid("the revocation list"))?;
    Ok(Trust {
        anchor: parse_anchor(&anchor_bytes).map_err(|_| invalid("the trust anchor"))?,
        revocations: RevocationList::from_json(&revocation_bytes)
            .map_err(|_| invalid("the revocation list"))?,
    })
}

// --- the hand-over file ------------------------------------------------------------------------

/// The JSON header line of a hand-over file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandOverHeader {
    pub format: String,
    pub version: u32,
    pub source_site: String,
    pub source_kind: String,
    pub target_kind: String,
    pub name: String,
    pub bundle_version: String,
    pub created_by: String,
    pub digest: String,
    pub bundle_len: u64,
    pub signed_at: i64,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub certificate: Option<String>,
}

/// The identity a signature binds: the two kinds, the name and who made it.
#[must_use]
pub fn transport_id(source_kind: &str, target_kind: &str, name: &str, created_by: &str) -> String {
    format!("custom-bundle:{source_kind}>{target_kind}:{name}:{created_by}")
}

impl HandOverHeader {
    pub fn identity(&self) -> String {
        transport_id(
            &self.source_kind,
            &self.target_kind,
            &self.name,
            &self.created_by,
        )
    }
}

/// Encodes a header and the archive into one file.
///
/// # Errors
/// A message when the header does not serialise.
pub fn encode_handover(header: &HandOverHeader, bundle: &[u8]) -> Result<Vec<u8>, String> {
    let mut line = serde_json::to_vec(header).map_err(|_| "the header does not serialise")?;
    line.push(b'\n');
    line.extend_from_slice(bundle);
    Ok(line)
}

/// Splits a hand-over file into its header and the archive bytes.
///
/// # Errors
/// A [`Refusal`] with reason `unreadable`.
pub fn decode_handover(bytes: &[u8]) -> Result<(HandOverHeader, &[u8]), Refusal> {
    let unreadable = |m: &str| Refusal::new("unreadable", m.to_owned());
    let end = bytes
        .iter()
        .take(MAX_HEADER_BYTES)
        .position(|b| *b == b'\n')
        .ok_or_else(|| unreadable("the file has no header line"))?;
    let header: HandOverHeader = serde_json::from_slice(&bytes[..end])
        .map_err(|_| unreadable("the header line does not parse"))?;
    if header.format != HANDOVER_FORMAT || header.version != HANDOVER_VERSION {
        return Err(unreadable(
            "the file is not a hand-over file of this version",
        ));
    }
    let body = &bytes[end + 1..];
    if u64::try_from(body.len()).ok() != Some(header.bundle_len) {
        return Err(unreadable("the archive length does not match the header"));
    }
    Ok((header, body))
}

pub fn parse_digest<T: std::str::FromStr>(hex: &str) -> Result<T, Refusal> {
    hex.parse().map_err(|_| {
        Refusal::new(
            "digest_mismatch",
            "the digest is not 64 hexadecimal characters",
        )
    })
}

/// Opens an archive under the default limits and the default registry.
pub fn open_bundle(bytes: &[u8], registry: &SectionRegistry) -> Result<Bundle, Refusal> {
    Bundle::open(bytes, &Limits::default(), registry, false).map_err(|errors| {
        let code = errors.first().map_or("bad_archive", |e| e.code());
        Refusal::new(
            "invalid_bundle",
            format!("the archive is not a readable customisation bundle ({code})"),
        )
    })
}

/// Signs a bundle for one hop and returns the file's bytes.
///
/// # Errors
/// A message; never key material.
#[allow(clippy::too_many_arguments)]
pub fn make_handover(
    bundle: &Bundle,
    source_site: &str,
    source: Role,
    target: Role,
    created_by: &str,
    key: &KeyPair,
    certificate: &SignedCertificate,
    signed_at: i64,
) -> Result<Vec<u8>, String> {
    check_direction(source, target).map_err(|r| r.to_string())?;
    let header = bundle.header();
    let identity = transport_id(source.name(), target.name(), &header.name, created_by);
    let hex = bundle.digest().to_hex();
    let digest = parse_digest(&hex).map_err(|r| r.to_string())?;
    let signature = sign_bundle(
        &digest,
        &identity,
        &header.bundle_version,
        certificate,
        key,
        signed_at,
    )
    .map_err(|e| e.to_string())?;
    let archive = bundle.to_bytes();
    let out = HandOverHeader {
        format: HANDOVER_FORMAT.to_owned(),
        version: HANDOVER_VERSION,
        source_site: source_site.to_owned(),
        source_kind: source.name().to_owned(),
        target_kind: target.name().to_owned(),
        name: header.name.clone(),
        bundle_version: header.bundle_version.clone(),
        created_by: created_by.to_owned(),
        digest: hex,
        bundle_len: u64::try_from(archive.len()).map_err(|_| "the bundle is too large")?,
        signed_at,
        signature: Some(signature.to_json()),
        certificate: Some(certificate.to_json()),
    };
    encode_handover(&out, &archive)
}
