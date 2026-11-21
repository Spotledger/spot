//! WP-PRD-03.T5: `spot update` and the updater service. The engine and the verification are
//! `spot-updater`'s; these tests prove the host side: the HTTPS source (a local server on a
//! generated certificate authority), the trust anchor and revocation files, the refusals with
//! their stable codes (each leaving the installed version untouched), apply over the engine's
//! in-memory seams, rollback, power cuts at every operation, and the service's mode, window and
//! gate. The process tests need PostgreSQL (`SPOT_TEST_PG_URL`) and print one `SKIPPED` line
//! without it.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use semver::Version;
use spot_cli::services::updater::{
    self as up, Cycle, HttpsClient, Mode, Settings, Source, Updater, Window,
};
use spot_core::ManualClock;
use spot_updater::canonical::b64;
use spot_updater::testing::{FakeDataHooks, FakeFileSystem, FakeService};
use spot_updater::{
    Channel, Decision, Digest, FileSystem, Layout, Outcome, RevokedUpdateKey, SigningKey, Step,
    UpdateEngine, UpdateManifest, UpdateRevocation, UpdateRevocationBody, UpdaterError,
    UpdaterErrorCode,
};
use time::macros::datetime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const NOW: i64 = 1_800_000_000;
const SIGNER: u8 = 1;
const REVOKER: u8 = 9;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

// --- a release: manifest, signature, revocations, archive ------------------------------------

#[derive(Clone)]
struct Release {
    version: String,
    epoch: u64,
    migrations: Vec<u64>,
    payload: Vec<u8>,
    /// The bytes the manifest's digest and size describe (the archive as built).
    declared: Vec<u8>,
    signer: u8,
    sign_version: Option<String>,
    revoked: Vec<u8>,
    generation: u64,
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

impl Release {
    fn new(version: &str) -> Self {
        let payload = zip_of(&[("bin/spot", b"new-binary")]);
        Self {
            version: version.to_owned(),
            epoch: 2,
            migrations: Vec::new(),
            declared: payload.clone(),
            payload,
            signer: SIGNER,
            sign_version: None,
            revoked: Vec::new(),
            generation: 1,
        }
    }

    fn manifest_text(&self, version: &str) -> String {
        let file = format!("spotledger-{version}.zip");
        format!(
            "format = \"spot-update-manifest\"\nproduct = \"spotledger\"\nchannel = \"stable\"\nversion = \"{version}\"\nmin_from = \"1.0.0\"\nreleased_at = 1700000000\nschema_epoch = {}\nmigrations = {:?}\nfiles = [{{ path = \"bin/spot\", size = 10, sha256 = \"{}\" }}]\n[payload]\nfile = \"{file}\"\nsize = {}\nsha256 = \"{}\"\n",
            self.epoch,
            self.migrations,
            Digest::of(b"new-binary").to_hex(),
            self.declared.len(),
            Digest::of(&self.declared).to_hex(),
        )
    }

    fn texts(&self) -> (String, String, String) {
        let manifest = self.manifest_text(&self.version);
        let signed = self.manifest_text(self.sign_version.as_deref().unwrap_or(&self.version));
        let signing = key(self.signer);
        let sig = signing.sign(
            &UpdateManifest::from_toml(&signed)
                .unwrap()
                .signing_message(),
        );
        let signature = format!(
            "format = \"spot-update-signature\"\nkey_id = \"{}\"\nsignature = \"{}\"\n",
            signing.public().key_id(),
            b64(&sig)
        );
        let revoker = key(REVOKER);
        let list = UpdateRevocation::issue(
            UpdateRevocationBody {
                generation: self.generation,
                issued_at: 0,
                issued_by: revoker.public().key_id(),
                keys: self
                    .revoked
                    .iter()
                    .map(|seed| RevokedUpdateKey {
                        key_id: key(*seed).public().key_id(),
                        reason: "compromised".to_owned(),
                    })
                    .collect(),
            },
            &revoker,
        );
        let keys: Vec<String> = list
            .body
            .keys
            .iter()
            .map(|k| format!("{{ key_id = \"{}\", reason = \"{}\" }}", k.key_id, k.reason))
            .collect();
        let revocations = format!(
            "format = \"spot-update-revocations\"\ngeneration = {}\nissued_at = 0\nissued_by = \"{}\"\nsignature = \"{}\"\nkeys = [{}]\n",
            self.generation,
            list.body.issued_by,
            b64(list.signature()),
            keys.join(", ")
        );
        (manifest, signature, revocations)
    }

    /// Writes the repository layout `<dir>/stable/...`.
    fn publish(&self, dir: &Path) {
        let (manifest, signature, revocations) = self.texts();
        let stable = dir.join("stable");
        std::fs::create_dir_all(&stable).unwrap();
        std::fs::write(stable.join("update.toml"), manifest).unwrap();
        std::fs::write(stable.join("update.sig"), signature).unwrap();
        std::fs::write(stable.join("revocations.toml"), revocations).unwrap();
        std::fs::write(
            stable.join(format!("spotledger-{}.zip", self.version)),
            &self.payload,
        )
        .unwrap();
    }
}

fn trust_text(signer_valid_to: Option<i64>) -> String {
    let end = signer_valid_to.map_or(String::new(), |t| format!("valid_to = {t}\n"));
    format!(
        "format = \"spot-update-trust\"\n[[keys]]\npublic = \"{}\"\nvalid_from = 0\n{end}[[keys]]\npublic = \"{}\"\nvalid_from = 0\n",
        b64(key(SIGNER).public().as_bytes()),
        b64(key(REVOKER).public().as_bytes()),
    )
}

// --- the in-memory world an apply runs in --------------------------------------------------------

struct World {
    fs: FakeFileSystem,
    layout: Layout,
    repo: tempfile::TempDir,
    settings: Settings,
}

fn world(release: &Release, installed: &str, signer_valid_to: Option<i64>) -> World {
    let fs = FakeFileSystem::new();
    let layout = Layout::new("/opt/spot");
    fs.seed_version(&layout, &v(installed), &[("bin/spot", b"old-binary")]);
    fs.seed_current(&layout, &v(installed));
    fs.put_file(
        &layout.root().join(up::TRUST_FILE),
        trust_text(signer_valid_to).as_bytes(),
    );
    let repo = tempfile::tempdir().unwrap();
    release.publish(repo.path());
    World {
        fs,
        layout,
        repo,
        settings: Settings {
            schema_epoch: 2,
            ..Settings::default()
        },
    }
}

impl World {
    fn source(&self) -> Source {
        Source::Dir(self.repo.path().to_path_buf())
    }

    fn current(&self) -> Option<Version> {
        self.fs.pointer(&self.layout.current_path())
    }

    /// Check, then apply when the decision says so. `Err` is a verification refusal.
    async fn run(
        &self,
        service: &FakeService,
        hooks: &FakeDataHooks,
    ) -> Result<Outcome, UpdaterError> {
        let installed = up::installed(&self.fs, &self.layout, &self.settings, Channel::Stable)?;
        let policy = self.settings.policy(Channel::Stable);
        let source = self.source();
        let root = self.layout.root();
        let (offer, decision) =
            up::check(&source, &self.fs, root, &installed, &policy, NOW).await?;
        match decision {
            Decision::Refuse(code) => Ok(Outcome::Refused(code)),
            Decision::Skip(reason) => Ok(Outcome::Skipped(reason)),
            Decision::StepVia(version) => Ok(Outcome::StepVia(version)),
            Decision::Apply => {
                let payload = source
                    .read(
                        Channel::Stable,
                        &offer.manifest.payload.file,
                        offer.manifest.payload.size,
                    )
                    .await?;
                let trust = up::load_trust(&self.fs, root)?;
                let seams = up::Seams {
                    fs: &self.fs,
                    service,
                    hooks,
                };
                Ok(up::apply(
                    &seams,
                    &self.layout,
                    (&trust, installed, &offer),
                    (payload.as_slice(), policy, NOW),
                ))
            }
        }
    }

    /// The installed version is untouched: same pointer, no new version directory, the service
    /// was never stopped.
    fn assert_untouched(&self, service: &FakeService, installed: &str, offered: &str) {
        assert_eq!(self.current(), Some(v(installed)));
        assert!(!self.fs.has_dir(&self.layout.version_dir(&v(offered))));
        assert_eq!(service.stops(), 0);
    }
}

fn running(installed: &str) -> FakeService {
    FakeService::new(Some(v(installed)))
}

fn code_of(error: &UpdaterError) -> &'static str {
    error.code().as_str()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_good_update_applies_and_the_host_records_it() {
    let release = Release::new("2.0.0");
    let w = world(&release, "1.0.0", None);
    let (service, hooks) = (running("1.0.0"), FakeDataHooks::new());
    let outcome = w.run(&service, &hooks).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Applied {
            version: v("2.0.0")
        }
    );
    assert_eq!(w.current(), Some(v("2.0.0")));
    assert_eq!(
        w.fs.pointer(&w.layout.previous_path()),
        Some(v("1.0.0")),
        "the old version is kept for a rollback"
    );
    assert_eq!(service.running(), Some(v("2.0.0")));
    // The installed version now records its schema epoch and digest; the revocation generation
    // is remembered, so an older list is refused next time.
    let state =
        String::from_utf8(w.fs.file(&w.layout.root().join(up::STATE_FILE)).unwrap()).unwrap();
    assert!(state.contains("schema_epoch = 2"), "{state}");
    assert_eq!(up::held_generation(&w.fs, w.layout.root()), 1);
    // The same offer is now "already installed", not applied twice.
    let again = w.run(&running("2.0.0"), &hooks).await.unwrap();
    assert!(matches!(again, Outcome::Skipped(_)), "{again:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_signed_by_a_key_the_anchor_does_not_hold_is_refused() {
    let mut release = Release::new("2.0.0");
    release.signer = 5;
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    let error = w.run(&service, &FakeDataHooks::new()).await.unwrap_err();
    assert_eq!(code_of(&error), "updater.key_unknown");
    w.assert_untouched(&service, "1.0.0", "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signature_over_other_bytes_is_refused() {
    let mut release = Release::new("2.0.0");
    release.sign_version = Some("2.0.1".to_owned());
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    let error = w.run(&service, &FakeDataHooks::new()).await.unwrap_err();
    assert_eq!(code_of(&error), "updater.signature");
    w.assert_untouched(&service, "1.0.0", "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_outside_its_window_is_refused() {
    let release = Release::new("2.0.0");
    let w = world(&release, "1.0.0", Some(NOW - 10));
    let service = running("1.0.0");
    let error = w.run(&service, &FakeDataHooks::new()).await.unwrap_err();
    assert_eq!(code_of(&error), "updater.key_expired");
    w.assert_untouched(&service, "1.0.0", "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_key_is_refused_and_an_older_list_is_refused_after_a_newer_one() {
    let mut release = Release::new("2.0.0");
    release.revoked = vec![SIGNER];
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    let error = w.run(&service, &FakeDataHooks::new()).await.unwrap_err();
    assert_eq!(code_of(&error), "updater.key_revoked");
    w.assert_untouched(&service, "1.0.0", "2.0.0");

    // A good release published under generation 5 is accepted and remembered; then the same
    // release under generation 1 (a replayed older list) is refused.
    let mut newer = Release::new("2.0.0");
    newer.generation = 5;
    newer.publish(w.repo.path());
    w.run(&service, &FakeDataHooks::new()).await.unwrap();
    assert_eq!(up::held_generation(&w.fs, w.layout.root()), 5);
    let mut replay = Release::new("2.1.0");
    replay.generation = 1;
    replay.publish(w.repo.path());
    let error = w
        .run(&running("2.0.0"), &FakeDataHooks::new())
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "updater.revocation_stale");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_whose_digest_differs_is_refused_before_anything_is_staged() {
    let mut release = Release::new("2.0.0");
    let mut tampered = release.payload.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    release.payload = tampered; // same size, different bytes
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    let outcome = w.run(&service, &FakeDataHooks::new()).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::Refused(UpdaterErrorCode::PayloadDigestMismatch)
    );
    w.assert_untouched(&service, "1.0.0", "2.0.0");
    assert!(!w.fs.has_dir(&w.layout.staging_path(&v("2.0.0"))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_downgrade_is_refused() {
    let release = Release::new("2.0.0");
    let w = world(&release, "3.0.0", None);
    let service = running("3.0.0");
    let outcome = w.run(&service, &FakeDataHooks::new()).await.unwrap();
    assert_eq!(outcome, Outcome::Refused(UpdaterErrorCode::Downgrade));
    w.assert_untouched(&service, "3.0.0", "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_entry_that_escapes_staging_is_refused() {
    let mut release = Release::new("2.0.0");
    let slip = zip_of(&[("bin/spot", b"new-binary"), ("../evil", b"x")]);
    release.declared = slip.clone();
    release.payload = slip;
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    let outcome = w.run(&service, &FakeDataHooks::new()).await.unwrap();
    assert_eq!(outcome, Outcome::Refused(UpdaterErrorCode::ArchiveEscape));
    w.assert_untouched(&service, "1.0.0", "2.0.0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_health_check_rolls_back_to_the_installed_version() {
    let release = Release::new("2.0.0");
    let w = world(&release, "1.0.0", None);
    let service = running("1.0.0");
    service.unhealthy();
    let outcome = w.run(&service, &FakeDataHooks::new()).await.unwrap();
    match outcome {
        Outcome::RolledBack { failed_step, .. } => assert_eq!(failed_step, Step::Health),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(w.current(), Some(v("1.0.0")));
    assert_eq!(service.running(), Some(v("1.0.0")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_backup_aborts_the_update_before_the_schema_is_touched() {
    let mut release = Release::new("2.0.0");
    release.epoch = 3;
    release.migrations = vec![2];
    let w = world(&release, "1.0.0", None);
    let (service, hooks) = (running("1.0.0"), FakeDataHooks::new());
    hooks.fail_backup();
    let outcome = w.run(&service, &hooks).await.unwrap();
    match outcome {
        Outcome::RolledBack { failed_step, .. } => assert_eq!(failed_step, Step::Backup),
        other => panic!("expected a rollback, got {other:?}"),
    }
    assert_eq!(hooks.migrations(), 0, "the schema was never touched");
    assert_eq!(w.current(), Some(v("1.0.0")));
    assert_eq!(service.running(), Some(v("1.0.0")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_power_cut_at_every_operation_resumes_to_the_old_or_the_new_version() {
    let release = Release::new("2.0.0");
    let mut saw_old = false;
    let mut saw_new = false;
    for cut in 1..=600_u64 {
        let w = world(&release, "1.0.0", None);
        let (service, hooks) = (running("1.0.0"), FakeDataHooks::new());
        w.fs.cut_after(cut);
        let first = w.run(&service, &hooks).await;
        // A cut during the check itself is an error before any change; later ones interrupt.
        if !matches!(first, Ok(Outcome::Interrupted { .. })) {
            continue;
        }
        w.fs.reboot();
        let engine = UpdateEngine::new(&w.fs, &service, &hooks, w.layout.clone());
        let _ = engine.resume(NOW);
        let current = w.current().unwrap();
        assert!(
            current == v("1.0.0") || current == v("2.0.0"),
            "cut {cut}: {current}"
        );
        assert!(
            w.fs.has_dir(&w.layout.version_dir(&current)),
            "cut {cut}: the pointer names a missing directory"
        );
        saw_old |= current == v("1.0.0");
        saw_new |= current == v("2.0.0");
    }
    // The pointer swap is the last operation of an update, so a cut can leave the old version or
    // (when something follows the swap) the new one; the run that is never cut ends on the new.
    assert!(saw_old, "the cut points must reach the old outcome");
    let w = world(&release, "1.0.0", None);
    let (service, hooks) = (running("1.0.0"), FakeDataHooks::new());
    let finished = w.run(&service, &hooks).await;
    assert!(finished.is_ok(), "an uncut update completes: {finished:?}");
    assert_eq!(w.current(), Some(v("2.0.0")));
    let _ = saw_new;
}

// --- settings, window, URL ----------------------------------------------------------------------

#[test]
fn the_defaults_look_but_do_not_change_anything() {
    let parsed = Settings::default().validate().unwrap();
    assert_eq!(parsed.mode, Mode::Notify);
    assert_eq!(parsed.channel, Channel::Stable);
    assert!(parsed.url.is_none() && parsed.window.is_none());
}

#[test]
fn only_an_https_url_without_credentials_is_accepted() {
    for text in [
        "http://updates.example.com/",
        "ftp://updates.example.com/",
        "https://user:pw@updates.example.com/",
        "https://",
        "not a url",
    ] {
        let settings = Settings {
            url: Some(text.to_owned()),
            ..Settings::default()
        };
        let error = settings.validate().unwrap_err().to_string();
        assert!(error.starts_with("services.updater.url:"), "{error}");
        assert!(!error.contains("pw"), "a credential reached the message");
    }
    let ok = Settings {
        url: Some("https://updates.example.com/spot".to_owned()),
        ..Settings::default()
    };
    assert!(ok.validate().is_ok());
}

#[test]
fn unknown_settings_and_bad_values_are_refused() {
    let unknown: Result<Settings, _> = toml::from_str("mod = 'auto'");
    assert!(unknown.is_err());
    for text in [
        "mode = 'sometimes'",
        "channel = 'nightly'",
        "window = '25:00-26:00'",
        "window = '02:00-02:00'",
        "health_timeout_secs = 0",
    ] {
        let settings: Settings = toml::from_str(text).unwrap();
        assert!(settings.validate().is_err(), "{text}");
    }
}

#[test]
fn a_window_may_cross_midnight() {
    let day = Window::parse("02:00-05:00").unwrap();
    let hour = |h: i64, m: i64| h * 3600 + m * 60;
    assert!(day.contains(hour(2, 0)) && day.contains(hour(4, 59)));
    assert!(!day.contains(hour(5, 0)) && !day.contains(hour(1, 59)));
    let night = Window::parse("22:00-02:00").unwrap();
    assert!(night.contains(hour(23, 30)) && night.contains(hour(1, 0)));
    assert!(!night.contains(hour(12, 0)));
    // Any day: Unix seconds modulo a day.
    assert!(day.contains(NOW - NOW.rem_euclid(86_400) + hour(3, 0)));
}

// --- the HTTPS source ---------------------------------------------------------------------------

struct Authority {
    params: CertificateParams,
    key: KeyPair,
    certificate: rcgen::Certificate,
}

fn authority() -> Authority {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "spot update test CA");
    let key = KeyPair::generate().unwrap();
    let certificate = params.self_signed(&key).unwrap();
    Authority {
        params,
        key,
        certificate,
    }
}

type Routes = BTreeMap<String, (u16, Vec<u8>)>;

struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn base(&self) -> url::Url {
        url::Url::parse(&format!("https://127.0.0.1:{}/spot/", self.addr.port())).unwrap()
    }
}

async fn serve(authority: &Authority, routes: Routes) -> Server {
    let issuer = Issuer::from_params(&authority.params, &authority.key);
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = params.signed_by(&leaf_key, &issuer).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![leaf.der().clone()],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (routes, log) = (Arc::new(routes), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, routes, log) = (acceptor.clone(), routes.clone(), log.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buffer = vec![0_u8; 4096];
                let Ok(n) = tls.read(&mut buffer).await else {
                    return;
                };
                let head = String::from_utf8_lossy(&buffer[..n]).into_owned();
                let target = head.split(' ').nth(1).unwrap_or("/").to_owned();
                log.lock().unwrap().push(target.clone());
                let path = target.split('?').next().unwrap_or("/");
                let (status, body) = routes
                    .get(path)
                    .cloned()
                    .unwrap_or((404, b"missing".to_vec()));
                let location = if status == 302 {
                    "Location: http://127.0.0.1:9/elsewhere\r\n"
                } else {
                    ""
                };
                let reply = format!(
                    "HTTP/1.0 {status} X\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(reply.as_bytes()).await;
                let _ = tls.write_all(&body).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    Server { addr, seen }
}

fn client(authority: &Authority) -> HttpsClient {
    HttpsClient::with_roots(vec![authority.certificate.der().to_vec()]).unwrap()
}

fn routes_of(release: &Release) -> Routes {
    let (manifest, signature, revocations) = release.texts();
    let mut routes = Routes::new();
    routes.insert(
        "/spot/stable/update.toml".into(),
        (200, manifest.into_bytes()),
    );
    routes.insert(
        "/spot/stable/update.sig".into(),
        (200, signature.into_bytes()),
    );
    routes.insert(
        "/spot/stable/revocations.toml".into(),
        (200, revocations.into_bytes()),
    );
    routes.insert(
        format!("/spot/stable/spotledger-{}.zip", release.version),
        (200, release.payload.clone()),
    );
    routes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_is_fetched_over_https_verified_and_decided() {
    let release = Release::new("2.0.0");
    let ca = authority();
    let server = serve(&ca, routes_of(&release)).await;
    let w = world(&release, "1.0.0", None);
    let source = Source::Https {
        base: server.base(),
        client: client(&ca),
        query: None,
    };
    let installed = up::installed(&w.fs, &w.layout, &w.settings, Channel::Stable).unwrap();
    let policy = w.settings.policy(Channel::Stable);
    let (offer, decision) = up::check(&source, &w.fs, w.layout.root(), &installed, &policy, NOW)
        .await
        .unwrap();
    assert_eq!(decision, Decision::Apply);
    assert_eq!(offer.manifest.version, v("2.0.0"));
    let payload = source
        .read(
            Channel::Stable,
            &offer.manifest.payload.file,
            offer.manifest.payload.size,
        )
        .await
        .unwrap();
    assert_eq!(payload, release.payload);
    // The default request carries no query at all: nothing about the installation is sent.
    assert!(
        server.seen.lock().unwrap().iter().all(|t| !t.contains('?')),
        "{:?}",
        server.seen.lock().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_opt_in_query_is_all_the_server_ever_sees() {
    let release = Release::new("2.0.0");
    let ca = authority();
    let server = serve(&ca, routes_of(&release)).await;
    let source = Source::Https {
        base: server.base(),
        client: client(&ca),
        query: Some("v=1.0.0&p=linux-x86_64&c=stable".to_owned()),
    };
    source
        .read(Channel::Stable, "update.toml", 1 << 20)
        .await
        .unwrap();
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        ["/spot/stable/update.toml?v=1.0.0&p=linux-x86_64&c=stable"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_the_client_does_not_trust_is_not_talked_to() {
    let release = Release::new("2.0.0");
    let (ca, other) = (authority(), authority());
    let server = serve(&ca, routes_of(&release)).await;
    let source = Source::Https {
        base: server.base(),
        client: client(&other),
        query: None,
    };
    let error = source
        .read(Channel::Stable, "update.toml", 1 << 20)
        .await
        .unwrap_err();
    assert_eq!(code_of(&error), "updater.io");
    assert!(
        server.seen.lock().unwrap().is_empty(),
        "no request was sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_an_error_and_is_never_followed() {
    let ca = authority();
    let mut routes = Routes::new();
    routes.insert("/spot/stable/update.toml".into(), (302, Vec::new()));
    let server = serve(&ca, routes).await;
    let source = Source::Https {
        base: server.base(),
        client: client(&ca),
        query: None,
    };
    let error = source
        .read(Channel::Stable, "update.toml", 1 << 20)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("redirect"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_http_url_and_an_oversized_body_are_refused() {
    let ca = authority();
    let c = client(&ca);
    let plain = url::Url::parse("http://127.0.0.1:9/x").unwrap();
    assert_eq!(code_of(&c.get(&plain, 10).await.unwrap_err()), "updater.io");
    let mut routes = Routes::new();
    routes.insert("/spot/stable/big".into(), (200, vec![0_u8; 5000]));
    let server = serve(&ca, routes).await;
    let source = Source::Https {
        base: server.base(),
        client: c,
        query: None,
    };
    let error = source.read(Channel::Stable, "big", 100).await.unwrap_err();
    assert!(error.to_string().contains("larger"), "{error}");
    // A name that is not one plain file name never becomes a path.
    for name in ["../x", "a/b", "..", "c:evil"] {
        let error = source.read(Channel::Stable, name, 100).await.unwrap_err();
        assert_eq!(code_of(&error), "updater.archive_escape", "{name}");
    }
}

// --- the service -----------------------------------------------------------------------------------

#[derive(Default)]
struct CountingLauncher(AtomicUsize);

impl up::Launcher for CountingLauncher {
    fn launch(&self, _host_root: &Path, _channel: Channel) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Rig {
    dir: tempfile::TempDir,
    repo: tempfile::TempDir,
    launcher: Arc<CountingLauncher>,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    Release::new("2.0.0").publish(repo.path());
    std::fs::create_dir_all(dir.path().join("versions/1.0.0")).unwrap();
    std::fs::write(dir.path().join("current"), "1.0.0").unwrap();
    std::fs::write(dir.path().join(up::TRUST_FILE), trust_text(None)).unwrap();
    Rig {
        dir,
        repo,
        launcher: Arc::default(),
    }
}

impl Rig {
    fn updater(&self, text: &str, clock: time::OffsetDateTime, idle: bool) -> Updater {
        let settings: Settings = toml::from_str(&format!("schema_epoch = 2\n{text}")).unwrap();
        Updater {
            parsed: settings.validate().unwrap(),
            host_root: self.dir.path().to_path_buf(),
            layout: Layout::new(self.dir.path()),
            source: Source::Dir(self.repo.path().to_path_buf()),
            fs: Arc::new(spot_updater::StdFileSystem::new()),
            clock: Arc::new(ManualClock::new(clock)),
            launcher: self.launcher.clone(),
            gate: Arc::new(move || idle),
            settings,
        }
    }

    fn launched(&self) -> usize {
        self.launcher.0.load(Ordering::SeqCst)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_records_the_update_and_changes_nothing() {
    let rig = rig();
    let updater = rig.updater("mode = 'notify'", datetime!(2026-11-12 03:00 UTC), true);
    assert_eq!(updater.cycle().await, Cycle::Available(v("2.0.0")));
    assert_eq!(rig.launched(), 0);
    let status = up::read_status(&spot_updater::StdFileSystem::new(), rig.dir.path()).unwrap();
    assert_eq!(
        (status.state.as_str(), status.available.as_deref()),
        ("available", Some("2.0.0"))
    );
    // At most once a day.
    assert_eq!(updater.cycle().await, Cycle::TooSoon);
    assert_eq!(
        std::fs::read_to_string(rig.dir.path().join("current")).unwrap(),
        "1.0.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_outside_the_window_does_nothing() {
    let rig = rig();
    let outside = rig.updater(
        "mode = 'auto'\nwindow = '02:00-05:00'",
        datetime!(2026-11-12 12:00 UTC),
        true,
    );
    assert!(matches!(outside.cycle().await, Cycle::Deferred(_, _)));
    assert_eq!(rig.launched(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_inside_the_window_starts_the_apply_when_the_host_is_idle() {
    let rig = rig();
    let busy = rig.updater(
        "mode = 'auto'\nwindow = '02:00-05:00'",
        datetime!(2026-11-12 03:00 UTC),
        false,
    );
    assert!(matches!(busy.cycle().await, Cycle::Deferred(_, _)));
    assert_eq!(rig.launched(), 0, "a job lock or a migration holds it back");

    // The deferred look is recorded as a look; a day later, idle, inside the window.
    let idle = rig.updater(
        "mode = 'auto'\nwindow = '02:00-05:00'",
        datetime!(2026-11-13 03:00 UTC),
        true,
    );
    assert_eq!(idle.cycle().await, Cycle::Launched(v("2.0.0")));
    assert_eq!(rig.launched(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_never_looks_and_a_bad_offer_is_recorded_as_failed() {
    let rig = rig();
    let off = rig.updater("mode = 'off'", datetime!(2026-11-12 03:00 UTC), true);
    assert_eq!(off.cycle().await, Cycle::Off);
    assert!(up::read_status(&spot_updater::StdFileSystem::new(), rig.dir.path()).is_none());

    std::fs::write(rig.dir.path().join(up::TRUST_FILE), "format = 'wrong'").unwrap();
    let notify = rig.updater("", datetime!(2026-11-12 03:00 UTC), true);
    assert_eq!(
        notify.cycle().await,
        Cycle::Failed("updater.key_malformed".to_owned())
    );
    let status = up::read_status(&spot_updater::StdFileSystem::new(), rig.dir.path()).unwrap();
    assert_eq!(status.state, "failed");
    assert_eq!(rig.launched(), 0);
}

// --- the command, through the binary ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_checks_plans_and_reports_against_a_repository() {
    let Some(env) =
        common::Env::new("[services.updater]\nmode = 'notify'\nschema_epoch = 2\n").await
    else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    let root = env.root.0.clone();
    std::fs::create_dir_all(root.join("versions/1.0.0")).unwrap();
    std::fs::write(root.join("current"), "1.0.0").unwrap();
    std::fs::write(root.join(up::TRUST_FILE), trust_text(None)).unwrap();
    let repo = tempfile::tempdir().unwrap();
    Release::new("2.0.0").publish(repo.path());
    let repo_arg = repo.path().display().to_string();

    let out = common::run(&root, &["update", "check", "--repo", &repo_arg], None);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("update 2.0.0 (stable) is available"),
        "{text}"
    );

    let out = common::run(&root, &["update", "apply", "--repo", &repo_arg], None);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("Nothing was changed"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("current")).unwrap(),
        "1.0.0"
    );

    let out = common::run(&root, &["update", "status"], None);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("installed: 1.0.0"), "{text}");
    assert!(text.contains("last check: available"), "{text}");

    // A tampered manifest is refused with its code (exit 2), and nothing is recorded as available.
    let mut bad = Release::new("2.0.0");
    bad.sign_version = Some("2.0.1".to_owned());
    bad.publish(repo.path());
    let out = common::run(&root, &["update", "check", "--repo", &repo_arg], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("updater.signature"));
}
