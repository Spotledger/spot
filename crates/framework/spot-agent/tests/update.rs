//! WP-ADM-03.T3 tests: the agent's self-update over `spot-updater`: every refusal case, a good
//! update, a bad new binary that rolls back, a power cut at every filesystem operation, the
//! revocation list rules, the wire form, the repository source and the service seam.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Cursor, Write as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use semver::Version;
use serde_json::{Value, json};
use spot_agent::update::runner::UpdateConfig;
use spot_agent::update::service::{AgentService, Probe, ProbeState, QuiesceFlag, Restarter};
use spot_agent::update::source::{DirSource, UpdateSource};
use spot_agent::update::trust::{MemoryRevocationStore, RevocationStore, list_is_trusted};
use spot_agent::update::wire::{parse_offer, revocation_to_json};
use spot_agent::update::{CODE_ROLLED_BACK, SelfUpdater, UpdateError, UpdateReport, UpdateStatus};
use spot_core::SystemClock;
use spot_updater::testing::{FakeFileSystem, FakeService};
use spot_updater::{
    Channel, Digest, Layout, RevokedUpdateKey, SigningKey, TrustedUpdateKey, UpdateFile,
    UpdateManifest, UpdatePayload, UpdateRevocation, UpdateRevocationBody, UpdateSignature,
    UpdateTrust, UpdaterErrorCode,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const NOW: i64 = 1_800_000_000;
const SIGNER: u8 = 1;
const REVOKER: u8 = 9;
const OTHER: u8 = 5;

const OLD_FILES: &[(&str, &[u8])] = &[("spot-agent", b"old-binary"), ("share/notice", b"old")];
const NEW_FILES: &[(&str, &[u8])] = &[("spot-agent", b"new-binary"), ("share/notice", b"new")];

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32])
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn anchor() -> UpdateTrust {
    UpdateTrust::new(vec![
        TrustedUpdateKey::new(key(SIGNER).public(), 0, None),
        TrustedUpdateKey::new(key(REVOKER).public(), 0, None),
    ])
}

fn revocation(generation: u64, revoked: &[u8]) -> UpdateRevocation {
    let signing = key(REVOKER);
    UpdateRevocation::issue(
        UpdateRevocationBody {
            generation,
            issued_at: 0,
            issued_by: signing.public().key_id(),
            keys: revoked
                .iter()
                .map(|seed| RevokedUpdateKey {
                    key_id: key(*seed).public().key_id(),
                    reason: "compromised".to_owned(),
                })
                .collect(),
        },
        &signing,
    )
}

fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in files {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn manifest_over(version: &str, files: &[(&str, &[u8])], archive: &[u8]) -> UpdateManifest {
    UpdateManifest {
        product: "spot-agent".to_owned(),
        channel: Channel::Stable,
        version: v(version),
        min_from: v("1.0.0"),
        via: Vec::new(),
        released_at: 1_700_000_000,
        payload: UpdatePayload {
            file: format!("spot-agent-{version}.zip"),
            size: archive.len() as u64,
            sha256: Digest::of(archive),
        },
        files: files
            .iter()
            .map(|(path, bytes)| UpdateFile {
                path: (*path).to_owned(),
                size: bytes.len() as u64,
                sha256: Digest::of(bytes),
            })
            .collect(),
        migrations: Vec::new(),
        requires_os: None,
        schema_epoch: 0,
        notes: None,
    }
}

fn sign_with(manifest: &UpdateManifest, seed: u8) -> UpdateSignature {
    let signing = key(seed);
    UpdateSignature::from_parts(
        signing.public().key_id(),
        signing.sign(&manifest.signing_message()),
    )
}

fn manifest_json(m: &UpdateManifest) -> Value {
    json!({
        "product": m.product,
        "channel": m.channel.as_str(),
        "version": m.version.to_string(),
        "min_from": m.min_from.to_string(),
        "via": m.via.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "released_at": m.released_at,
        "payload": {"file": m.payload.file, "size": m.payload.size, "sha256": m.payload.sha256.to_hex()},
        "files": m.files.iter().map(|f| json!({"path": f.path, "size": f.size, "sha256": f.sha256.to_hex()})).collect::<Vec<_>>(),
        "migrations": m.migrations,
        "requires_os": m.requires_os,
        "schema_epoch": m.schema_epoch,
    })
}

fn proof_of(signature: &UpdateSignature) -> String {
    format!(
        "format = \"spot-update-signature\"\nkey_id = \"{}\"\nsignature = \"{}\"\n",
        signature.key_id().as_str(),
        STANDARD.encode(signature.signature())
    )
}

fn offer_value(
    manifest: &UpdateManifest,
    signature: Option<&UpdateSignature>,
    revocations: Option<&UpdateRevocation>,
) -> Value {
    let mut value = json!({"manifest": manifest_json(manifest)});
    if let Some(signature) = signature {
        value["proof"] = json!(proof_of(signature));
    }
    if let Some(list) = revocations {
        value["revocations"] = revocation_to_json(list);
    }
    value
}

/// A source with a scripted offer and payload that counts payload fetches.
struct ScriptedSource {
    offer: Mutex<Result<Value, UpdateError>>,
    payload: Vec<u8>,
    payload_calls: AtomicUsize,
}

impl ScriptedSource {
    fn new(offer: Value, payload: Vec<u8>) -> Self {
        Self {
            offer: Mutex::new(Ok(offer)),
            payload,
            payload_calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.payload_calls.load(Ordering::SeqCst)
    }
}

impl UpdateSource for ScriptedSource {
    fn offer(&self, _channel: Option<&str>, _now: i64) -> Result<Value, UpdateError> {
        self.offer.lock().unwrap().clone()
    }

    fn payload(&self, _file: &str, _size: u64, _now: i64) -> Result<Vec<u8>, UpdateError> {
        self.payload_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.payload.clone())
    }
}

#[derive(Default)]
struct CountingRestarter(AtomicUsize);

impl Restarter for CountingRestarter {
    fn restart(&self) -> Result<(), UpdateError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct World {
    fs: FakeFileSystem,
    service: FakeService,
    source: ScriptedSource,
    store: MemoryRevocationStore,
    config: UpdateConfig,
    trust: UpdateTrust,
    restarter: CountingRestarter,
    layout: Layout,
}

impl World {
    /// Installed 1.0.0 and running; the gateway offers `manifest` signed by `signer` (if any).
    fn with(
        installed: &str,
        manifest: &UpdateManifest,
        signature: Option<&UpdateSignature>,
        archive: Vec<u8>,
        offered_list: Option<&UpdateRevocation>,
        stored_list: Option<UpdateRevocation>,
    ) -> Self {
        let layout = Layout::new("/opt/spot-agent");
        let fs = FakeFileSystem::new();
        fs.seed_version(&layout, &v(installed), OLD_FILES);
        fs.seed_current(&layout, &v(installed));
        let mut config = UpdateConfig::new(v(installed), "/opt/spot-agent");
        config.platform = "linux".to_owned();
        Self {
            fs,
            service: FakeService::new(Some(v(installed))),
            source: ScriptedSource::new(offer_value(manifest, signature, offered_list), archive),
            store: stored_list.map_or_else(MemoryRevocationStore::new, MemoryRevocationStore::with),
            config,
            trust: anchor(),
            restarter: CountingRestarter::default(),
            layout,
        }
    }

    /// The good case: 2.0.0 signed by the trusted key, a generation-1 list stored.
    fn good() -> Self {
        let archive = make_zip(NEW_FILES);
        let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
        let signature = sign_with(&manifest, SIGNER);
        Self::with(
            "1.0.0",
            &manifest,
            Some(&signature),
            archive,
            None,
            Some(revocation(1, &[])),
        )
    }

    fn updater(&self) -> SelfUpdater<'_> {
        SelfUpdater::new(
            &self.config,
            &self.fs,
            &self.service,
            &self.source,
            &self.restarter,
            &self.trust,
            &self.store,
        )
    }

    fn current(&self) -> Option<Version> {
        self.fs.pointer(&self.layout.current_path())
    }

    fn assert_untouched(&self) {
        assert_eq!(self.current(), Some(v("1.0.0")));
        assert_eq!(self.fs.pointer(&self.layout.previous_path()), None);
        assert!(!self.fs.has_dir(&self.layout.version_dir(&v("2.0.0"))));
        assert!(!self.fs.has_dir(&self.layout.staging_path(&v("2.0.0"))));
    }
}

fn refusal(report: &UpdateReport) -> String {
    match &report.status {
        UpdateStatus::Refused(code) => code.strip_prefix("refused: ").unwrap_or(code).to_owned(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[allow(dead_code)]
fn refusal_of(code: UpdaterErrorCode) -> String {
    format!("refused: {}", code.as_str())
}

// ---- the good path ----------------------------------------------------------------------

#[test]
fn a_good_update_installs_reports_the_new_version_and_asks_for_a_restart() {
    let world = World::good();
    let report = world.updater().check(NOW);
    assert_eq!(report.status, UpdateStatus::Applied(v("2.0.0")));
    assert!(report.needs_restart);
    assert_eq!(report.outcome(), "succeeded");
    assert_eq!(report.code, None);
    assert_eq!(world.current(), Some(v("2.0.0")));
    assert_eq!(
        world.fs.pointer(&world.layout.previous_path()),
        Some(v("1.0.0"))
    );
    assert_eq!(
        world
            .fs
            .file(&world.layout.version_file(&v("2.0.0"), "spot-agent")),
        Some(b"new-binary".to_vec())
    );
    // The old version stays on disk for the rollback.
    assert!(world.fs.has_dir(&world.layout.version_dir(&v("1.0.0"))));
    assert_eq!(world.source.calls(), 1);
    world.updater().restart().unwrap();
    assert_eq!(world.restarter.0.load(Ordering::SeqCst), 1);
}

#[test]
fn no_published_update_is_not_an_error() {
    let world = World::good();
    *world.source.offer.lock().unwrap() = Err(UpdateError::Unavailable);
    let report = world.updater().check(NOW);
    assert_eq!(report.status, UpdateStatus::NoUpdate);
    assert_eq!(report.outcome(), "succeeded");
    world.assert_untouched();
}

// ---- refusals: nothing is downloaded, nothing on disk changes -----------------------------

#[test]
fn an_unsigned_manifest_is_refused_and_never_downloaded() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let world = World::with(
        "1.0.0",
        &manifest,
        None,
        archive,
        None,
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::Unsigned.as_str());
    assert_eq!(world.source.calls(), 0);
    world.assert_untouched();
}

#[test]
fn a_signature_by_a_key_the_anchor_does_not_trust_is_refused() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, OTHER);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::KeyUnknown.as_str());
    assert_eq!(world.source.calls(), 0);
    world.assert_untouched();
}

#[test]
fn a_signature_outside_the_key_window_is_refused_before_and_after_it() {
    for (from, to, expected) in [
        (NOW + 100, None, UpdaterErrorCode::KeyNotYetValid),
        (0, Some(NOW - 100), UpdaterErrorCode::KeyExpired),
    ] {
        let mut world = World::good();
        world.trust = UpdateTrust::new(vec![
            TrustedUpdateKey::new(key(SIGNER).public(), from, to),
            TrustedUpdateKey::new(key(REVOKER).public(), 0, None),
        ]);
        let report = world.updater().check(NOW);
        assert_eq!(refusal(&report), expected.as_str());
        assert_eq!(world.source.calls(), 0);
        world.assert_untouched();
    }
}

#[test]
fn a_revoked_signing_key_is_refused() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(2, &[SIGNER])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::KeyRevoked.as_str());
    world.assert_untouched();
}

#[test]
fn a_manifest_changed_after_signing_is_refused() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let mut forged = manifest.clone();
    forged.notes = Some("changed after signing".to_owned());
    let mut value = offer_value(&forged, Some(&signature), None);
    value["manifest"]["notes"] = json!("changed after signing");
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(1, &[])),
    );
    *world.source.offer.lock().unwrap() = Ok(value);
    let report = world.updater().check(NOW);
    assert_eq!(
        refusal(&report),
        UpdaterErrorCode::SignatureInvalid.as_str()
    );
    world.assert_untouched();
}

#[test]
fn a_lower_version_is_refused_and_so_is_a_signed_one() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("0.9.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::Downgrade.as_str());
    assert_eq!(world.source.calls(), 0);
    world.assert_untouched();
}

#[test]
fn the_same_version_is_a_no_op() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("1.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(report.status, UpdateStatus::UpToDate);
    assert_eq!(world.source.calls(), 0);
}

#[test]
fn a_payload_that_does_not_match_the_signed_digest_is_refused() {
    let mut world = World::good();
    let mut tampered = make_zip(NEW_FILES);
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    world.source.payload = tampered;
    let report = world.updater().check(NOW);
    assert_eq!(
        refusal(&report),
        UpdaterErrorCode::PayloadDigestMismatch.as_str()
    );
    world.assert_untouched();
}

#[test]
fn a_zip_slip_entry_is_refused() {
    for name in ["../evil", "/etc/cron.d/evil", "bin/../../evil"] {
        let files: &[(&str, &[u8])] = &[(name, b"x")];
        let archive = make_zip(files);
        let manifest = manifest_over("2.0.0", files, &archive);
        let signature = sign_with(&manifest, SIGNER);
        let world = World::with(
            "1.0.0",
            &manifest,
            Some(&signature),
            archive,
            None,
            Some(revocation(1, &[])),
        );
        let report = world.updater().check(NOW);
        assert_eq!(
            refusal(&report),
            UpdaterErrorCode::ArchiveEscape.as_str(),
            "{name}"
        );
        world.assert_untouched();
    }
}

#[test]
fn an_archive_entry_the_manifest_does_not_list_is_refused() {
    let archive = make_zip(&[("spot-agent", b"new-binary"), ("extra", b"surprise")]);
    let listed: &[(&str, &[u8])] = &[("spot-agent", b"new-binary")];
    let manifest = manifest_over("2.0.0", listed, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        None,
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::ArchiveUnlisted.as_str());
    world.assert_untouched();
}

#[test]
fn an_oversized_payload_is_refused_before_it_is_fetched() {
    let mut world = World::good();
    world.config.max_payload = 16;
    let report = world.updater().check(NOW);
    assert_eq!(refusal(&report), UpdaterErrorCode::ArchiveBomb.as_str());
    assert_eq!(world.source.calls(), 0);
}

#[test]
fn a_manifest_for_another_product_channel_or_platform_is_refused() {
    let archive = make_zip(NEW_FILES);
    let mut other = manifest_over("2.0.0", NEW_FILES, &archive);
    other.product = "spotledger".to_owned();
    let mut beta = manifest_over("2.0.0", NEW_FILES, &archive);
    beta.channel = Channel::Beta;
    let mut windows = manifest_over("2.0.0", NEW_FILES, &archive);
    windows.requires_os = Some("Windows Server 2022".to_owned());
    let mut migrating = manifest_over("2.0.0", NEW_FILES, &archive);
    migrating.schema_epoch = 4;
    migrating.migrations = vec![0];
    for (manifest, expected) in [
        (other, UpdaterErrorCode::ProductMismatch),
        (beta, UpdaterErrorCode::ChannelNotAllowed),
        (windows, UpdaterErrorCode::ProductMismatch),
        (migrating, UpdaterErrorCode::SchemaEpoch),
    ] {
        let signature = sign_with(&manifest, SIGNER);
        let world = World::with(
            "1.0.0",
            &manifest,
            Some(&signature),
            archive.clone(),
            None,
            Some(revocation(1, &[])),
        );
        let report = world.updater().check(NOW);
        assert_eq!(refusal(&report), expected.as_str());
        assert_eq!(world.source.calls(), 0);
        world.assert_untouched();
    }
}

#[test]
fn a_missing_install_layout_is_a_failure_not_a_panic() {
    let mut world = World::good();
    world.config.root = "/nowhere".into();
    world.fs = FakeFileSystem::new();
    let report = world.updater().check(NOW);
    assert!(
        matches!(report.status, UpdateStatus::Failed(_)),
        "{report:?}"
    );
    assert_eq!(report.outcome(), "failed");
}

#[test]
fn a_pointer_ahead_of_the_running_version_waits_for_its_restart() {
    let mut world = World::good();
    world.config.running = v("0.9.0");
    let report = world.updater().check(NOW);
    assert_eq!(report.status, UpdateStatus::RestartPending);
    assert_eq!(world.source.calls(), 0);
}

// ---- revocation lists --------------------------------------------------------------------

#[test]
fn without_any_revocation_list_the_update_fails_closed() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let world = World::with("1.0.0", &manifest, Some(&signature), archive, None, None);
    let report = world.updater().check(NOW);
    assert!(matches!(report.status, UpdateStatus::Refused(_)));
    assert_eq!(world.source.calls(), 0);
    world.assert_untouched();
}

#[test]
fn a_newer_list_that_came_with_the_offer_is_stored_before_anything_installs() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    let offered = revocation(7, &[OTHER]);
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        Some(&offered),
        Some(revocation(1, &[])),
    );
    let report = world.updater().check(NOW);
    assert_eq!(report.status, UpdateStatus::Applied(v("2.0.0")));
    assert_eq!(world.store.load().unwrap().unwrap().body.generation, 7);
}

#[test]
fn an_older_or_forged_list_never_replaces_the_stored_one() {
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    // Older: generation 3 against a stored 5.
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive.clone(),
        Some(&revocation(3, &[])),
        Some(revocation(5, &[])),
    );
    let _ = world.updater().check(NOW);
    assert_eq!(world.store.load().unwrap().unwrap().body.generation, 5);
    // Forged: higher generation, signed by a key the anchor does not trust.
    let forger = key(OTHER);
    let forged = UpdateRevocation::issue(
        UpdateRevocationBody {
            generation: 99,
            issued_at: 0,
            issued_by: forger.public().key_id(),
            keys: Vec::new(),
        },
        &forger,
    );
    assert!(!list_is_trusted(&anchor(), &forged, NOW));
    let world = World::with(
        "1.0.0",
        &manifest,
        Some(&signature),
        archive,
        Some(&forged),
        Some(revocation(5, &[])),
    );
    let _ = world.updater().check(NOW);
    assert_eq!(world.store.load().unwrap().unwrap().body.generation, 5);
}

// ---- rollback on a bad new version ---------------------------------------------------------

#[test]
fn a_new_version_that_fails_its_health_check_rolls_back_and_reports_it() {
    let world = World::good();
    world.service.not_ready_once();
    let report = world.updater().check(NOW);
    assert!(
        matches!(report.status, UpdateStatus::RolledBack { .. }),
        "{report:?}"
    );
    assert_eq!(report.code, Some(CODE_ROLLED_BACK));
    assert_eq!(report.outcome(), "failed");
    assert!(!report.needs_restart);
    assert_eq!(world.current(), Some(v("1.0.0")));
    assert_eq!(world.service.running(), Some(v("1.0.0")));
    assert!(!world.fs.has_dir(&world.layout.version_dir(&v("2.0.0"))));
}

#[test]
fn an_unhealthy_service_after_the_switch_rolls_back() {
    let world = World::good();
    world.service.unhealthy();
    let report = world.updater().check(NOW);
    // The rollback restarts the old version, which the unhealthy fake still reports as
    // unhealthy: the engine ends needing attention or rolled back, never applied.
    assert!(
        matches!(
            report.status,
            UpdateStatus::RolledBack { .. } | UpdateStatus::NeedsAttention(_)
        ),
        "{report:?}"
    );
    assert!(!report.needs_restart);
}

// ---- power cut at every filesystem operation ------------------------------------------------

#[test]
fn a_power_cut_at_every_operation_resumes_into_the_old_or_the_new_version_never_a_mix() {
    let total = {
        let world = World::good();
        assert!(matches!(
            world.updater().check(NOW).status,
            UpdateStatus::Applied(_)
        ));
        world.fs.ops()
    };
    assert!(total > 10);
    for n in 0..=total {
        let world = World::good();
        world.fs.cut_after(n);
        let _ = world.updater().check(NOW);

        world.fs.reboot();
        let down = FakeService::down();
        let updater = SelfUpdater::new(
            &world.config,
            &world.fs,
            &down,
            &world.source,
            &world.restarter,
            &world.trust,
            &world.store,
        );
        let _ = updater.boot(NOW + 1);

        let current = world
            .current()
            .unwrap_or_else(|| panic!("cut {n}: no pointer"));
        assert!(
            current == v("1.0.0") || current == v("2.0.0"),
            "cut {n}: {current}"
        );
        assert!(
            world.fs.has_dir(&world.layout.version_dir(&current)),
            "cut {n}"
        );
        let files = if current == v("2.0.0") {
            NEW_FILES
        } else {
            OLD_FILES
        };
        for (path, bytes) in files {
            assert_eq!(
                world.fs.file(&world.layout.version_file(&current, path)),
                Some(bytes.to_vec()),
                "cut {n}: {path}"
            );
        }
        assert!(
            !world.fs.has_dir(&world.layout.staging_path(&v("2.0.0"))),
            "cut {n}: staging"
        );
    }
}

#[test]
fn a_second_boot_after_recovery_has_nothing_to_do() {
    let world = World::good();
    world.fs.cut_after(20);
    let _ = world.updater().check(NOW);
    world.fs.reboot();
    let down = FakeService::down();
    let updater = SelfUpdater::new(
        &world.config,
        &world.fs,
        &down,
        &world.source,
        &world.restarter,
        &world.trust,
        &world.store,
    );
    let _ = updater.boot(NOW + 1);
    let later = updater.boot(NOW + 100_000);
    assert!(
        matches!(later.status, UpdateStatus::UpToDate | UpdateStatus::Kept(_)),
        "{later:?}"
    );
}

// ---- the repository source ------------------------------------------------------------------

#[test]
fn a_repository_directory_serves_an_offer_and_its_payload() {
    let dir = tempfile::tempdir().unwrap();
    let archive = make_zip(NEW_FILES);
    let manifest = manifest_over("2.0.0", NEW_FILES, &archive);
    let signature = sign_with(&manifest, SIGNER);
    std::fs::write(
        dir.path().join("update.json"),
        offer_value(&manifest, Some(&signature), Some(&revocation(1, &[]))).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join(&manifest.payload.file), &archive).unwrap();
    let source = DirSource::new(dir.path());

    let mut world = World::good();
    let data = source.offer(None, NOW).unwrap();
    assert!(parse_offer(&data).is_ok());
    assert_eq!(
        source
            .payload(&manifest.payload.file, archive.len() as u64, NOW)
            .unwrap(),
        archive
    );
    // A payload whose length differs from the signed size is refused.
    assert!(
        source
            .payload(&manifest.payload.file, archive.len() as u64 + 1, NOW)
            .is_err()
    );
    assert!(source.payload(&manifest.payload.file, 4, NOW).is_err());
    world.source = ScriptedSource::new(data, archive);
    assert_eq!(
        world.updater().check(NOW).status,
        UpdateStatus::Applied(v("2.0.0"))
    );
}

// ---- the service seam -------------------------------------------------------------------------

/// A probe whose answer the test sets.
struct ScriptedProbe {
    state: Mutex<ProbeState>,
    started: Mutex<Vec<Version>>,
    aborted: AtomicUsize,
}

impl ScriptedProbe {
    fn new(state: ProbeState) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
            started: Mutex::new(Vec::new()),
            aborted: AtomicUsize::new(0),
        })
    }
}

impl Probe for ScriptedProbe {
    fn start(&self, version: &Version) -> Result<(), UpdateError> {
        self.started.lock().unwrap().push(version.clone());
        Ok(())
    }

    fn state(&self) -> ProbeState {
        self.state.lock().unwrap().clone()
    }

    fn abort(&self) {
        self.aborted.fetch_add(1, Ordering::SeqCst);
    }
}

fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn agent_world(probe: Arc<ScriptedProbe>) -> (World, AgentService, Arc<QuiesceFlag>) {
    let mut world = World::good();
    // `released_at` is far in the past, so the real clock is fine for the decision.
    world.config.policy.health_timeout_secs = 1;
    let flag = Arc::new(QuiesceFlag::new());
    let service = AgentService::new(
        v("1.0.0"),
        flag.clone(),
        probe,
        Arc::new(SystemClock::new()),
    )
    .with_poll(std::time::Duration::from_millis(10));
    (world, service, flag)
}

#[test]
fn the_agent_service_probes_the_new_version_while_the_old_one_keeps_running() {
    let probe = ScriptedProbe::new(ProbeState::Healthy);
    let (world, service, flag) = agent_world(probe.clone());
    let updater = SelfUpdater::new(
        &world.config,
        &world.fs,
        &service,
        &world.source,
        &world.restarter,
        &world.trust,
        &world.store,
    );
    let report = updater.check(real_now());
    assert_eq!(
        report.status,
        UpdateStatus::Applied(v("2.0.0")),
        "{report:?}"
    );
    assert_eq!(*probe.started.lock().unwrap(), vec![v("2.0.0")]);
    // Intake stays paused until the restart into the new version.
    assert!(flag.is_paused());
}

#[test]
fn a_failed_probe_rolls_back_and_intake_resumes() {
    let probe = ScriptedProbe::new(ProbeState::Failed("exit 1".to_owned()));
    let (world, service, flag) = agent_world(probe);
    let updater = SelfUpdater::new(
        &world.config,
        &world.fs,
        &service,
        &world.source,
        &world.restarter,
        &world.trust,
        &world.store,
    );
    let report = updater.check(real_now());
    assert_eq!(report.code, Some(CODE_ROLLED_BACK), "{report:?}");
    assert_eq!(world.current(), Some(v("1.0.0")));
    assert!(!flag.is_paused(), "the old version takes commands again");
}

#[test]
fn a_probe_that_never_finishes_is_aborted_at_the_deadline_and_rolled_back() {
    let probe = ScriptedProbe::new(ProbeState::Running);
    let (world, service, flag) = agent_world(probe.clone());
    let updater = SelfUpdater::new(
        &world.config,
        &world.fs,
        &service,
        &world.source,
        &world.restarter,
        &world.trust,
        &world.store,
    );
    let report = updater.check(real_now());
    assert_eq!(report.code, Some(CODE_ROLLED_BACK), "{report:?}");
    assert!(probe.aborted.load(Ordering::SeqCst) >= 1);
    assert!(!flag.is_paused());
    assert_eq!(world.current(), Some(v("1.0.0")));
}
