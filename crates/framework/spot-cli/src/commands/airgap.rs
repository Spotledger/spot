//! `spot airgap export|verify|import|list`: air-gapped operation over `spot-airgap`
//! (WP-ADM-12.T2). Guide: `docs/running-spot/airgap.md`.
//!
//! Everything crosses the gap as signed files on removable media:
//!
//! - `export --out DIR --what runtime,agent,packs,mirror --source DIR --version V --key FILE
//!   --certificate FILE`: on a connected machine, builds one signed bundle per artefact kind
//!   from `<source>/<kind>/` (for `packs`, one bundle per sub-directory of `<source>/packs/`);
//! - `verify BUNDLE --trust FILE --revocations FILE`: checks the signature chain, the validity
//!   window, the revocation list and every file's size and SHA-256 against the manifest, and
//!   that the media holds nothing the manifest does not list;
//! - `import BUNDLE --repo DIR ...`: verifies as above, refuses a downgrade or a changed digest,
//!   stages every file and commits into the repository by one rename;
//! - `list --repo DIR`: what the repository holds.
//!
//! `BUNDLE` is one bundle directory (it holds `offline.toml`) or an export directory as `export`
//! wrote it; every bundle found in the latter is handled.
//!
//! # Security
//!
//! - **Only signed content from a trusted root is accepted**, with the same rules as an online
//!   update: no flag turns the signature check off. Without `--trust` and `--revocations` (or
//!   `[trust]` in `common.toml`) every verify and import refuses.
//! - **No symlink is ever followed**: a symbolic link anywhere in a bundle, a release directory
//!   or a repository is refused (the storage wrapper [`NoLinkStorage`] checks every entry the
//!   library reads, lists or walks), and a manifest path with `..`, an absolute path or a
//!   backslash is refused by the library's own path rule.
//! - **Export allow-list**: only `runtime`, `agent`, `packs` and `mirror` are accepted; a release
//!   directory holding a private key (by file name or by a PEM private-key header) is refused;
//!   the signing key file must be mode 0600 and is read once, never printed, never copied.
//! - **Atomic**: an export is built in `<name>.partial` and renamed; an import stages and renames
//!   (a kill mid-import leaves the previous repository intact and the next import resumes).
//!
//! Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgMatches, Command};
use semver::Version;
use spot_airgap::manifest::{MEDIA_FILES_DIR, MEDIA_MANIFEST, MEDIA_SIGNATURE};
use spot_airgap::{
    AirgapError, BundleBuilder, BundleKind, BundleSpec, BundleTrust, ImportRequest, LocalRepo,
    OfflineBundle, Signer, StdStorage, Storage, import, verify_file, walk_files,
};
use spot_bundle_signing::{
    BundleSignature, KeyPair, PublicKey, RevocationList, SignedCertificate, SigningError,
    TrustAnchor, TrustedRoot, generate_keypair, sign_bundle,
};

use super::{CommandCtx, Exit};

pub const NAME: &str = "airgap";

/// The artefact kinds `export --what` accepts, and nothing else.
pub const ALLOWED_WHAT: [&str; 4] = ["runtime", "agent", "packs", "mirror"];
/// The file in the repository root that remembers the newest revocation generation seen.
pub const HELD_GENERATION_FILE: &str = "revocation.generation";
/// The most a key, certificate, root or revocation file may weigh.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

pub fn command() -> Command {
    let trust = || {
        [
            Arg::new("trust")
                .long("trust")
                .value_name("FILE")
                .help("Trust roots: one base64 root public key per line (default: [trust] roots in common.toml)"),
            Arg::new("revocations")
                .long("revocations")
                .value_name("FILE")
                .help("The signed revocation list, JSON (default: [trust] revocations in common.toml)"),
        ]
    };
    Command::new(NAME)
        .about("Air-gapped operation: signed offline bundles on removable media")
        .subcommand_required(true)
        .subcommand(
            Command::new("export")
                .about("Build signed bundles (runtime, agent, packs, mirror) on a connected machine")
                .arg(Arg::new("out").long("out").value_name("DIR").required(true).help("Where the media layout is written (must not exist yet)"))
                .arg(Arg::new("what").long("what").value_name("LIST").required(true).help("Comma list of: runtime, agent, packs, mirror"))
                .arg(Arg::new("source").long("source").value_name("DIR").required(true).help("The release directory: <DIR>/runtime, <DIR>/agent, <DIR>/packs/<name>, <DIR>/mirror"))
                .arg(Arg::new("version").long("version").value_name("SEMVER").required(true).help("The version of every bundle"))
                .arg(Arg::new("key").long("key").value_name("FILE").required(true).help("The publisher signing key: base64 of the 32-byte seed, mode 0600"))
                .arg(Arg::new("certificate").long("certificate").value_name("FILE").required(true).help("The publisher certificate the key belongs to (JSON)"))
                .arg(Arg::new("notes").long("notes").value_name("TEXT").help("Release notes for the administrator")),
        )
        .subcommand(
            Command::new("verify")
                .about("Check a bundle's signature chain, window, revocation and every file digest")
                .arg(Arg::new("bundle").value_name("BUNDLE").required(true))
                .args(trust())
                .arg(Arg::new("repo").long("repo").value_name("DIR").help("A repository whose held revocation generation applies")),
        )
        .subcommand(
            Command::new("import")
                .about("Verify and import a bundle into the local repository, atomically")
                .arg(Arg::new("bundle").value_name("BUNDLE").required(true))
                .arg(Arg::new("repo").long("repo").value_name("DIR").required(true))
                .args(trust()),
        )
        .subcommand(
            Command::new("list")
                .about("List what the local repository holds")
                .arg(Arg::new("repo").long("repo").value_name("DIR").required(true)),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let outcome = match matches.subcommand() {
        Some(("export", m)) => export(m, ctx),
        Some(("verify", m)) => verify(m, ctx),
        Some(("import", m)) => import_command(m, ctx),
        Some(("list", m)) => list(m, ctx),
        _ => Err(Fault::Usage(
            "usage: spot airgap export|verify|import|list".to_owned(),
        )),
    };
    Ok(match outcome {
        Ok(()) => Exit::Ok,
        Err(fault) => {
            let (exit, text) = match fault {
                Fault::Usage(text) => (Exit::Usage, text),
                Fault::Refused(text) => (Exit::Refused, text),
                Fault::Failed(text) => (Exit::Failed, text),
            };
            let _ = writeln!(ctx.err(), "{text}");
            exit
        }
    })
}

/// Why a subcommand stopped.
#[derive(Debug)]
pub enum Fault {
    /// Wrong or missing arguments.
    Usage(String),
    /// Refused by policy or by a check; nothing was changed.
    Refused(String),
    /// Failed while running (an I/O fault).
    Failed(String),
}

impl From<AirgapError> for Fault {
    fn from(error: AirgapError) -> Self {
        let text = format!("refused: {}: {error}", error.code());
        match error {
            AirgapError::Io { .. } => Fault::Failed(text),
            _ => Fault::Refused(text),
        }
    }
}

fn say(ctx: &mut CommandCtx, line: impl AsRef<str>) {
    let _ = writeln!(ctx.out(), "{}", line.as_ref());
}

fn arg<'a>(matches: &'a ArgMatches, name: &str) -> Result<&'a String, Fault> {
    matches
        .get_one::<String>(name)
        .ok_or_else(|| Fault::Usage(format!("missing --{name}")))
}

fn now(ctx: &CommandCtx) -> i64 {
    ctx.clock().now_utc().unix_timestamp()
}

// ---- storage that never follows a symbolic link ---------------------------------------------

/// `StdStorage` that refuses to read, list or stat a symbolic link. The library walks and reads
/// through this seam, so a link anywhere in a bundle or a release directory stops the run.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLinkStorage {
    inner: StdStorage,
}

impl NoLinkStorage {
    /// The wrapper over the real filesystem.
    pub fn new() -> Self {
        Self::default()
    }
}

fn refuse_link(path: &Path) -> Result<(), AirgapError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(AirgapError::ExtraFile {
            path: format!("{} (a symbolic link)", path.display()),
        }),
        _ => Ok(()),
    }
}

impl Storage for NoLinkStorage {
    fn read(&self, path: &Path) -> Result<Vec<u8>, AirgapError> {
        refuse_link(path)?;
        self.inner.read(path)
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), AirgapError> {
        self.inner.write_atomic(path, bytes)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), AirgapError> {
        self.inner.rename(from, to)
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
        self.inner.remove_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> Result<(), AirgapError> {
        self.inner.remove_file(path)
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
        self.inner.create_dir_all(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, AirgapError> {
        self.inner.exists(path)
    }

    fn list(&self, path: &Path) -> Result<Vec<String>, AirgapError> {
        refuse_link(path)?;
        self.inner.list(path)
    }

    fn is_dir(&self, path: &Path) -> Result<bool, AirgapError> {
        refuse_link(path)?;
        self.inner.is_dir(path)
    }
}

// ---- trust and key files --------------------------------------------------------------------

fn read_regular(path: &Path, what: &str) -> Result<Vec<u8>, Fault> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| Fault::Refused(format!("refused: the {what} file cannot be read")))?;
    if !meta.is_file() || meta.len() > SIDECAR_MAX_BYTES {
        return Err(Fault::Refused(format!(
            "refused: the {what} is not a regular file of sane size"
        )));
    }
    std::fs::read(path)
        .map_err(|_| Fault::Refused(format!("refused: the {what} file cannot be read")))
}

/// The roots and the revocation list a verify or an import is checked against.
pub struct TrustFiles {
    anchor: TrustAnchor,
    revocations: RevocationList,
}

fn load_trust(ctx: &mut CommandCtx, matches: &ArgMatches) -> Result<TrustFiles, Fault> {
    let explicit_roots = matches.get_one::<String>("trust").map(PathBuf::from);
    let explicit_revocations = matches.get_one::<String>("revocations").map(PathBuf::from);
    let (roots, revocations) = match (explicit_roots, explicit_revocations) {
        (Some(roots), Some(revocations)) => (roots, revocations),
        (roots, revocations) => {
            let root = ctx.root().to_path_buf();
            let common = ctx.common().map_err(|error| {
                Fault::Refused(format!(
                    "refused: no trust configured ({error}); pass --trust and --revocations"
                ))
            })?;
            let configured_roots =
                roots.or_else(|| common.trust.roots.as_ref().map(|p| root.join(p)));
            let configured_revocations =
                revocations.or_else(|| common.trust.revocations.as_ref().map(|p| root.join(p)));
            match (configured_roots, configured_revocations) {
                (Some(roots), Some(revocations)) => (roots, revocations),
                _ => {
                    return Err(Fault::Refused(
                        "refused: this host trusts no publisher: pass --trust and --revocations, or set [trust] in common.toml"
                            .to_owned(),
                    ));
                }
            }
        }
    };
    let text = String::from_utf8(read_regular(&roots, "trust roots")?)
        .map_err(|_| Fault::Refused("refused: the trust roots file is not text".to_owned()))?;
    let mut list = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let key = spot_bundle_signing::canonical::unb64(line)
            .and_then(|bytes| PublicKey::from_bytes(&bytes).ok())
            .ok_or_else(|| {
                Fault::Refused("refused: a trust root is not a public key".to_owned())
            })?;
        list.push(TrustedRoot::new(key, 0, None));
    }
    if list.is_empty() {
        return Err(Fault::Refused(
            "refused: the trust roots file names no root".to_owned(),
        ));
    }
    let revocations = RevocationList::from_json(&read_regular(&revocations, "revocation list")?)
        .map_err(|_| Fault::Refused("refused: the revocation list is not valid".to_owned()))?;
    Ok(TrustFiles {
        anchor: TrustAnchor::new(list),
        revocations,
    })
}

fn held_generation(storage: &dyn Storage, repo: &LocalRepo) -> u64 {
    let path = repo.root().join(HELD_GENERATION_FILE);
    match storage.exists(&path) {
        Ok(true) => storage
            .read(&path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

fn remember_generation(
    storage: &dyn Storage,
    repo: &LocalRepo,
    generation: u64,
) -> Result<(), AirgapError> {
    if generation <= held_generation(storage, repo) {
        return Ok(());
    }
    storage.create_dir_all(repo.root())?;
    storage.write_atomic(
        &repo.root().join(HELD_GENERATION_FILE),
        format!("{generation}\n").as_bytes(),
    )
}

// ---- finding bundles ------------------------------------------------------------------------

/// The bundle directories under `path`: `path` itself when it holds `offline.toml`, else the
/// `runtime`, `agent`, `mirror` and `packs/<name>` directories of an export that hold one.
pub fn find_bundles(storage: &dyn Storage, path: &Path) -> Result<Vec<PathBuf>, AirgapError> {
    if storage.exists(&path.join(MEDIA_MANIFEST))? {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut found = Vec::new();
    for fixed in ["runtime", "agent", "mirror"] {
        let candidate = path.join(fixed);
        if storage.exists(&candidate.join(MEDIA_MANIFEST))? {
            found.push(candidate);
        }
    }
    let packs = path.join("packs");
    if storage.is_dir(&packs)? {
        for name in storage.list(&packs)? {
            let candidate = packs.join(name);
            if storage.exists(&candidate.join(MEDIA_MANIFEST))? {
                found.push(candidate);
            }
        }
    }
    Ok(found)
}

fn bundles_of(storage: &dyn Storage, path: &Path) -> Result<Vec<PathBuf>, Fault> {
    let found = find_bundles(storage, path)?;
    if found.is_empty() {
        return Err(Fault::Refused(format!(
            "refused: no bundle (offline.toml) under {}",
            path.display()
        )));
    }
    Ok(found)
}

// ---- verify ---------------------------------------------------------------------------------

/// What a successful verification established.
pub struct Verified {
    pub kind: BundleKind,
    pub name: String,
    pub version: Version,
    pub digest: String,
    pub publisher: String,
    pub files: usize,
}

/// Verifies one bundle on media without touching any repository: a signature must exist, the
/// signature chain, window and revocation must pass, every listed file must match its size and
/// SHA-256, and no unlisted file may be present.
///
/// # Errors
/// The first failing check, as the library reports it.
pub fn verify_media(
    storage: &dyn Storage,
    media: &Path,
    trust: &BundleTrust<'_>,
) -> Result<Verified, AirgapError> {
    if !storage.exists(&media.join(MEDIA_SIGNATURE))? {
        return Err(AirgapError::Unsigned);
    }
    let bundle = OfflineBundle::read(storage, media)?;
    let verified = bundle.verify(trust)?;
    let files_dir = media.join(MEDIA_FILES_DIR);
    if storage.exists(&files_dir)? {
        for present in walk_files(storage, &files_dir)? {
            if bundle.manifest.file(&present).is_none() {
                return Err(AirgapError::ExtraFile { path: present });
            }
        }
    }
    for file in &bundle.manifest.files {
        let path = files_dir.join(&file.path);
        if !storage.exists(&path)? {
            return Err(AirgapError::FileMissing {
                path: file.path.clone(),
            });
        }
        let bytes = storage
            .read(&path)
            .map_err(|_| AirgapError::MediaUnreadable {
                path: path.display().to_string(),
            })?;
        verify_file(file, &bytes)?;
    }
    Ok(Verified {
        kind: bundle.manifest.kind,
        name: bundle.manifest.name.clone(),
        version: bundle.manifest.version.clone(),
        digest: bundle.manifest.digest().to_hex(),
        publisher: verified.publisher_id,
        files: bundle.manifest.files.len(),
    })
}

fn verify(matches: &ArgMatches, ctx: &mut CommandCtx) -> Result<(), Fault> {
    let storage = NoLinkStorage::new();
    let bundle = PathBuf::from(arg(matches, "bundle")?);
    let trust = load_trust(ctx, matches)?;
    let held = match matches.get_one::<String>("repo") {
        Some(repo) => held_generation(&storage, &LocalRepo::new(repo)),
        None => 0,
    };
    let at = now(ctx);
    let facts = BundleTrust {
        anchor: &trust.anchor,
        revocations: &trust.revocations,
        now: at,
        held_revocation_generation: held,
    };
    for media in bundles_of(&storage, &bundle)? {
        let ok = verify_media(&storage, &media, &facts)?;
        say(
            ctx,
            format!(
                "verified {} {} {} publisher={} digest={} files={}",
                ok.kind.as_str(),
                ok.name,
                ok.version,
                ok.publisher,
                ok.digest,
                ok.files
            ),
        );
    }
    Ok(())
}

// ---- import ---------------------------------------------------------------------------------

fn import_command(matches: &ArgMatches, ctx: &mut CommandCtx) -> Result<(), Fault> {
    let storage = NoLinkStorage::new();
    let bundle = PathBuf::from(arg(matches, "bundle")?);
    let repo = LocalRepo::new(arg(matches, "repo")?);
    let trust = load_trust(ctx, matches)?;
    let at = now(ctx);
    let media_dirs = bundles_of(&storage, &bundle)?;
    for media in media_dirs {
        if !storage.exists(&media.join(MEDIA_SIGNATURE))? {
            return Err(AirgapError::Unsigned.into());
        }
        let request = ImportRequest {
            media: &media,
            repo: &repo,
            trust: BundleTrust {
                anchor: &trust.anchor,
                revocations: &trust.revocations,
                now: at,
                held_revocation_generation: held_generation(&storage, &repo),
            },
            imported_at: at,
        };
        let report = import(&storage, &request)?;
        remember_generation(&storage, &repo, trust.revocations.body.generation)?;
        let state = if report.already_present {
            "already present"
        } else if report.resumed {
            "imported (resumed)"
        } else {
            "imported"
        };
        say(
            ctx,
            format!(
                "{state} {} {} {} files={}",
                report.kind.as_str(),
                report.name,
                report.version,
                report.files
            ),
        );
    }
    Ok(())
}

// ---- list -----------------------------------------------------------------------------------

fn list(matches: &ArgMatches, ctx: &mut CommandCtx) -> Result<(), Fault> {
    let storage = NoLinkStorage::new();
    let repo = LocalRepo::new(arg(matches, "repo")?);
    let index = repo.read_index(&storage)?;
    if index.entries.is_empty() {
        say(ctx, "the repository holds no bundle");
    }
    for entry in &index.entries {
        say(
            ctx,
            format!(
                "{} {} {} digest={} files={}",
                entry.kind.as_str(),
                entry.name,
                entry.version,
                entry.digest.to_hex(),
                entry.files.len()
            ),
        );
    }
    Ok(())
}

/// The directory of the newest imported agent bundle named `name`: an agent's offline update
/// source (`spot_agent::update::DirSource::new(dir)` reads `update.json` and the payload there).
///
/// # Errors
/// The library's repository errors.
pub fn latest_agent_dir(
    storage: &dyn Storage,
    repo: &LocalRepo,
    name: &str,
) -> Result<Option<PathBuf>, AirgapError> {
    let index = repo.read_index(storage)?;
    Ok(index
        .installed(BundleKind::Agent, name)
        .map(|entry| repo.bundle_dir(entry.kind, &entry.name, &entry.version)))
}

// ---- export ---------------------------------------------------------------------------------

struct FileSigner {
    key: KeyPair,
    certificate: SignedCertificate,
}

impl Signer for FileSigner {
    fn certificate(&self) -> SignedCertificate {
        self.certificate.clone()
    }

    fn sign_bundle(
        &self,
        digest: &spot_bundle::Digest,
        name: &str,
        version: &str,
        signed_at: i64,
    ) -> Result<BundleSignature, SigningError> {
        sign_bundle(
            digest,
            name,
            version,
            &self.certificate,
            &self.key,
            signed_at,
        )
    }
}

fn load_signer(key_path: &Path, certificate_path: &Path) -> Result<FileSigner, Fault> {
    let meta = std::fs::symlink_metadata(key_path)
        .map_err(|_| Fault::Refused("refused: the key file cannot be read".to_owned()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Fault::Refused(
                "refused: the key file must not be readable by group or others (chmod 600)"
                    .to_owned(),
            ));
        }
    }
    let text = String::from_utf8(read_regular(key_path, "key")?)
        .map_err(|_| Fault::Refused("refused: the key file is not text".to_owned()))?;
    let seed_bytes = spot_bundle_signing::canonical::unb64(text.trim())
        .ok_or_else(|| Fault::Refused("refused: the key file is not a base64 seed".to_owned()))?;
    let seed = <[u8; 32]>::try_from(seed_bytes.as_slice())
        .map_err(|_| Fault::Refused("refused: the key seed is not 32 bytes".to_owned()))?;
    let key = generate_keypair(&seed);
    let certificate = SignedCertificate::from_json(&read_regular(certificate_path, "certificate")?)
        .map_err(|_| Fault::Refused("refused: the certificate is not valid".to_owned()))?;
    if key.public() != certificate.body.key {
        return Err(Fault::Refused(
            "refused: the key is not the certificate's key".to_owned(),
        ));
    }
    Ok(FileSigner { key, certificate })
}

/// One bundle to build.
#[derive(Debug, PartialEq, Eq)]
pub struct Planned {
    pub kind: BundleKind,
    pub name: String,
    pub release: PathBuf,
    /// The bundle's directory relative to `--out`.
    pub relative: PathBuf,
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// Parses `--what` against the allow-list and lays out the bundles.
///
/// # Errors
/// [`Fault::Usage`] for an unknown kind or an empty list, [`Fault::Refused`] for a missing or
/// unsafe release directory.
pub fn plan_export(
    storage: &dyn Storage,
    what: &str,
    source: &Path,
) -> Result<Vec<Planned>, Fault> {
    let mut wanted: Vec<&str> = Vec::new();
    for item in what.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !ALLOWED_WHAT.contains(&item) {
            return Err(Fault::Usage(format!(
                "`{item}` is not an artefact kind; export takes only: {}",
                ALLOWED_WHAT.join(", ")
            )));
        }
        if !wanted.contains(&item) {
            wanted.push(item);
        }
    }
    if wanted.is_empty() {
        return Err(Fault::Usage("--what names no artefact kind".to_owned()));
    }
    let mut plan = Vec::new();
    for item in wanted {
        let dir = source.join(item);
        if !storage.is_dir(&dir)? {
            return Err(Fault::Refused(format!(
                "refused: the source has no `{item}` directory"
            )));
        }
        match item {
            "packs" => {
                let mut any = false;
                for name in storage.list(&dir)? {
                    let release = dir.join(&name);
                    if !storage.is_dir(&release)? {
                        continue;
                    }
                    if !safe_name(&name) {
                        return Err(Fault::Refused(format!(
                            "refused: `{name}` is not a safe pack name"
                        )));
                    }
                    any = true;
                    plan.push(Planned {
                        kind: BundleKind::Pack,
                        relative: PathBuf::from("packs").join(&name),
                        name,
                        release,
                    });
                }
                if !any {
                    return Err(Fault::Refused(
                        "refused: `packs` holds no pack directory".to_owned(),
                    ));
                }
            }
            other => {
                let (kind, name) = match other {
                    "runtime" => (BundleKind::Runtime, "spot-runtime"),
                    "agent" => (BundleKind::Agent, "spot-agent"),
                    _ => (BundleKind::OsMirror, "os-mirror"),
                };
                plan.push(Planned {
                    kind,
                    name: name.to_owned(),
                    release: dir,
                    relative: PathBuf::from(other),
                });
            }
        }
    }
    Ok(plan)
}

const SECRET_NAMES: [&str; 8] = [
    ".key",
    ".pem",
    ".p12",
    ".pfx",
    ".jks",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
];

/// Refuses a release directory that holds a private key, by file name or by a PEM private-key
/// header in the first bytes of a file.
///
/// # Errors
/// [`Fault::Refused`] naming the file (never its content).
pub fn refuse_secrets(storage: &dyn Storage, release: &Path) -> Result<(), Fault> {
    for relative in walk_files(storage, release)? {
        let lower = relative.to_ascii_lowercase();
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        if SECRET_NAMES.iter().any(|n| base.ends_with(n)) {
            return Err(Fault::Refused(format!(
                "refused: {relative} looks like a key file; a bundle never carries private keys"
            )));
        }
        let mut head = [0u8; 8192];
        let read = std::fs::File::open(release.join(&relative))
            .and_then(|mut file| file.read(&mut head))
            .map_err(|_| Fault::Failed(format!("{relative} cannot be read")))?;
        let text = String::from_utf8_lossy(&head[..read]);
        if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
            return Err(Fault::Refused(format!(
                "refused: {relative} holds a private key; a bundle never carries private keys"
            )));
        }
    }
    Ok(())
}

fn export(matches: &ArgMatches, ctx: &mut CommandCtx) -> Result<(), Fault> {
    let storage = NoLinkStorage::new();
    let out = PathBuf::from(arg(matches, "out")?);
    let source = PathBuf::from(arg(matches, "source")?);
    let version = Version::parse(arg(matches, "version")?)
        .map_err(|_| Fault::Usage("--version is not a semantic version".to_owned()))?;
    let plan = plan_export(&storage, arg(matches, "what")?, &source)?;
    let (Ok(abs_out), Ok(abs_source)) = (std::path::absolute(&out), std::path::absolute(&source))
    else {
        return Err(Fault::Failed("a path cannot be made absolute".to_owned()));
    };
    if abs_out.starts_with(&abs_source) {
        return Err(Fault::Usage("--out must not be inside --source".to_owned()));
    }
    if storage.exists(&out)? {
        return Err(Fault::Refused(format!(
            "refused: {} already exists; an export never overwrites",
            out.display()
        )));
    }
    for item in &plan {
        refuse_secrets(&storage, &item.release)?;
    }
    let signer = load_signer(
        Path::new(arg(matches, "key")?),
        Path::new(arg(matches, "certificate")?),
    )?;
    let at = now(ctx);
    // Build everything beside the final directory, then rename once: a failed or interrupted
    // export leaves no half-written media layout under the name that was asked for.
    let building = PathBuf::from(format!("{}.partial", out.display()));
    storage.remove_dir_all(&building)?;
    storage.create_dir_all(&building)?;
    for item in &plan {
        let spec = BundleSpec {
            kind: item.kind,
            name: item.name.clone(),
            version: version.clone(),
            released_at: at,
            notes: matches.get_one::<String>("notes").cloned(),
        };
        let manifest = BundleBuilder::new(spec, &signer)
            .assemble(&storage, &item.release, &building.join(&item.relative))
            .inspect_err(|_| {
                let _ = storage.remove_dir_all(&building);
            })?;
        say(
            ctx,
            format!(
                "exported {} {} {} files={} digest={}",
                item.kind.as_str(),
                manifest.name,
                manifest.version,
                manifest.files.len(),
                manifest.digest().to_hex()
            ),
        );
    }
    storage.rename(&building, &out)?;
    say(ctx, format!("wrote {}", out.display()));
    Ok(())
}
