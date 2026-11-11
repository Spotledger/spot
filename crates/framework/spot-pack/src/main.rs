//! The `spot-pack` command line: `check`, `pack`, `verify`, `inspect`, human text by
//! default and `--json`, exit codes 0 ok / 1 problems / 2 usage, and no network.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use spot_bundle::{Bundle, HostInfo, Limits};
use spot_bundle_signing::{
    BundleSignature, CertificateChain, RevocationList, SignedCertificate, TrustAnchor, TrustedRoot,
    VerifyMode, VerifyPolicy,
};
use spot_pack::PackProblem;
use spot_pack::error;
use spot_pack::report;
use spot_pack::signing::{FileKeyProvider, install_policy, sign_packed, verify_packed};

/// The exit code for a usage problem.
const USAGE: u8 = 2;
/// The exit code for problems a file or signature carries.
const PROBLEMS: u8 = 1;

#[derive(Parser)]
#[command(
    name = "spot-pack",
    version,
    about = "Check, pack, sign, verify and inspect a Spotledger app",
    after_help = "Exit codes: 0 ok, 1 problems, 2 usage."
)]
struct Cli {
    /// Print machine-readable JSON instead of human text.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Checks an app directory and reports every problem at once.
    Check {
        /// The app directory.
        dir: PathBuf,
        /// Optional native controller names this build provides (`native:` references are
        /// checked against them), comma-separated.
        #[arg(long)]
        native: Option<String>,
    },
    /// Packs a checked app into one deterministic bundle.
    Pack {
        /// The app directory.
        dir: PathBuf,
        /// Where the bundle is written.
        #[arg(short, long)]
        out: PathBuf,
        /// The key file to sign with (32 seed bytes, base64); signing is skipped without it.
        #[arg(long)]
        sign: Option<PathBuf>,
        /// The publisher certificate file, required with `--sign`.
        #[arg(long, requires = "sign")]
        certificate: Option<PathBuf>,
        /// The signing time as UTC seconds; packing never reads a clock.
        #[arg(long, requires = "sign")]
        signed_at: Option<i64>,
    },
    /// Verifies a bundle against its signature file, certificate, trust roots and
    /// revocation list.
    Verify {
        /// The bundle file.
        file: PathBuf,
        /// The signature file (as `pack --sign` wrote it).
        #[arg(long)]
        signature: PathBuf,
        /// The publisher certificate file.
        #[arg(long)]
        certificate: PathBuf,
        /// The trust-roots file: one root public key per line (base64).
        #[arg(long = "trust")]
        trust: PathBuf,
        /// The revocation list file.
        #[arg(long)]
        revocations: PathBuf,
        /// The check time as UTC seconds; verification never reads a clock.
        #[arg(long)]
        now: i64,
        /// The mode: `install` (default) or `startup`.
        #[arg(long, default_value = "install")]
        mode: String,
    },
    /// Inspects a bundle: manifest identity, digest, entries, capabilities, publisher.
    Inspect {
        /// The bundle file.
        file: PathBuf,
        /// The signature file, when the publisher should be shown.
        #[arg(long)]
        signature: Option<PathBuf>,
        /// The publisher certificate file, required with `--signature`.
        #[arg(long, requires = "signature")]
        certificate: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::from(0),
        Err(Failure::Usage(message)) => {
            eprintln!("spot-pack: {message}");
            ExitCode::from(USAGE)
        }
        Err(Failure::Problems(text)) => {
            if cli.json {
                print!("{text}");
            } else {
                eprint!("{text}");
            }
            ExitCode::from(PROBLEMS)
        }
    }
}

/// How one command's run ends.
enum Failure {
    /// A wrong command line: exit 2, human text on stderr.
    Usage(String),
    /// The file or signature has problems: exit 1 (JSON when `--json`).
    Problems(String),
}

fn run(cli: &Cli) -> Result<(), Failure> {
    match &cli.command {
        Command::Check { dir, native } => run_check(cli.json, dir, native.as_deref()),
        Command::Pack {
            dir,
            out,
            sign,
            certificate,
            signed_at,
        } => run_pack(
            cli.json,
            dir,
            out,
            sign.as_ref(),
            certificate.as_ref(),
            *signed_at,
        ),
        Command::Verify {
            file,
            signature,
            certificate,
            trust,
            revocations,
            now,
            mode,
        } => run_verify(
            cli.json,
            file,
            signature,
            certificate,
            trust,
            revocations,
            *now,
            mode,
        ),
        Command::Inspect {
            file,
            signature,
            certificate,
        } => run_inspect(cli.json, file, signature.as_ref(), certificate.as_ref()),
    }
}

fn run_check(json: bool, dir: &Path, native: Option<&str>) -> Result<(), Failure> {
    if !dir.is_dir() {
        return Err(Failure::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let names = native.map(|text| {
        BTreeSet::from_iter(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        )
    });
    let check = spot_pack::check_app(dir, names.as_ref());
    if check.is_ok() {
        if json {
            println!("{}", report::check_json(&check).unwrap_or_default());
        } else {
            println!("{}", report::check_text(&check).trim_end());
        }
        Ok(())
    } else {
        Err(check_failure(json, &check))
    }
}

fn run_pack(
    json: bool,
    dir: &Path,
    out: &Path,
    sign: Option<&PathBuf>,
    certificate: Option<&PathBuf>,
    signed_at: Option<i64>,
) -> Result<(), Failure> {
    if sign.is_some() && signed_at.is_none() {
        return Err(Failure::Usage(
            "--sign needs --signed-at (packing never reads a clock)".to_owned(),
        ));
    }
    let options = spot_pack::PackOptions::default();
    let (check, packed) = match spot_pack::pack_app(dir, &options) {
        Ok(pair) => pair,
        Err(check) => return Err(check_failure(json, &check)),
    };
    std::fs::write(out, &packed.bytes)
        .map_err(|err| Failure::Usage(format!("{} could not be written: {err}", out.display())))?;
    if let (Some(key_path), Some(cert_path), Some(at)) = (sign, certificate, signed_at) {
        sign_written(json, &packed, out, key_path, cert_path, at)?;
    }
    if json {
        println!(
            "{{\"ok\": true, \"out\": {:?}, \"digest\": \"{}\"}}",
            out.display(),
            packed.digest
        );
    } else {
        println!("packed {} ({})", out.display(), packed.digest);
    }
    let _ = check;
    Ok(())
}

/// Signs the just-written bundle and writes the signature next to it.
fn sign_written(
    json: bool,
    packed: &spot_pack::PackedBundle,
    out: &Path,
    key_path: &Path,
    cert_path: &Path,
    signed_at: i64,
) -> Result<(), Failure> {
    let bundle = open_bundle_bytes(&packed.bytes)?;
    let manifest = bundle.manifest();
    let certificate = read_certificate(cert_path).map_err(Failure::Usage)?;
    let provider = FileKeyProvider::new(key_path);
    let signature = sign_packed(
        &bundle,
        manifest.app.name.as_str(),
        &manifest.app.version.to_string(),
        &certificate,
        &provider,
        signed_at,
    )
    .map_err(|err| Failure::Usage(format!("signing failed: {err}")))?;
    let sig_path = sig_path_for(out);
    std::fs::write(&sig_path, signature.to_json() + "\n").map_err(|err| {
        Failure::Usage(format!(
            "{} could not be written: {err}",
            sig_path.display()
        ))
    })?;
    if json {
        println!(
            "{{\"signed\": true, \"signature\": {:?}, \"key_id\": \"{}\"}}",
            sig_path.display(),
            signature.key_id.as_str()
        );
    } else {
        println!(
            "signed as {} (key {})",
            sig_path.display(),
            signature.key_id.as_str()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_verify(
    json: bool,
    file: &Path,
    signature: &Path,
    certificate: &Path,
    trust: &Path,
    revocations: &Path,
    now: i64,
    mode: &str,
) -> Result<(), Failure> {
    let policy = match mode {
        "install" => install_policy(),
        "startup" => VerifyPolicy {
            mode: VerifyMode::Startup,
            allow_expired_certificate_at_startup: true,
            held_revocation_generation: 0,
        },
        other => {
            return Err(Failure::Usage(format!(
                "--mode is `install` or `startup`, not `{other}`"
            )));
        }
    };
    let bytes = read_file(file)?;
    let bundle = open_bundle_bytes(&bytes)?;
    let signature = read_signature(signature)?;
    let certificate = read_certificate(certificate).map_err(Failure::Usage)?;
    let chain = CertificateChain::new(certificate);
    let roots = read_trust_roots(trust).map_err(Failure::Usage)?;
    let anchor = TrustAnchor::new(roots);
    let list = read_revocations(revocations)?;
    let proof = match verify_packed(&bundle, &signature, &chain, &anchor, &list, now, &policy) {
        Ok(proof) => proof,
        Err(refusal) => {
            return Err(Failure::Problems(if json {
                report::refusal_json(&refusal).unwrap_or_default()
            } else {
                format!("refused: {}\n", refusal.0)
            }));
        }
    };
    if json {
        println!("{}", report::proof_json(&proof).unwrap_or_default());
    } else {
        println!(
            "verified: publisher {} signed at {} over {}",
            proof.publisher_id, proof.signed_at, proof.digest
        );
    }
    Ok(())
}

fn run_inspect(
    json: bool,
    file: &Path,
    signature: Option<&PathBuf>,
    certificate: Option<&PathBuf>,
) -> Result<(), Failure> {
    let bytes = read_file(file)?;
    let publisher = match (signature, certificate) {
        (Some(sig_path), _) => Some(read_signature(sig_path)?.publisher_id),
        _ => None,
    };
    let inspect =
        match spot_pack::inspect::inspect_bytes(&bytes, &Limits::new(), &the_host(), publisher) {
            Ok(inspect) => inspect,
            Err(errs) => return Err(bundle_failure(json, &errs)),
        };
    if json {
        println!("{}", report::inspect_json(&inspect).unwrap_or_default());
    } else {
        print!("{}", report::inspect_text(&inspect));
    }
    Ok(())
}

// --- shared helpers ----------------------------------------------------------------------------

/// The host version this CLI opens bundles for: the platform is at 0.1.0.
fn the_host() -> HostInfo {
    HostInfo {
        version: semver::Version::new(0, 1, 0),
    }
}

/// The exit-1 form of a check report.
fn check_failure(json: bool, check: &spot_pack::CheckReport) -> Failure {
    Failure::Problems(if json {
        report::check_json(check).unwrap_or_default()
    } else {
        report::check_text(check)
    })
}

/// The exit-1 form of the bundle's own open problems.
fn bundle_failure(json: bool, errs: &[spot_bundle::BundleError]) -> Failure {
    let problems: Vec<PackProblem> = errs
        .iter()
        .map(|e| PackProblem::new(error::WASM, e.to_string()))
        .collect();
    check_failure(
        json,
        &spot_pack::CheckReport {
            manifest: None,
            problems,
        },
    )
}

/// Opens bundle bytes as the installer does.
fn open_bundle_bytes(bytes: &[u8]) -> Result<Bundle, Failure> {
    Bundle::open(bytes, &Limits::new(), &the_host()).map_err(|errs| bundle_failure(false, &errs))
}

/// Reads a file, naming it on failure.
fn read_file(path: &Path) -> Result<Vec<u8>, Failure> {
    std::fs::read(path)
        .map_err(|err| Failure::Usage(format!("{} could not be read: {err}", path.display())))
}

/// Reads a signature file.
fn read_signature(path: &Path) -> Result<BundleSignature, Failure> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| Failure::Usage(format!("{} could not be read: {err}", path.display())))?;
    BundleSignature::from_json(text.as_bytes()).map_err(|err| {
        Failure::Usage(format!(
            "the signature file {} is malformed: {err}",
            path.display()
        ))
    })
}

/// Reads a publisher certificate file.
fn read_certificate(path: &Path) -> Result<SignedCertificate, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("{} could not be read: {err}", path.display()))?;
    SignedCertificate::from_json(text.as_bytes()).map_err(|err| {
        format!(
            "the certificate file {} is malformed: {err}",
            path.display()
        )
    })
}

/// Reads the trust-roots file: one root public key per line (base64, `#` comments allowed).
fn read_trust_roots(path: &Path) -> Result<Vec<TrustedRoot>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("{} could not be read: {err}", path.display()))?;
    let mut roots = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let bytes = spot_bundle_signing::canonical::unb64(line)
            .ok_or_else(|| format!("{} line {}: not a base64 public key", path.display(), n + 1))?;
        let key = spot_bundle_signing::PublicKey::from_bytes(&bytes)
            .map_err(|err| format!("{} line {}: {err}", path.display(), n + 1))?;
        roots.push(TrustedRoot::new(key, 0, None));
    }
    if roots.is_empty() {
        return Err(format!("{} names no root", path.display()));
    }
    Ok(roots)
}

/// Reads the revocation list file.
fn read_revocations(path: &Path) -> Result<RevocationList, Failure> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| Failure::Usage(format!("{} could not be read: {err}", path.display())))?;
    RevocationList::from_json(text.as_bytes()).map_err(|err| {
        Failure::Usage(format!(
            "the revocation list {} is malformed: {err}",
            path.display()
        ))
    })
}

/// Where `pack --sign` writes the signature: next to the bundle, `app.sig`.
fn sig_path_for(out: &Path) -> PathBuf {
    let mut path = out.to_path_buf();
    path.set_file_name("app.sig");
    path
}
