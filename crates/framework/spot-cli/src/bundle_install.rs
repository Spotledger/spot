//! Installing an app from a packed, signed bundle.
//!
//! `spot install-app <file>` treats a regular file as a bundle written by `spot-pack pack`. The
//! signature is checked **before** anything is written: the bundle is opened in memory by the
//! bundle reader (path rules, limits, manifest), the detached signature, the publisher
//! certificate, the site's trust roots and its revocation list go through the one verify the
//! installer calls (`spot_pack::verify_packed`), and only a bundle that passes is unpacked, into
//! a fresh temporary directory beside its final place, then renamed into the app folder. A
//! refusal changes nothing on disk.
//!
//! Beside the bundle `spot-pack pack --sign` writes `app.sig`; the publisher's certificate sits
//! next to it as `app.cert` (the JSON `spot-pack verify --certificate` reads). The roots and
//! the revocation list are the site's own (`[trust]` in `common.toml`): they never travel with
//! the bundle. Every refusal carries a stable `cli.bundle_*` (or `cli.directory_unsigned`) code,
//! and never a key, a signature or a payload.

use crate::Error;
use crate::appdir::{self, AppSource, PLATFORM};
use crate::apps;
use crate::config::{Loaded, Trust};
use crate::onetime::Issue;
use spot_bundle_signing::{
    BundleSignature, CertificateChain, PublicKey, RevocationList, SignedCertificate, SigningError,
    TrustAnchor, TrustedRoot,
};
use spot_pack::{PackOptions, install_policy, pathsafe::is_safe_rel_path, verify_packed};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The signature file `spot-pack pack --sign` writes next to the bundle.
pub const SIGNATURE_FILE: &str = concat!("app", ".sig");
/// The publisher certificate file, next to the bundle.
pub const CERTIFICATE_FILE: &str = concat!("app", ".cert");
/// The most a sidecar file (signature, certificate, roots, revocation list) may weigh.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

/// Why an install was refused before anything was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The stable code, for example `cli.bundle_signature_invalid`.
    pub code: &'static str,
    /// What happened, in plain words; never a key, a signature or a payload.
    pub message: String,
    /// The bundle's canonical digest (hex), when it was read.
    pub digest: Option<String>,
    /// The publisher the signature file names, when it parsed and the name is plain. It is the
    /// signature file's claim; only a verified bundle proves it.
    pub claimed_publisher: Option<String>,
}

impl Refusal {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            digest: None,
            claimed_publisher: None,
        }
    }

    /// The error the operator sees: the code first, then the words.
    #[must_use]
    pub fn into_error(self) -> Error {
        Error(format!("{}: {}", self.code, self.message))
    }
}

/// A bundle that verified and was unpacked.
pub struct Installed {
    /// The unpacked, validated app directory.
    pub app: AppSource,
    /// What the command prints about how it ran.
    pub note: String,
}

/// The file `wanted` names as a bundle, when it is one: a regular file, looked up as given and
/// under the configuration root.
#[must_use]
pub fn bundle_path(root: &Path, wanted: &str) -> Option<PathBuf> {
    [PathBuf::from(wanted), root.join(wanted)]
        .into_iter()
        .find(|p| p.is_file())
}

/// The refusal for a plain app directory (or a catalogue name) when the site requires signed
/// installs, or `None` when the site allows development installs.
#[must_use]
pub fn directory_policy(trust: &Trust) -> Option<Refusal> {
    trust.require_signed.then(|| {
        Refusal::new(
            "cli.directory_unsigned",
            "this site requires signed apps ([trust] require_signed); install a signed bundle, \
             or set `require_signed = false` in common.toml on a development site",
        )
    })
}

/// Verifies the bundle at `path` against the site's trust settings and unpacks it into the app
/// folder. Nothing is written before every check passed.
///
/// # Errors
/// A [`Refusal`] with its stable code; the disk is unchanged.
pub fn install(
    root: &Path,
    loaded: &Loaded,
    path: &Path,
    issue: &Issue,
) -> Result<Installed, Refusal> {
    let trust = &loaded.common.trust;
    let options = PackOptions::default();
    let size = std::fs::metadata(path)
        .map_err(|_| Refusal::new("cli.bundle_unreadable", "the bundle file cannot be read"))?
        .len();
    if size > options.limits.max_compressed_bytes {
        return Err(Refusal::new(
            "cli.bundle_invalid",
            "the bundle is larger than the limit",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|_| Refusal::new("cli.bundle_unreadable", "the bundle file cannot be read"))?;
    // The bundle reader opens it in memory: path rules, size limits, manifest and host version.
    // It writes nothing.
    // `open` never reads the digest field; a parsed all-zero value fills it without naming the
    // bundle crate's type.
    let Ok(unused) = "0".repeat(64).parse() else {
        return Err(Refusal::new(
            "cli.bundle_invalid",
            "the bundle cannot be opened",
        ));
    };
    let packed = spot_pack::PackedBundle {
        bytes,
        digest: unused,
    };
    let bundle = packed.open(&options).map_err(|errors| {
        let has = |wanted: &[&str]| errors.iter().any(|e| wanted.contains(&e.code));
        let (code, text) = if has(&[
            "bundle.illegal_path",
            "bundle.bad_symlink",
            "bundle.duplicate_entry",
            "bundle.unexpected_entry",
        ]) {
            (
                "cli.bundle_unsafe_path",
                "the bundle carries an entry that is not a plain relative file",
            )
        } else if has(&["bundle.host_required"]) {
            (
                "cli.bundle_incompatible",
                "the bundle needs another host version than this one",
            )
        } else {
            ("cli.bundle_invalid", "the bundle does not open as a bundle")
        };
        Refusal::new(code, text)
    })?;
    let digest = bundle.digest();
    let digest_hex = digest.to_hex();
    let manifest = bundle.manifest();
    let name = manifest.app.name.as_str().to_owned();
    let version = manifest.app.version.to_string();
    let with_digest = |mut r: Refusal| {
        r.digest = Some(digest_hex.clone());
        r
    };
    if name == PLATFORM || !apps::app_name(&name) {
        return Err(with_digest(Refusal::new(
            "cli.bundle_invalid",
            "the bundle names an app that cannot be installed",
        )));
    }
    let dir = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let claimed = read_signature(&dir);
    let claimed_publisher = claimed
        .as_ref()
        .ok()
        .map(|s| s.publisher_id.clone())
        .filter(|p| plain_id(p));
    let with_claim = |mut r: Refusal| {
        r.digest = Some(digest_hex.clone());
        r.claimed_publisher.clone_from(&claimed_publisher);
        r
    };

    let mode = match claimed {
        Err(SidecarFault::Missing) => {
            if trust.require_signed {
                return Err(with_claim(Refusal::new(
                    "cli.bundle_unsigned",
                    "the bundle has no signature and this site requires signed apps",
                )));
            }
            format!(
                "install mode: development, UNSIGNED bundle {name} {version} (digest {digest_hex}); \
                 [trust] require_signed = false"
            )
        }
        Err(SidecarFault::Bad) => {
            return Err(with_claim(Refusal::new(
                "cli.bundle_signature_invalid",
                "the signature file cannot be read",
            )));
        }
        Ok(signature) => {
            let certificate = read_certificate(&dir).map_err(|fault| {
                with_claim(match fault {
                    SidecarFault::Missing => Refusal::new(
                        "cli.bundle_certificate_missing",
                        "the publisher certificate is missing next to the bundle",
                    ),
                    SidecarFault::Bad => Refusal::new(
                        "cli.bundle_untrusted_publisher",
                        "the publisher certificate cannot be read",
                    ),
                })
            })?;
            let anchor = trust_anchor(root, trust).map_err(&with_claim)?;
            let revocations = revocation_list(root, trust).map_err(&with_claim)?;
            let proof = verify_packed(
                &bundle,
                &signature,
                &CertificateChain::new(certificate),
                &anchor,
                &revocations,
                issue.issued_at(),
                &install_policy(),
            )
            .map_err(|refusal| with_claim(map_signing(&refusal.0)))?;
            format!(
                "install mode: verified bundle {name} {version} (publisher {}, digest {digest_hex})",
                proof.publisher_id
            )
        }
    };

    // Verified. Only now does anything touch the disk.
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("app.toml".to_owned(), bundle.manifest_bytes().to_vec());
    if !bundle.wasm().is_empty() {
        files.insert("app.wasm".to_owned(), bundle.wasm().to_vec());
    }
    for (rel, content) in bundle.assets() {
        if !is_safe_rel_path(rel) || files.insert(rel.clone(), content.clone()).is_some() {
            return Err(with_claim(Refusal::new(
                "cli.bundle_unsafe_path",
                "the bundle carries an entry that is not a plain relative file",
            )));
        }
    }
    let target = root.join("apps").join(&name);
    if !apps::is_listed(root, loaded, &target) {
        return Err(with_claim(Refusal::new(
            "cli.bundle_not_listed",
            format!(
                "{} is not listed in common.toml `apps`; create it (empty, with an empty doctypes \
                 directory) and list it there first, then install the bundle",
                target.display()
            ),
        )));
    }
    // The operator provides the place: an app directory listed in `common.toml` `apps`, holding
    // nothing but an empty `doctypes` directory (the host refuses a listed directory without
    // one). A place that already holds this very content is fine (a dry run, then --apply);
    // a place that holds anything else is never replaced.
    let present = match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.is_dir() => read_tree(&target),
        _ => None,
    };
    match present {
        Some(found) if found == files => {}
        Some(found) if found.is_empty() => {
            // No file in it: only empty directories, which go before the verified tree comes in.
            std::fs::remove_dir_all(&target).map_err(|_| {
                with_claim(Refusal::new(
                    "cli.bundle_unpack_failed",
                    "the empty app directory cannot be replaced",
                ))
            })?;
            unpack(&target, &files).map_err(|e| {
                // Give the operator's empty place back.
                let _ = std::fs::create_dir_all(target.join("doctypes"));
                with_claim(Refusal::new("cli.bundle_unpack_failed", e))
            })?;
        }
        _ => {
            return Err(with_claim(Refusal::new(
                "cli.bundle_app_exists",
                format!(
                    "{} is missing or already holds other content; create it empty (with an empty \
                     doctypes directory) and list it in common.toml `apps`",
                    target.display()
                ),
            )));
        }
    }
    let app = appdir::load(&target)
        .map_err(|e| with_claim(Refusal::new("cli.bundle_invalid", e.to_string())))?;
    Ok(Installed { app, note: mode })
}

/// Writes `files` into a fresh temporary directory beside `target` and renames it into place.
/// A failure removes the temporary directory.
fn unpack(target: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let parent = target.parent().ok_or("the app folder has no parent")?;
    std::fs::create_dir_all(parent).map_err(|_| "the app folder cannot be created".to_owned())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temporary = parent.join(format!(".install-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&temporary)
        .map_err(|_| "no temporary directory could be made".to_owned())?;
    // The host lists app directories that carry a `doctypes` directory; a bundle has no DocType
    // files, so the directory is empty.
    let written = std::fs::create_dir(temporary.join("doctypes"))
        .map_err(|_| "a directory cannot be made".to_owned())
        .and_then(|()| write_all(&temporary, files));
    let placed = written.and_then(|()| {
        // A final place that appeared meanwhile is never replaced.
        if std::fs::symlink_metadata(target).is_ok() {
            return Err("the app directory appeared while unpacking".to_owned());
        }
        std::fs::rename(&temporary, target)
            .map_err(|_| "the app directory cannot be placed".to_owned())
    });
    if placed.is_err() {
        let _ = std::fs::remove_dir_all(&temporary);
    }
    placed
}

fn write_all(dir: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    for (rel, content) in files {
        if !is_safe_rel_path(rel) {
            return Err("an entry name is not a plain relative path".to_owned());
        }
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "a directory cannot be made".to_owned())?;
        }
        // `create_new`: never overwrites and never follows a link that is already there.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| "a file cannot be created".to_owned())?;
        file.write_all(content)
            .and_then(|()| file.sync_all())
            .map_err(|_| "a file cannot be written".to_owned())?;
    }
    Ok(())
}

/// Every regular file under `dir` by relative path, or `None` when there is a link, a special
/// file or an unreadable entry (so a planted link never counts as "the same content").
fn read_tree(dir: &Path) -> Option<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    collect(dir, "", &mut out).then_some(out)
}

fn collect(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Ok(name) = entry.file_name().into_string() else {
            return false;
        };
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        let rel = format!("{prefix}{name}");
        if kind.is_dir() {
            if !collect(&entry.path(), &format!("{rel}/"), out) {
                return false;
            }
        } else if kind.is_file() {
            match std::fs::read(entry.path()) {
                Ok(bytes) => {
                    out.insert(rel, bytes);
                }
                Err(_) => return false,
            }
        } else {
            return false;
        }
    }
    true
}

enum SidecarFault {
    Missing,
    Bad,
}

fn read_sidecar(path: &Path) -> Result<Vec<u8>, SidecarFault> {
    let meta = std::fs::metadata(path).map_err(|_| SidecarFault::Missing)?;
    if !meta.is_file() || meta.len() > SIDECAR_MAX_BYTES {
        return Err(SidecarFault::Bad);
    }
    std::fs::read(path).map_err(|_| SidecarFault::Bad)
}

fn read_signature(dir: &Path) -> Result<BundleSignature, SidecarFault> {
    let bytes = read_sidecar(&dir.join(SIGNATURE_FILE))?;
    BundleSignature::from_json(&bytes).map_err(|_| SidecarFault::Bad)
}

fn read_certificate(dir: &Path) -> Result<SignedCertificate, SidecarFault> {
    let bytes = read_sidecar(&dir.join(CERTIFICATE_FILE))?;
    SignedCertificate::from_json(&bytes).map_err(|_| SidecarFault::Bad)
}

fn configured(root: &Path, path: &Path) -> PathBuf {
    root.join(path)
}

/// The trust anchor from `[trust] roots`. Nothing configured, or nothing in the file, trusts
/// nothing.
fn trust_anchor(root: &Path, trust: &Trust) -> Result<TrustAnchor, Refusal> {
    let none = || {
        Refusal::new(
            "cli.bundle_no_trust",
            "this site trusts no publisher: pin a root key under [trust] roots in common.toml",
        )
    };
    let Some(path) = &trust.roots else {
        return Err(none());
    };
    let bytes = read_sidecar(&configured(root, path)).map_err(|_| none())?;
    let text = String::from_utf8(bytes).map_err(|_| none())?;
    let mut roots = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let key = spot_bundle_signing::canonical::unb64(line)
            .and_then(|b| PublicKey::from_bytes(&b).ok())
            .ok_or_else(none)?;
        roots.push(TrustedRoot::new(key, 0, None));
    }
    if roots.is_empty() {
        return Err(none());
    }
    Ok(TrustAnchor::new(roots))
}

/// The revocation list from `[trust] revocations`. Without one the installer cannot tell what
/// was revoked, so it refuses.
fn revocation_list(root: &Path, trust: &Trust) -> Result<RevocationList, Refusal> {
    let missing = || {
        Refusal::new(
            "cli.bundle_revocations_invalid",
            "this site has no usable revocation list: set [trust] revocations in common.toml",
        )
    };
    let Some(path) = &trust.revocations else {
        return Err(missing());
    };
    let bytes = read_sidecar(&configured(root, path)).map_err(|_| missing())?;
    RevocationList::from_json(&bytes).map_err(|_| missing())
}

/// A publisher id that is safe to put in an audit line: short and plain.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Maps the signing layer's refusal to this command's stable code.
fn map_signing(error: &SigningError) -> Refusal {
    use SigningError as E;
    let (code, text) = match error {
        E::UnknownRoot
        | E::RootOutsideWindow
        | E::CertificateSignatureInvalid
        | E::PublisherMismatch
        | E::SignatureKeyMismatch
        | E::KeyMalformed => (
            "cli.bundle_untrusted_publisher",
            "the publisher is not trusted by this site",
        ),
        E::PublisherRevoked | E::KeyRevoked | E::KeyCompromised | E::BundleRevoked => (
            "cli.bundle_revoked",
            "the publisher, the signing key or this bundle was revoked",
        ),
        E::CertificateExpired | E::CertificateNotYetValid | E::SignedOutsideCertificateWindow => (
            "cli.bundle_certificate_expired",
            "the publisher certificate is not valid now, or was not when the bundle was signed",
        ),
        E::RevocationSignatureInvalid | E::StaleRevocationList => (
            "cli.bundle_revocations_invalid",
            "the revocation list is not valid for this site",
        ),
        _ => (
            "cli.bundle_signature_invalid",
            "the signature does not verify over this bundle",
        ),
    };
    Refusal::new(code, text)
}
