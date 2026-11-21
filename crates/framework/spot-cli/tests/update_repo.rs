//! WP-ADM-12.T3: `spot update --repo` reads an air-gapped runtime bundle, a local repository or a
//! plain update folder, verified as `spot airgap import` verifies, driven over the command registry
//! without a process or a database (the technique of `tests/airgap.rs`).
//!
//! Covered: a runtime bundle on media (the bundle directory, the export directory, its
//! `offline.toml`) is verified and offered; `download` remembers the revocation generation and
//! `check` does not; a tampered file, an unsigned bundle, an untrusted root, a stale revocation list
//! and a downgrade are refused with their `airgap.*` codes and **nothing on disk changes**; the local
//! repository an import fills is read and its files re-checked; a plain update folder and the option
//! being absent behave as before.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::ArgMatches;
use spot_bundle_signing::{
    KeyPair, PublisherCertificate, RevocationBody, generate_keypair, issue_certificate,
    issue_revocation_list,
};
use spot_core::ManualClock;
use spot_updater::{
    Digest, RevokedUpdateKey, SigningKey, UpdateManifest, UpdateRevocation, UpdateRevocationBody,
};
use time::OffsetDateTime;
use time::macros::datetime;

#[path = "../src/commands/mod.rs"]
mod commands;

use commands::{CommandCtx, Exit};

const NOW: OffsetDateTime = datetime!(2026-11-08 12:00:00 UTC);
const SIGNER: u8 = 1;
const REVOKER: u8 = 9;

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

async fn try_run(root: &Path, args: &[&str]) -> Result<Run, spot_cli::Error> {
    let command = commands::augment(
        clap::Command::new("spot").subcommand_required(true),
        commands::COMMANDS,
    )
    .unwrap();
    let mut full = vec!["spot"];
    full.extend_from_slice(args);
    let matches: ArgMatches = command.try_get_matches_from(full).unwrap();
    let (out, err) = (Sink::default(), Sink::default());
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        Arc::new(ManualClock::new(NOW)),
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let exit = commands::run_registered(commands::COMMANDS, &matches, &mut ctx)
        .await
        .ok_or_else(|| spot_cli::Error("no command matched".to_owned()))?;
    Ok(Run {
        exit,
        out: out.text(),
        err: err.text(),
    })
}

async fn run(root: &Path, args: &[&str]) -> Run {
    try_run(root, args).await.unwrap()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A temp directory that is removed at the end of the test.
struct Temp(PathBuf);
impl Temp {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spot-cli-update-repo-{label}-{}-{}",
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

/// Every file under `dir`, relative, sorted: what "nothing changed" compares.
fn listing(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.push(format!(
                    "{} {}",
                    path.strip_prefix(base).unwrap().display(),
                    Digest::of(&bytes).to_hex()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The first file called `name` under `dir`.
fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

fn flip_a_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(path, bytes).unwrap();
}

// --- the publisher authority of the media (spot airgap's trust) ------------------------------------

struct Authority {
    roots: PathBuf,
    revocations: PathBuf,
    key: PathBuf,
    certificate: PathBuf,
}

fn authority(dir: &Path, root_seed: u8, generation: u64) -> Authority {
    let root: KeyPair = generate_keypair(&[root_seed; 32]);
    let publisher = generate_keypair(&[2u8; 32]);
    let certificate = issue_certificate(
        PublisherCertificate {
            publisher_id: "spotledger-test".to_owned(),
            display_name: "Test Publisher".to_owned(),
            key: publisher.public(),
            valid_from: 0,
            valid_to: NOW.unix_timestamp() + 1_000_000,
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
    let out = Authority {
        roots: dir.join("roots.txt"),
        revocations: dir.join("revocations.json"),
        key: dir.join("publisher.seed"),
        certificate: dir.join("certificate.json"),
    };
    std::fs::write(
        &out.roots,
        format!(
            "{}\n",
            spot_bundle_signing::canonical::b64(root.public().as_bytes())
        ),
    )
    .unwrap();
    std::fs::write(&out.revocations, revocations.to_json()).unwrap();
    std::fs::write(
        &out.key,
        format!(
            "{}\n",
            spot_bundle_signing::canonical::b64(&publisher.to_seed_bytes())
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&out.key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    std::fs::write(&out.certificate, certificate.to_json()).unwrap();
    out
}

// --- a release: the update layout `<dir>/stable/...` the updater verifies --------------------------

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// Writes `<dir>/stable/{update.toml, update.sig, revocations.toml, spotledger-V.zip}`, signed by
/// the update key the host's `update-trust.toml` names.
fn publish(dir: &Path, version: &str) {
    let payload = zip_of(&[("bin/spot", b"new-binary")]);
    let manifest = format!(
        "format = \"spot-update-manifest\"\nproduct = \"spotledger\"\nchannel = \"stable\"\nversion = \"{version}\"\nmin_from = \"1.0.0\"\nreleased_at = 1700000000\nschema_epoch = 0\nmigrations = []\nfiles = [{{ path = \"bin/spot\", size = 10, sha256 = \"{}\" }}]\n[payload]\nfile = \"spotledger-{version}.zip\"\nsize = {}\nsha256 = \"{}\"\n",
        Digest::of(b"new-binary").to_hex(),
        payload.len(),
        Digest::of(&payload).to_hex(),
    );
    let signing = key(SIGNER);
    let sig = signing.sign(
        &UpdateManifest::from_toml(&manifest)
            .unwrap()
            .signing_message(),
    );
    let signature = format!(
        "format = \"spot-update-signature\"\nkey_id = \"{}\"\nsignature = \"{}\"\n",
        signing.public().key_id(),
        spot_updater::canonical::b64(&sig)
    );
    let revoker = key(REVOKER);
    let list = UpdateRevocation::issue(
        UpdateRevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: revoker.public().key_id(),
            keys: Vec::<RevokedUpdateKey>::new(),
        },
        &revoker,
    );
    let revocations = format!(
        "format = \"spot-update-revocations\"\ngeneration = 1\nissued_at = 0\nissued_by = \"{}\"\nsignature = \"{}\"\nkeys = []\n",
        list.body.issued_by,
        spot_updater::canonical::b64(list.signature()),
    );
    let stable = dir.join("stable");
    std::fs::create_dir_all(&stable).unwrap();
    std::fs::write(stable.join("update.toml"), manifest).unwrap();
    std::fs::write(stable.join("update.sig"), signature).unwrap();
    std::fs::write(stable.join("revocations.toml"), revocations).unwrap();
    std::fs::write(stable.join(format!("spotledger-{version}.zip")), payload).unwrap();
}

fn update_trust() -> String {
    format!(
        "format = \"spot-update-trust\"\n[[keys]]\npublic = \"{}\"\nvalid_from = 0\n[[keys]]\npublic = \"{}\"\nvalid_from = 0\n",
        spot_updater::canonical::b64(key(SIGNER).public().as_bytes()),
        spot_updater::canonical::b64(key(REVOKER).public().as_bytes()),
    )
}

// --- one host with a runtime bundle exported to media ------------------------------------------------

struct World {
    temp: Temp,
    root: PathBuf,
    authority: Authority,
    /// `<temp>/media/runtime`: the bundle directory `spot airgap export` wrote.
    bundle: PathBuf,
}

impl World {
    /// A host with `installed` running, and version 2.0.0 exported to media.
    async fn new(label: &str, installed: &str) -> World {
        let temp = Temp::new(label);
        let root = temp.join("host");
        std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
        std::fs::write(
            root.join("common.toml"),
            format!(
                "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n",
                root.join("apps/books").display().to_string()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(format!("versions/{installed}"))).unwrap();
        std::fs::write(root.join("current"), installed).unwrap();
        std::fs::write(
            root.join(spot_cli::services::updater::TRUST_FILE),
            update_trust(),
        )
        .unwrap();
        let authority = authority(temp.path(), 1, 1);
        let release = temp.join("release");
        publish(&release.join("runtime"), "2.0.0");
        let out = temp.join("media");
        let exported = run(
            temp.path(),
            &[
                "airgap",
                "export",
                "--out",
                s(&out),
                "--what",
                "runtime",
                "--source",
                s(&release),
                "--version",
                "2.0.0",
                "--key",
                s(&authority.key),
                "--certificate",
                s(&authority.certificate),
            ],
        )
        .await;
        assert_eq!(exported.exit, Exit::Ok, "{}", exported.all());
        let bundle = out.join("runtime");
        World {
            temp,
            root,
            authority,
            bundle,
        }
    }

    /// `spot update <verb> --repo <repo>` with the authority's trust.
    async fn update(&self, verb: &str, repo: &Path) -> Run {
        run(
            &self.root,
            &[
                "update",
                verb,
                "--repo",
                s(repo),
                "--trust",
                s(&self.authority.roots),
                "--revocations",
                s(&self.authority.revocations),
            ],
        )
        .await
    }

    fn held(&self) -> PathBuf {
        self.root.join("revocation.generation")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_bundle_on_media_is_verified_and_offered() {
    let world = World::new("offered", "1.0.0").await;
    let checked = world.update("check", &world.bundle).await;
    assert_eq!(checked.exit, Exit::Ok, "{}", checked.all());
    assert!(
        checked.out.contains("update 2.0.0 (stable) is available"),
        "{}",
        checked.all()
    );
    // A check changes no trust state: the generation is remembered only by what installs.
    assert!(!world.held().exists());

    let downloaded = world.update("download", &world.bundle).await;
    assert_eq!(downloaded.exit, Exit::Ok, "{}", downloaded.all());
    assert!(
        downloaded.out.contains("downloaded and verified"),
        "{}",
        downloaded.all()
    );
    assert_eq!(std::fs::read_to_string(world.held()).unwrap().trim(), "1");
    assert!(
        world
            .root
            .join(spot_cli::services::updater::CACHE_DIR)
            .join("stable/update.toml")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_export_directory_and_the_manifest_file_name_the_same_bundle() {
    let world = World::new("forms", "1.0.0").await;
    let export_dir = world.bundle.parent().unwrap().to_path_buf();
    let manifest = world.bundle.join("offline.toml");
    for repo in [export_dir, manifest] {
        let checked = world.update("check", &repo).await;
        assert_eq!(
            checked.exit,
            Exit::Ok,
            "{}: {}",
            repo.display(),
            checked.all()
        );
        assert!(checked.out.contains("is available"), "{}", checked.all());
    }
    // Some other file is a usage error, not a guess.
    let other = world.bundle.join("certificate.json");
    let usage = world.update("check", &other).await;
    assert_eq!(usage.exit, Exit::Usage, "{}", usage.all());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_bundle_is_refused_and_nothing_on_disk_changes() {
    let world = World::new("tampered", "1.0.0").await;
    flip_a_byte(&world.bundle.join("files/stable/spotledger-2.0.0.zip"));
    let before = listing(world.temp.path());
    for verb in ["check", "download"] {
        let refused = world.update(verb, &world.bundle).await;
        assert_eq!(refused.exit, Exit::Refused, "{}", refused.all());
        assert!(
            refused.err.contains("airgap.file_digest"),
            "{}",
            refused.all()
        );
    }
    assert_eq!(
        listing(world.temp.path()),
        before,
        "a refused repository changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsigned_bundle_an_untrusted_root_and_a_stale_list_are_refused() {
    let world = World::new("untrusted", "1.0.0").await;
    // Another root than the one that signed it.
    let other_dir = world.temp.join("other");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other = authority(&other_dir, 7, 1);
    let run_untrusted = run(
        &world.root,
        &[
            "update",
            "check",
            "--repo",
            s(&world.bundle),
            "--trust",
            s(&other.roots),
            "--revocations",
            s(&other.revocations),
        ],
    )
    .await;
    assert_eq!(run_untrusted.exit, Exit::Refused, "{}", run_untrusted.all());
    assert!(
        run_untrusted.err.contains("airgap."),
        "{}",
        run_untrusted.all()
    );

    // A revocation list older than the one this host already saw.
    std::fs::write(world.held(), "5\n").unwrap();
    let stale = world.update("check", &world.bundle).await;
    assert_eq!(stale.exit, Exit::Refused, "{}", stale.all());
    assert!(stale.err.contains("airgap."), "{}", stale.all());
    std::fs::remove_file(world.held()).unwrap();

    // No signature at all.
    std::fs::remove_file(world.bundle.join("offline.sig")).unwrap();
    let unsigned = world.update("check", &world.bundle).await;
    assert_eq!(unsigned.exit, Exit::Refused, "{}", unsigned.all());
    assert!(
        unsigned.err.contains("airgap.unsigned"),
        "{}",
        unsigned.all()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_trusts_no_publisher_refuses_media() {
    let world = World::new("notrust", "1.0.0").await;
    let refused = run(
        &world.root,
        &["update", "check", "--repo", s(&world.bundle)],
    )
    .await;
    assert_eq!(refused.exit, Exit::Refused, "{}", refused.all());
    assert!(
        refused.err.contains("trusts no publisher"),
        "{}",
        refused.all()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downgrade_is_refused() {
    let world = World::new("downgrade", "3.0.0").await;
    let before = listing(&world.root);
    let refused = world.update("check", &world.bundle).await;
    assert_eq!(refused.exit, Exit::Refused, "{}", refused.all());
    assert!(
        refused.err.contains("airgap.downgrade"),
        "{}",
        refused.all()
    );
    assert_eq!(listing(&world.root), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_local_repository_an_import_fills_is_read_and_its_files_rechecked() {
    let world = World::new("repo", "1.0.0").await;
    let repo = world.temp.join("repository");
    let imported = run(
        world.temp.path(),
        &[
            "airgap",
            "import",
            s(&world.bundle),
            "--repo",
            s(&repo),
            "--trust",
            s(&world.authority.roots),
            "--revocations",
            s(&world.authority.revocations),
        ],
    )
    .await;
    assert_eq!(imported.exit, Exit::Ok, "{}", imported.all());

    // The repository needs no trust flags: it was verified when it was imported, and its files are
    // checked against the index now.
    let checked = run(&world.root, &["update", "check", "--repo", s(&repo)]).await;
    assert_eq!(checked.exit, Exit::Ok, "{}", checked.all());
    assert!(checked.out.contains("is available"), "{}", checked.all());

    let file = find(&repo.join("bundles"), "spotledger-2.0.0.zip").expect("the imported archive");
    flip_a_byte(&file);
    let refused = run(&world.root, &["update", "check", "--repo", s(&repo)]).await;
    assert_eq!(refused.exit, Exit::Refused, "{}", refused.all());
    assert!(
        refused.err.contains("airgap.file_digest"),
        "{}",
        refused.all()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_update_folder_and_the_option_being_absent_behave_as_before() {
    let world = World::new("plain", "1.0.0").await;
    let plain = world.temp.join("plain");
    publish(&plain, "2.0.0");
    let checked = run(&world.root, &["update", "check", "--repo", s(&plain)]).await;
    assert_eq!(checked.exit, Exit::Ok, "{}", checked.all());
    assert!(
        checked.out.contains("update 2.0.0 (stable) is available"),
        "{}",
        checked.all()
    );

    // Without --repo and without a configured server there is nowhere to look.
    let none = run(&world.root, &["update", "check"]).await;
    assert_ne!(none.exit, Exit::Ok, "{}", none.all());
    assert!(none.all().contains("no update source"), "{}", none.all());
}
