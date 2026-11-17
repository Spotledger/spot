//! WP-ADM-12.T2: `spot airgap export|verify|import|list`, driven over the command registry
//! without a process (the technique of `tests/support_bundle.rs`), on temporary directories.
//!
//! Covered: the export/verify/import round trip for every artefact kind; the refusals the brief
//! names, each with its own code (tampered file, expired window, unsigned bundle, zip-slip
//! path, symlink entry, extra file, downgrade); atomic import (a refused or interrupted import
//! leaves the previous repository intact, a leftover staging directory is resumed over); the
//! export allow-list, the private-key guard, the key-file mode and no overwrite; and that no
//! output ever carries the signing seed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::ArgMatches;
use spot_bundle_signing::{
    KeyPair, PublisherCertificate, RevocationBody, TrustAnchor, canonical, generate_keypair,
    issue_certificate, issue_revocation_list,
};
use spot_core::ManualClock;
use time::OffsetDateTime;
use time::macros::datetime;

#[path = "../src/commands/mod.rs"]
mod commands;

use commands::{CommandCtx, Exit};

const NOW: OffsetDateTime = datetime!(2026-11-08 12:00:00 UTC);
const LATER: OffsetDateTime = datetime!(2026-11-08 20:00:00 UTC);

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// What one command printed and how it ended.
struct Run {
    exit: Exit,
    out: String,
    err: String,
}

impl Run {
    fn all(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

async fn run_at(at: OffsetDateTime, root: &Path, args: &[&str]) -> Run {
    let command = commands::augment(
        clap::Command::new("spot").subcommand_required(true),
        commands::COMMANDS,
    )
    .unwrap();
    let mut full = vec!["spot", "airgap"];
    full.extend_from_slice(args);
    let matches: ArgMatches = command.try_get_matches_from(full).unwrap();
    let (out, err) = (Sink::default(), Sink::default());
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        Arc::new(ManualClock::new(at)),
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let exit = commands::run_registered(commands::COMMANDS, &matches, &mut ctx)
        .await
        .unwrap();
    Run {
        exit,
        out: out.text(),
        err: err.text(),
    }
}

async fn run(root: &Path, args: &[&str]) -> Run {
    run_at(NOW, root, args).await
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A temp directory that is removed at the end of the test.
struct Temp(PathBuf);
impl Temp {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spot-cli-airgap-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn join(&self, part: &str) -> PathBuf {
        self.0.join(part)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A publisher authority written to files: roots, revocation list, signing key, certificate.
struct Authority {
    seed_text: String,
    roots: PathBuf,
    revocations: PathBuf,
    key: PathBuf,
    certificate: PathBuf,
}

fn write_private(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let _ = mode;
}

fn authority(dir: &Path, valid_to: i64, generation: u64) -> Authority {
    let root: KeyPair = generate_keypair(&[1u8; 32]);
    let publisher = generate_keypair(&[2u8; 32]);
    let certificate = issue_certificate(
        PublisherCertificate {
            publisher_id: "spotledger-test".to_owned(),
            display_name: "Test Publisher".to_owned(),
            key: publisher.public(),
            valid_from: 0,
            valid_to,
            issued_by: root.public().key_id(),
            serial: 1,
        },
        &root,
    );
    let revocations = issue_revocation_list(
        RevocationBody {
            generation,
            issued_at: 0,
            issued_by: root.public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root,
    );
    let seed_text = canonical::b64(&publisher.to_seed_bytes());
    let out = Authority {
        seed_text: seed_text.clone(),
        roots: dir.join("roots.txt"),
        revocations: dir.join("revocations.json"),
        key: dir.join("publisher.seed"),
        certificate: dir.join("certificate.json"),
    };
    std::fs::write(
        &out.roots,
        format!(
            "# the store root\n{}\n",
            canonical::b64(root.public().as_bytes())
        ),
    )
    .unwrap();
    std::fs::write(&out.revocations, revocations.to_json()).unwrap();
    write_private(&out.key, &format!("{seed_text}\n"), 0o600);
    std::fs::write(&out.certificate, certificate.to_json()).unwrap();
    let _ = TrustAnchor::new(Vec::new());
    out
}

fn far_future() -> i64 {
    NOW.unix_timestamp() + 1_000_000
}

/// A release tree: runtime, agent, two packs, a mirror.
fn release(dir: &Path, tag: &str) -> PathBuf {
    let source = dir.join("release");
    let put = |relative: &str, bytes: &str| {
        let path = source.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    put("runtime/bin/spot", &format!("runtime-binary-{tag}"));
    put("runtime/share/notice.txt", "notice");
    put("agent/update.json", &format!("{{\"tag\":\"{tag}\"}}"));
    put(
        "agent/spot-agent-update.zip",
        &format!("agent-archive-{tag}"),
    );
    put("packs/accounts/pack.toml", "name = \"accounts\"");
    put("packs/sales/pack.toml", "name = \"sales\"");
    put("mirror/pool/main/p/pkg_1.0_amd64.deb", "deb-bytes");
    put("mirror/dists/stable/Release", "release-file");
    source
}

struct Fixture {
    temp: Temp,
    authority: Authority,
    source: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temp = Temp::new(label);
        let authority = authority(temp.path(), far_future(), 1);
        let source = release(temp.path(), "1");
        Self {
            temp,
            authority,
            source,
        }
    }

    async fn export(&self, out_name: &str, version: &str, what: &str) -> Run {
        let out = self.temp.join(out_name);
        run(
            self.temp.path(),
            &[
                "export",
                "--out",
                s(&out),
                "--what",
                what,
                "--source",
                s(&self.source),
                "--version",
                version,
                "--key",
                s(&self.authority.key),
                "--certificate",
                s(&self.authority.certificate),
            ],
        )
        .await
    }

    fn trust_args(&self) -> [String; 4] {
        [
            "--trust".to_owned(),
            s(&self.authority.roots).to_owned(),
            "--revocations".to_owned(),
            s(&self.authority.revocations).to_owned(),
        ]
    }

    async fn with_trust(&self, at: OffsetDateTime, command: &[&str]) -> Run {
        let trust = self.trust_args();
        let mut args: Vec<&str> = command.to_vec();
        args.extend(trust.iter().map(String::as_str));
        run_at(at, self.temp.path(), &args).await
    }

    async fn verify(&self, bundle: &Path) -> Run {
        self.with_trust(NOW, &["verify", s(bundle)]).await
    }

    async fn import(&self, bundle: &Path, repo: &Path) -> Run {
        self.with_trust(NOW, &["import", s(bundle), "--repo", s(repo)])
            .await
    }
}

fn tree(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<_> = entries.map(|e| e.unwrap().file_name()).collect();
        names.sort();
        for name in names {
            let name = name.to_string_lossy().into_owned();
            let path = dir.join(&name);
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if path.is_dir() {
                walk(&path, &relative, out);
            } else {
                out.push(format!(
                    "{relative}={}",
                    std::fs::read(&path).unwrap().len()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", &mut out);
    out
}

// ---- the round trip -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_verify_import_round_trip_for_every_artefact_kind() {
    let f = Fixture::new("roundtrip");
    let exported = f
        .export("media", "2.0.0", "runtime,agent,packs,mirror")
        .await;
    assert_eq!(exported.exit, Exit::Ok, "{}", exported.all());
    for line in [
        "exported runtime spot-runtime 2.0.0",
        "exported agent spot-agent 2.0.0",
        "exported pack accounts 2.0.0",
        "exported pack sales 2.0.0",
        "exported os-mirror os-mirror 2.0.0",
    ] {
        assert!(exported.out.contains(line), "{line} in {}", exported.out);
    }
    let media = f.temp.join("media");
    assert!(media.join("runtime/offline.toml").is_file());
    assert!(media.join("packs/sales/files/pack.toml").is_file());

    let verified = f.verify(&media).await;
    assert_eq!(verified.exit, Exit::Ok, "{}", verified.all());
    assert_eq!(
        verified.out.matches("verified ").count(),
        5,
        "{}",
        verified.out
    );
    assert!(verified.out.contains("publisher=spotledger-test"));

    let repo = f.temp.join("repo");
    let imported = f.import(&media, &repo).await;
    assert_eq!(imported.exit, Exit::Ok, "{}", imported.all());
    assert_eq!(
        imported.out.matches("imported ").count(),
        5,
        "{}",
        imported.out
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("bundles/runtime/spot-runtime/2.0.0/bin/spot")).unwrap(),
        "runtime-binary-1"
    );
    assert!(
        repo.join("bundles/agent/spot-agent/2.0.0/update.json")
            .is_file()
    );
    assert!(!repo.join("staging").exists() || tree(&repo.join("staging")).is_empty());
    assert_eq!(
        std::fs::read_to_string(repo.join("revocation.generation"))
            .unwrap()
            .trim(),
        "1"
    );

    let listed = run(f.temp.path(), &["list", "--repo", s(&repo)]).await;
    assert_eq!(listed.exit, Exit::Ok);
    assert_eq!(listed.out.lines().count(), 5, "{}", listed.out);

    // The same bundles again are a no-op, not an error.
    let again = f.import(&media, &repo).await;
    assert_eq!(again.exit, Exit::Ok, "{}", again.all());
    assert_eq!(again.out.matches("already present").count(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_bundle_directory_can_be_verified_and_imported_alone() {
    let f = Fixture::new("single");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let bundle = f.temp.join("media/agent");
    assert_eq!(f.verify(&bundle).await.exit, Exit::Ok);
    let repo = f.temp.join("repo");
    assert_eq!(f.import(&bundle, &repo).await.exit, Exit::Ok);
    assert_eq!(tree(&repo.join("bundles/agent/spot-agent/1.0.0")).len(), 2);
}

// ---- refusals, each with its own code ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_file_is_refused_by_verify_and_import_and_the_repository_is_untouched() {
    let f = Fixture::new("tamper");
    assert_eq!(f.export("media", "1.0.0", "runtime").await.exit, Exit::Ok);
    let repo = f.temp.join("repo");
    assert_eq!(f.import(&f.temp.join("media"), &repo).await.exit, Exit::Ok);
    let before = tree(&repo);

    assert_eq!(f.export("media2", "2.0.0", "runtime").await.exit, Exit::Ok);
    let victim = f.temp.join("media2/runtime/files/bin/spot");
    std::fs::write(&victim, "runtime-binary-TAMPERED").unwrap();
    let verified = f.verify(&f.temp.join("media2")).await;
    assert_eq!(verified.exit, Exit::Refused);
    assert!(
        verified.err.contains("airgap.file_size") || verified.err.contains("airgap.file_digest"),
        "{}",
        verified.err
    );
    let imported = f.import(&f.temp.join("media2"), &repo).await;
    assert_eq!(imported.exit, Exit::Refused);
    assert_eq!(tree(&repo), before, "a refused import changes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_length_flipped_byte_is_a_digest_refusal() {
    let f = Fixture::new("flip");
    assert_eq!(f.export("media", "1.0.0", "runtime").await.exit, Exit::Ok);
    std::fs::write(
        f.temp.join("media/runtime/files/bin/spot"),
        "runtime-binary-9",
    )
    .unwrap();
    let verified = f.verify(&f.temp.join("media")).await;
    assert_eq!(verified.exit, Exit::Refused);
    assert!(
        verified.err.contains("airgap.file_digest"),
        "{}",
        verified.err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_validity_window_is_refused() {
    let f = Fixture::new("expired");
    // A certificate that ends an hour after the export (it rewrites the fixture's authority files,
    // so the bundle is signed with it), verified eight hours later.
    let short = authority(f.temp.path(), NOW.unix_timestamp() + 3_600, 1);
    assert_eq!(f.export("media", "1.0.0", "runtime").await.exit, Exit::Ok);
    let trust = [
        "--trust".to_owned(),
        s(&short.roots).to_owned(),
        "--revocations".to_owned(),
        s(&short.revocations).to_owned(),
    ];
    let media = f.temp.join("media");
    let mut args = vec!["verify", s(&media)];
    args.extend(trust.iter().map(String::as_str));
    let verified = run_at(LATER, f.temp.path(), &args).await;
    assert_eq!(verified.exit, Exit::Refused, "{}", verified.all());
    assert!(
        verified.err.contains("airgap.certificate_expired")
            || verified.err.contains("airgap.root_outside_window"),
        "{}",
        verified.err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsigned_bundle_is_refused_with_its_own_code() {
    let f = Fixture::new("unsigned");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    std::fs::remove_file(f.temp.join("media/agent/offline.sig")).unwrap();
    let verified = f.verify(&f.temp.join("media/agent")).await;
    assert_eq!(verified.exit, Exit::Refused);
    assert!(verified.err.contains("airgap.unsigned"), "{}", verified.err);
    let imported = f
        .import(&f.temp.join("media/agent"), &f.temp.join("repo"))
        .await;
    assert_eq!(imported.exit, Exit::Refused);
    assert!(imported.err.contains("airgap.unsigned"));
    assert!(!f.temp.join("repo/bundles").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bundle_signed_by_an_untrusted_root_is_refused() {
    let f = Fixture::new("untrusted");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let stranger = generate_keypair(&[9u8; 32]);
    std::fs::write(
        &f.authority.roots,
        format!("{}\n", canonical::b64(stranger.public().as_bytes())),
    )
    .unwrap();
    let verified = f.verify(&f.temp.join("media/agent")).await;
    assert_eq!(verified.exit, Exit::Refused, "{}", verified.all());
    assert!(verified.err.contains("airgap."), "{}", verified.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_traversal_in_the_manifest_is_refused() {
    let f = Fixture::new("slip");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let manifest = f.temp.join("media/agent/offline.toml");
    let original = std::fs::read_to_string(&manifest).unwrap();
    for evil in ["../../evil", "/etc/cron.d/evil", "a\\\\b"] {
        let edited = original.replace("path = \"update.json\"", &format!("path = \"{evil}\""));
        assert_ne!(edited, original);
        std::fs::write(&manifest, edited).unwrap();
        let verified = f.verify(&f.temp.join("media/agent")).await;
        assert_eq!(verified.exit, Exit::Refused, "{evil}: {}", verified.all());
        assert!(
            verified.err.contains("airgap.manifest"),
            "{evil}: {}",
            verified.err
        );
        let imported = f
            .import(&f.temp.join("media/agent"), &f.temp.join("repo"))
            .await;
        assert_eq!(imported.exit, Exit::Refused);
    }
    assert!(!f.temp.join("repo/bundles").exists());
    assert!(!f.temp.join("evil").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_the_manifest_does_not_list_is_refused() {
    let f = Fixture::new("extra");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    std::fs::write(f.temp.join("media/agent/files/surprise.sh"), "#!/bin/sh").unwrap();
    let verified = f.verify(&f.temp.join("media/agent")).await;
    assert_eq!(verified.exit, Exit::Refused);
    assert!(
        verified.err.contains("airgap.extra_file"),
        "{}",
        verified.err
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_entry_is_refused_and_never_followed() {
    let f = Fixture::new("symlink");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let secret = f.temp.join("outside.txt");
    std::fs::write(&secret, "agent-archive-1").unwrap();
    let listed = f.temp.join("media/agent/files/spot-agent-update.zip");
    std::fs::remove_file(&listed).unwrap();
    // Even a link to a file with the right bytes is refused.
    std::os::unix::fs::symlink(&secret, &listed).unwrap();
    let verified = f.verify(&f.temp.join("media/agent")).await;
    assert_eq!(verified.exit, Exit::Refused, "{}", verified.all());
    assert!(
        verified.err.contains("airgap.extra_file")
            || verified.err.contains("airgap.media_unreadable"),
        "{}",
        verified.err
    );
    let imported = f
        .import(&f.temp.join("media/agent"), &f.temp.join("repo"))
        .await;
    assert_eq!(imported.exit, Exit::Refused);
    assert!(!f.temp.join("repo/bundles").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlink_in_a_release_directory_stops_the_export() {
    let f = Fixture::new("symlink-export");
    std::os::unix::fs::symlink("/etc/hostname", f.source.join("agent/link")).unwrap();
    let exported = f.export("media", "1.0.0", "agent").await;
    assert_eq!(exported.exit, Exit::Refused, "{}", exported.all());
    assert!(!f.temp.join("media").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downgrade_is_refused_and_the_newer_version_stays() {
    let f = Fixture::new("downgrade");
    let repo = f.temp.join("repo");
    assert_eq!(f.export("new", "2.0.0", "runtime").await.exit, Exit::Ok);
    assert_eq!(f.import(&f.temp.join("new"), &repo).await.exit, Exit::Ok);
    assert_eq!(f.export("old", "1.0.0", "runtime").await.exit, Exit::Ok);
    let before = tree(&repo);
    let imported = f.import(&f.temp.join("old"), &repo).await;
    assert_eq!(imported.exit, Exit::Refused);
    assert!(
        imported.err.contains("airgap.downgrade"),
        "{}",
        imported.err
    );
    assert_eq!(tree(&repo), before);
    let listed = run(f.temp.path(), &["list", "--repo", s(&repo)]).await;
    assert!(
        listed.out.contains("spot-runtime 2.0.0") && !listed.out.contains("1.0.0"),
        "{}",
        listed.out
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_revocation_list_than_the_one_held_is_refused() {
    let f = Fixture::new("stale");
    let repo = f.temp.join("repo");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    // The repository has already seen generation 5.
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("revocation.generation"), "5\n").unwrap();
    let imported = f.import(&f.temp.join("media/agent"), &repo).await;
    assert_eq!(imported.exit, Exit::Refused, "{}", imported.all());
    assert!(
        imported.err.contains("airgap.revocation_stale"),
        "{}",
        imported.err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_trust_configured_everything_is_refused() {
    let f = Fixture::new("notrust");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let media = f.temp.join("media/agent");
    let verified = run(f.temp.path(), &["verify", s(&media)]).await;
    assert_ne!(verified.exit, Exit::Ok);
    let imported = run(
        f.temp.path(),
        &["import", s(&media), "--repo", s(&f.temp.join("repo"))],
    )
    .await;
    assert_ne!(imported.exit, Exit::Ok);
    assert!(!f.temp.join("repo/bundles").exists());
}

// ---- atomic import ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_import_leaves_the_repository_intact_and_the_next_import_finishes() {
    let f = Fixture::new("resume");
    let repo = f.temp.join("repo");
    assert_eq!(f.export("one", "1.0.0", "runtime").await.exit, Exit::Ok);
    assert_eq!(f.import(&f.temp.join("one"), &repo).await.exit, Exit::Ok);
    let before = tree(&repo.join("bundles/runtime/spot-runtime/1.0.0"));

    // A kill mid-import leaves a staging directory with partial files and no commit.
    assert_eq!(f.export("two", "2.0.0", "runtime").await.exit, Exit::Ok);
    let staging = repo.join("staging/runtime/spot-runtime/2.0.0.partial");
    std::fs::create_dir_all(staging.join("bin")).unwrap();
    std::fs::write(staging.join("bin/spot"), "half-writ").unwrap();
    assert_eq!(
        tree(&repo.join("bundles/runtime/spot-runtime/1.0.0")),
        before
    );
    let listed = run(f.temp.path(), &["list", "--repo", s(&repo)]).await;
    assert!(
        !listed.out.contains("2.0.0"),
        "the half import is not a bundle: {}",
        listed.out
    );

    let imported = f.import(&f.temp.join("two"), &repo).await;
    assert_eq!(imported.exit, Exit::Ok, "{}", imported.all());
    assert_eq!(
        std::fs::read_to_string(repo.join("bundles/runtime/spot-runtime/2.0.0/bin/spot")).unwrap(),
        "runtime-binary-1"
    );
    assert_eq!(
        tree(&repo.join("bundles/runtime/spot-runtime/1.0.0")),
        before
    );
}

// ---- export guards ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_takes_only_the_allow_listed_kinds() {
    let f = Fixture::new("allow");
    for what in ["runtime,everything", "etc", "", "../runtime", "keys"] {
        let exported = f.export("media", "1.0.0", what).await;
        assert_eq!(exported.exit, Exit::Usage, "{what}: {}", exported.all());
        assert!(!f.temp.join("media").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_refuses_a_private_key_in_the_release_directory() {
    let f = Fixture::new("secret");
    std::fs::write(f.source.join("agent/agent.key"), "AAAA").unwrap();
    assert_eq!(
        f.export("media", "1.0.0", "agent").await.exit,
        Exit::Refused
    );
    std::fs::remove_file(f.source.join("agent/agent.key")).unwrap();
    std::fs::write(
        f.source.join("agent/notes.txt"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nxx\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .unwrap();
    let exported = f.export("media", "1.0.0", "agent").await;
    assert_eq!(exported.exit, Exit::Refused, "{}", exported.all());
    assert!(exported.err.contains("private key"), "{}", exported.err);
    assert!(!f.temp.join("media").exists());
    assert!(!f.temp.join("media.partial").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_never_overwrites_and_never_writes_inside_the_source() {
    let f = Fixture::new("nooverwrite");
    assert_eq!(f.export("media", "1.0.0", "agent").await.exit, Exit::Ok);
    let again = f.export("media", "1.0.0", "agent").await;
    assert_eq!(again.exit, Exit::Refused, "{}", again.all());
    let inside = run(
        f.temp.path(),
        &[
            "export",
            "--out",
            s(&f.source.join("out")),
            "--what",
            "agent",
            "--source",
            s(&f.source),
            "--version",
            "1.0.0",
            "--key",
            s(&f.authority.key),
            "--certificate",
            s(&f.authority.certificate),
        ],
    )
    .await;
    assert_eq!(inside.exit, Exit::Usage, "{}", inside.all());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_file_readable_by_others_is_refused() {
    let f = Fixture::new("keymode");
    write_private(
        &f.authority.key,
        &format!("{}\n", f.authority.seed_text),
        0o644,
    );
    let exported = f.export("media", "1.0.0", "agent").await;
    assert_eq!(exported.exit, Exit::Refused, "{}", exported.all());
    assert!(exported.err.contains("chmod 600"), "{}", exported.err);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_that_is_not_the_certificates_key_is_refused() {
    let f = Fixture::new("wrongkey");
    let other = generate_keypair(&[7u8; 32]);
    write_private(
        &f.authority.key,
        &format!("{}\n", canonical::b64(&other.to_seed_bytes())),
        0o600,
    );
    let exported = f.export("media", "1.0.0", "agent").await;
    assert_eq!(exported.exit, Exit::Refused, "{}", exported.all());
    assert!(
        exported.err.contains("not the certificate's key"),
        "{}",
        exported.err
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_output_ever_carries_the_signing_seed() {
    let f = Fixture::new("noseed");
    let mut all = String::new();
    let exported = f.export("media", "1.0.0", "runtime,agent").await;
    all.push_str(&exported.all());
    all.push_str(&f.verify(&f.temp.join("media")).await.all());
    all.push_str(
        &f.import(&f.temp.join("media"), &f.temp.join("repo"))
            .await
            .all(),
    );
    all.push_str(
        &run(f.temp.path(), &["list", "--repo", s(&f.temp.join("repo"))])
            .await
            .all(),
    );
    assert!(!all.contains(&f.authority.seed_text), "{all}");
    // And the media and the repository never hold it either.
    for dir in [f.temp.join("media"), f.temp.join("repo")] {
        for entry in tree(&dir) {
            let path = dir.join(entry.split('=').next().unwrap());
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(&f.authority.seed_text),
                "{}",
                path.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_have_the_usage_exit_code() {
    let f = Fixture::new("usage");
    let bad_version = run(
        f.temp.path(),
        &[
            "export",
            "--out",
            s(&f.temp.join("m")),
            "--what",
            "agent",
            "--source",
            s(&f.source),
            "--version",
            "one",
            "--key",
            s(&f.authority.key),
            "--certificate",
            s(&f.authority.certificate),
        ],
    )
    .await;
    assert_eq!(bad_version.exit, Exit::Usage, "{}", bad_version.all());
    let nothing = f.verify(&f.temp.join("empty")).await;
    assert_ne!(nothing.exit, Exit::Ok);
}
