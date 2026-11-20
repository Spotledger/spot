//! The licence client of a tenant host (WP-CTL-04.T3): offline load and verification, every
//! standing and what the gate answers in it, the clock guard across a restart, renewal against a
//! scripted control plane, atomic files, registration, and the `spot licence` exit codes.
//!
//! Licences are issued with `spot_license::issuing::Issuer` and a throwaway key. The control plane
//! is a scripted `LicenceFetcher`; the mutual-TLS handshake belongs to `HttpSender::with_tls`
//! (WP-SEC-09.T8) and is tested there. The `spot licence` process tests need PostgreSQL and skip
//! without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use ed25519_dalek::SigningKey;
use spot_cli::services::licence::files::{self, parse_trusted_keys};
use spot_cli::services::licence::renew::{
    FetchError, Fetched, LicenceFetcher, interpret, parse_response, renew_once, wants_renewal,
};
use spot_cli::services::licence::{
    HostConfig, LicenceEgress, LicenceGate, LicenceHost, LicencePorts, LicenceService,
    LicenceStateSource, Settings, Standing, code, register, state_code_of,
};
use spot_cli::services::{Level, SiteCtx};
use spot_core::ManualClock;
use spot_license::issuing::Issuer;
use spot_license::model::{
    Edition, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
};
use spot_license::{Action, Status, TrustedKeys};
use time::OffsetDateTime;

const KEY_ID: &str = "issuer-test";
const ISSUED: i64 = 1_770_000_000;
const EXPIRES: i64 = 1_772_683_200;
const RENEW_AFTER: i64 = EXPIRES - 7 * 86_400;
const GRACE: u64 = 7 * 86_400;

static DIRS: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("spot_lic_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signing_key() -> SigningKey {
    let mut seed = [7_u8; 32];
    seed[0] = 1;
    SigningKey::from_bytes(&seed)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn keys_text() -> String {
    format!(
        "# the issuer\n{KEY_ID} {} 0 -\n",
        hex(&signing_key().verifying_key().to_bytes())
    )
}

fn trusted() -> TrustedKeys {
    parse_trusted_keys(&keys_text()).unwrap()
}

fn licence(id: &str, tenant: &str, serial: u64, kind: Kind) -> Licence {
    let trial = kind == Kind::Trial;
    Licence::new(RawLicence {
        v: PAYLOAD_VERSION,
        licence_id: id.to_owned(),
        tenant: Some(tenant.to_owned()),
        installation_id: None,
        edition: Edition::new("business").unwrap(),
        ent: Entitlements::from_lists(
            Some(Limit::UpTo(25)),
            vec!["bookkeeping".to_owned()],
            vec!["export".to_owned()],
        ),
        kind,
        issued_at: ISSUED,
        not_before: ISSUED,
        expires_at: Some(EXPIRES),
        renew_after: Some(RENEW_AFTER),
        grace: if trial { 0 } else { GRACE },
        serial,
        key_id: KEY_ID.to_owned(),
    })
    .unwrap()
}

fn issue(licence: &Licence) -> String {
    Issuer::new(signing_key(), KEY_ID)
        .sign(licence)
        .unwrap()
        .to_text()
        .unwrap()
}

fn sub(serial: u64) -> String {
    issue(&licence("lic-1", "acme", serial, Kind::Subscription))
}

fn clock_at(epoch: i64) -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(epoch).unwrap(),
    ))
}

/// Writes a file the licence host accepts whatever the umask of the test process is (a group
/// writable default, as on a shared runner, is refused by the host as `file_unsafe`).
fn write_plain(path: &Path, text: impl AsRef<[u8]>) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

struct Site {
    dir: TempDir,
}

impl Site {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        write_plain(&dir.path().join("keys.txt"), keys_text());
        Self { dir }
    }
    fn licence_file(&self) -> PathBuf {
        self.dir.path().join("licence.txt")
    }
    fn state_file(&self) -> PathBuf {
        self.dir.path().join("clock.state")
    }
    fn put(&self, text: &str) {
        write_plain(&self.licence_file(), format!("{text}\n"));
    }
    fn config(&self, required: bool) -> HostConfig {
        HostConfig::new(required, "acme", self.licence_file(), self.state_file())
    }
    fn host(&self, required: bool, clock: &Arc<ManualClock>) -> Result<LicenceHost, String> {
        LicenceHost::load(self.config(required), trusted(), clock.clone())
            .map_err(|e| e.to_string())
    }
}

fn standing_at(site: &Site, epoch: i64) -> Standing {
    let clock = clock_at(epoch);
    LicenceGate::standing(&site.host(false, &clock).unwrap())
}

// ---- the standings and what the gate answers ----------------------------------------------

#[test]
fn every_standing_of_a_subscription_and_what_the_gate_allows() {
    let site = Site::new("standings");
    site.put(&sub(1));

    assert_eq!(
        standing_at(&site, ISSUED - 10),
        Standing::Licensed(Status::NotYetValid)
    );
    assert_eq!(
        standing_at(&site, ISSUED + 100),
        Standing::Licensed(Status::Active)
    );
    assert_eq!(
        standing_at(&site, RENEW_AFTER + 1),
        Standing::Licensed(Status::RenewalDue)
    );
    assert!(matches!(
        standing_at(&site, EXPIRES + 86_400),
        Standing::Licensed(Status::Grace { .. })
    ));
    assert_eq!(
        standing_at(&site, EXPIRES + GRACE as i64 + 86_400),
        Standing::Licensed(Status::Restricted)
    );
}

#[test]
fn a_restricted_licence_denies_new_documents_and_allows_export() {
    let site = Site::new("restricted");
    site.put(&sub(1));
    let clock = clock_at(EXPIRES + GRACE as i64 + 86_400);
    let host = site.host(true, &clock).unwrap();
    assert_eq!(
        host.decide(Action::CreateDocument).reason(),
        Some("licence.restricted")
    );
    assert!(host.decide(Action::ExportData).is_allow());
    assert!(host.decide(Action::Renew).is_allow());
    assert!(!host.decide(Action::InstallApp).is_allow());
}

#[test]
fn an_expired_trial_allows_nothing_and_a_grace_licence_allows_everything() {
    let site = Site::new("trial");
    site.put(&issue(&licence("lic-t", "acme", 1, Kind::Trial)));
    let host = site.host(true, &clock_at(EXPIRES + 1)).unwrap();
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Expired)
    );
    for action in Action::all() {
        assert!(!host.decide(action).is_allow(), "{action:?}");
    }
    let site = Site::new("grace");
    site.put(&sub(1));
    let host = site.host(true, &clock_at(EXPIRES + 3_600)).unwrap();
    for action in Action::all() {
        assert!(host.decide(action).is_allow(), "{action:?}");
    }
}

#[test]
fn entitlements_are_those_of_the_verified_licence() {
    let site = Site::new("ent");
    site.put(&sub(1));
    let host = site.host(false, &clock_at(ISSUED + 10)).unwrap();
    let ent = host.entitlements().unwrap();
    assert_eq!(ent.apps(), ["bookkeeping".to_owned()]);
    assert_eq!(ent.users(), Limit::UpTo(25));
}

// ---- unlicensed, tampered, foreign ---------------------------------------------------------

#[test]
fn no_licence_file_is_unlicensed_open_by_default_and_closed_when_required() {
    let site = Site::new("unlicensed");
    let open = site.host(false, &clock_at(ISSUED)).unwrap();
    assert_eq!(LicenceGate::standing(&open), Standing::Unlicensed);
    for action in Action::all() {
        assert!(open.decide(action).is_allow());
    }
    assert_eq!(open.state_code(), None, "no licence, no heartbeat signal");

    let closed = site.host(true, &clock_at(ISSUED)).unwrap();
    assert!(closed.decide(Action::Renew).is_allow());
    for action in [
        Action::UsePlatform,
        Action::CreateDocument,
        Action::ExportData,
        Action::InstallApp,
    ] {
        assert_eq!(closed.decide(action).reason(), Some(code::UNLICENSED));
    }
}

fn tampered(text: &str) -> String {
    let (head, signature) = text.rsplit_once('.').unwrap();
    let mut chars: Vec<char> = signature.chars().collect();
    chars[3] = if chars[3] == 'A' { 'B' } else { 'A' };
    format!("{head}.{}", chars.into_iter().collect::<String>())
}

#[test]
fn a_tampered_licence_refuses_start_when_required_and_degrades_otherwise() {
    let site = Site::new("tamper");
    site.put(&tampered(&sub(1)));
    let error = site.host(true, &clock_at(ISSUED + 10)).err().unwrap();
    assert!(error.contains("licence."), "{error}");
    assert!(!error.contains(&sub(1)), "no licence text in an error");

    let host = site.host(false, &clock_at(ISSUED + 10)).unwrap();
    assert_eq!(LicenceGate::standing(&host), Standing::Invalid);
    assert!(host.invalid().is_some());
    for action in Action::all() {
        assert!(host.decide(action).is_allow(), "optional: never blocks");
    }
    assert_eq!(host.state_code(), Some(2));
}

#[test]
fn a_licence_for_another_tenant_is_refused_like_a_tampered_one() {
    let site = Site::new("foreign");
    site.put(&issue(&licence("lic-x", "other", 1, Kind::Subscription)));
    let error = site.host(true, &clock_at(ISSUED + 10)).err().unwrap();
    assert!(error.contains(code::TENANT_MISMATCH), "{error}");
    let host = site.host(false, &clock_at(ISSUED + 10)).unwrap();
    assert_eq!(LicenceGate::standing(&host), Standing::Invalid);
}

#[test]
fn a_foreign_issuer_key_is_not_trusted() {
    let site = Site::new("key");
    let mut seed = [9_u8; 32];
    seed[0] = 2;
    let other = Issuer::new(SigningKey::from_bytes(&seed), KEY_ID);
    let text = other
        .sign(&licence("lic-1", "acme", 1, Kind::Subscription))
        .unwrap()
        .to_text()
        .unwrap();
    site.put(&text);
    assert!(site.host(true, &clock_at(ISSUED + 10)).is_err());
}

#[cfg(unix)]
#[test]
fn a_licence_or_state_file_writable_by_others_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let site = Site::new("mode");
    site.put(&sub(1));
    std::fs::set_permissions(site.licence_file(), std::fs::Permissions::from_mode(0o666)).unwrap();
    let error = site.host(false, &clock_at(ISSUED + 10)).err().unwrap();
    assert!(error.contains(code::FILE_UNSAFE), "{error}");
    std::fs::set_permissions(site.licence_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    write_plain(&site.state_file(), "1 1\n");
    std::fs::set_permissions(site.state_file(), std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(site.host(false, &clock_at(ISSUED + 10)).is_err());
}

// ---- the clock guard ------------------------------------------------------------------------

#[test]
fn a_clock_moved_back_is_suspect_and_the_guard_survives_a_restart() {
    let site = Site::new("clock");
    site.put(&sub(1));
    let day = 86_400;
    {
        let host = site.host(false, &clock_at(ISSUED + 20 * day)).unwrap();
        assert_eq!(
            LicenceGate::standing(&host),
            Standing::Licensed(Status::Active)
        );
        host.flush().unwrap();
    }
    assert!(site.state_file().exists(), "the observation was persisted");
    // Restart with the clock set back three days: beyond the one-day tolerance.
    let clock = clock_at(ISSUED + 17 * day);
    let host = site.host(false, &clock).unwrap();
    assert!(matches!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::ClockSuspect { .. })
    ));
    // It behaves like grace: everything still allowed, with the warning.
    assert!(host.decide(Action::CreateDocument).is_allow());
    assert_eq!(host.state_code(), Some(1));
    // A renewal re-anchors the guard. `ClockGuard::reanchor` never lowers the mark, so a clock that
    // is still behind stays suspect until it passes the mark again: advance it, and it is plain.
    host.install_text(&sub(2), true, &[]).unwrap();
    clock.advance(Duration::from_secs(4 * 86_400));
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );
    // A small correction inside the tolerance is absorbed.
    let site2 = Site::new("clock2");
    site2.put(&sub(1));
    {
        let host = site2.host(false, &clock_at(ISSUED + 20 * day)).unwrap();
        let _ = LicenceGate::standing(&host);
        host.flush().unwrap();
    }
    let host = site2
        .host(false, &clock_at(ISSUED + 20 * day - 3_600))
        .unwrap();
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );
}

// ---- install, atomic files ------------------------------------------------------------------

#[test]
fn install_refuses_a_lower_or_equal_serial_and_accepts_a_higher_one() {
    let site = Site::new("install");
    let host = site.host(false, &clock_at(ISSUED + 100)).unwrap();
    let first = host.install_text(&sub(3), false, &[]).unwrap();
    assert_eq!(first.serial, 3);
    assert!(
        std::fs::read_to_string(site.licence_file())
            .unwrap()
            .starts_with("SPOT-LICENCE-1")
    );
    for stale in [2, 3] {
        let error = host.install_text(&sub(stale), false, &[]).unwrap_err();
        assert_eq!(error.code, "licence.stale");
    }
    assert_eq!(host.licence().unwrap().serial, 3);
    assert_eq!(host.install_text(&sub(4), false, &[]).unwrap().serial, 4);
    // A new purchase (another licence id) may be installed by the operator.
    let other = issue(&licence("lic-2", "acme", 1, Kind::Subscription));
    assert_eq!(
        host.install_text(&other, false, &[]).unwrap().licence_id,
        "lic-2"
    );
    // A licence that fails verification or names another tenant never lands.
    assert!(host.install_text(&tampered(&sub(9)), false, &[]).is_err());
    let foreign = issue(&licence("lic-3", "other", 1, Kind::Subscription));
    assert_eq!(
        host.install_text(&foreign, false, &[]).unwrap_err().code,
        code::TENANT_MISMATCH
    );
    assert_eq!(host.licence().unwrap().licence_id, "lic-2");
}

#[test]
fn killed_between_the_temp_file_and_the_rename_the_old_licence_is_still_whole() {
    let site = Site::new("atomic");
    site.put(&sub(1));
    // A write that stops after step one: the new content sits under the temporary name.
    let tmp = files::write_temp(&site.licence_file(), sub(2).as_bytes()).unwrap();
    assert!(tmp.exists());
    let host = site.host(false, &clock_at(ISSUED + 100)).unwrap();
    assert_eq!(
        host.licence().unwrap().serial,
        1,
        "the old file is the licence"
    );
    // The next write replaces the leftover temporary file and completes.
    host.install_text(&sub(2), false, &[]).unwrap();
    assert_eq!(host.licence().unwrap().serial, 2);
    assert!(!tmp.exists());
    // The temporary file is never mistaken for a licence.
    let reload = site.host(false, &clock_at(ISSUED + 100)).unwrap();
    assert_eq!(reload.licence().unwrap().serial, 2);
}

#[test]
fn trusted_keys_parse_and_refuse_garbage() {
    assert!(parse_trusted_keys(&keys_text()).is_ok());
    for bad in [
        "",
        "# nothing\n",
        "id zz 0 -\n",
        "id 00 0 -\n",
        "id abcd\n",
        &format!(
            "{KEY_ID} {} x -\n",
            hex(&signing_key().verifying_key().to_bytes())
        ),
    ] {
        let error = parse_trusted_keys(bad).err().unwrap();
        assert!(
            !error.contains(&hex(&signing_key().verifying_key().to_bytes())),
            "{error}"
        );
    }
}

// ---- renewal --------------------------------------------------------------------------------

struct Plane {
    answer: Mutex<Result<Fetched, FetchError>>,
    calls: AtomicU64,
}

impl Plane {
    fn new(answer: Result<Fetched, FetchError>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            calls: AtomicU64::new(0),
        })
    }
    fn set(&self, answer: Result<Fetched, FetchError>) {
        *self.answer.lock().unwrap() = answer;
    }
}

#[async_trait]
impl LicenceFetcher for Plane {
    async fn fetch(&self) -> Result<Fetched, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer.lock().unwrap().clone()
    }
}

fn fetched(text: String, revoked: &[&str]) -> Result<Fetched, FetchError> {
    Ok(Fetched {
        licence_text: text,
        revoked: revoked.iter().map(|s| (*s).to_owned()).collect(),
    })
}

#[tokio::test]
async fn a_due_licence_is_renewed_replacing_the_file_and_ending_the_grace() {
    let site = Site::new("renew");
    site.put(&sub(1));
    let host = site.host(false, &clock_at(EXPIRES + 86_400)).unwrap();
    assert!(wants_renewal(LicenceGate::standing(&host)));
    let plane = Plane::new(fetched(
        issue(
            &Licence::new(RawLicence {
                v: PAYLOAD_VERSION,
                licence_id: "lic-1".to_owned(),
                tenant: Some("acme".to_owned()),
                installation_id: None,
                edition: Edition::new("business").unwrap(),
                ent: Entitlements::from_lists(Some(Limit::UpTo(25)), vec![], vec![]),
                kind: Kind::Subscription,
                issued_at: EXPIRES,
                not_before: EXPIRES,
                expires_at: Some(EXPIRES + 30 * 86_400),
                renew_after: None,
                grace: GRACE,
                serial: 2,
                key_id: KEY_ID.to_owned(),
            })
            .unwrap(),
        ),
        &[],
    ));
    let report = renew_once(&host, plane.as_ref(), false).await;
    assert_eq!(report.code, "ok");
    assert_eq!(report.installed.unwrap().serial, 2);
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );
    let reloaded = site.host(false, &clock_at(EXPIRES + 86_400)).unwrap();
    assert_eq!(
        reloaded.licence().unwrap().serial,
        2,
        "the file was replaced"
    );
    let attempt = reloaded.last_attempt().unwrap();
    assert_eq!(attempt.code, "ok");
}

#[tokio::test]
async fn a_stale_revoked_or_foreign_licence_is_refused_and_changes_nothing() {
    let site = Site::new("refuse");
    site.put(&sub(5));
    let host = site.host(false, &clock_at(EXPIRES + 86_400)).unwrap();
    let before = std::fs::read_to_string(site.licence_file()).unwrap();

    let plane = Plane::new(fetched(sub(4), &[]));
    assert_eq!(
        renew_once(&host, plane.as_ref(), false).await.code,
        "licence.stale"
    );
    plane.set(fetched(sub(5), &[]));
    assert_eq!(
        renew_once(&host, plane.as_ref(), false).await.code,
        "licence.stale"
    );
    plane.set(fetched(sub(6), &["lic-1"]));
    assert_eq!(
        renew_once(&host, plane.as_ref(), false).await.code,
        code::REVOKED
    );
    plane.set(fetched(
        issue(&licence("lic-1", "other", 9, Kind::Subscription)),
        &[],
    ));
    assert_eq!(
        renew_once(&host, plane.as_ref(), false).await.code,
        code::TENANT_MISMATCH
    );
    plane.set(fetched(tampered(&sub(9)), &[]));
    assert!(
        renew_once(&host, plane.as_ref(), false)
            .await
            .code
            .starts_with("licence.")
    );
    // A renewal is for the same licence id: another id is refused here (an operator may install it).
    plane.set(fetched(
        issue(&licence("lic-other", "acme", 9, Kind::Subscription)),
        &[],
    ));
    assert_eq!(
        renew_once(&host, plane.as_ref(), false).await.code,
        "licence.licence_id_mismatch"
    );

    assert_eq!(
        std::fs::read_to_string(site.licence_file()).unwrap(),
        before
    );
    assert_eq!(host.licence().unwrap().serial, 5);
}

#[tokio::test]
async fn an_unreachable_control_plane_is_a_recorded_attempt_and_the_file_is_untouched() {
    let site = Site::new("down");
    site.put(&sub(1));
    let host = site.host(false, &clock_at(EXPIRES + 86_400)).unwrap();
    let before = std::fs::read_to_string(site.licence_file()).unwrap();
    for (error, expected) in [
        (FetchError::Unreachable, code::FETCH_UNREACHABLE),
        (FetchError::NotAvailable, code::FETCH_NONE),
        (FetchError::Malformed, code::FETCH_MALFORMED),
    ] {
        let plane = Plane::new(Err(error));
        let report = renew_once(&host, plane.as_ref(), false).await;
        assert_eq!(report.code, expected);
        assert!(report.installed.is_none());
        assert_eq!(host.last_attempt().unwrap().code, expected);
    }
    assert_eq!(
        std::fs::read_to_string(site.licence_file()).unwrap(),
        before
    );
    assert!(
        matches!(
            LicenceGate::standing(&host),
            Standing::Licensed(Status::Grace { .. })
        ),
        "offline inside the grace period changes nothing"
    );
}

#[tokio::test]
async fn an_active_licence_is_not_renewed_unless_forced() {
    let site = Site::new("notdue");
    site.put(&sub(1));
    let host = site.host(false, &clock_at(ISSUED + 100)).unwrap();
    let plane = Plane::new(fetched(sub(2), &[]));
    let report = renew_once(&host, plane.as_ref(), false).await;
    assert_eq!(report.code, "not_due");
    assert_eq!(plane.calls.load(Ordering::SeqCst), 0);
    let forced = renew_once(&host, plane.as_ref(), true).await;
    assert_eq!(forced.code, "ok");
}

#[tokio::test]
async fn the_service_tick_renews_once_per_interval() {
    let site = Site::new("tick");
    site.put(&sub(1));
    let clock = clock_at(EXPIRES + 86_400);
    let host = Arc::new(site.host(false, &clock).unwrap());
    let plane = Plane::new(Err(FetchError::Unreachable));
    let settings = Settings {
        renew_check_hours: 6,
        ..Settings::default()
    };
    let service = LicenceService::new(
        Arc::clone(&host),
        settings,
        Some(plane.clone() as Arc<dyn LicenceFetcher>),
        clock.clone(),
    );
    assert!(service.tick().await.is_some());
    assert!(
        service.tick().await.is_none(),
        "inside the interval: no second attempt"
    );
    clock.advance(Duration::from_secs(6 * 3_600 + 1));
    assert!(service.tick().await.is_some());
    assert_eq!(plane.calls.load(Ordering::SeqCst), 2);
    // The tick also persisted the clock-guard observation.
    assert!(site.state_file().exists());
}

#[test]
fn the_control_plane_answer_is_parsed_strictly() {
    let ok = br#"{"data":{"licence":"SPOT-LICENCE-1.a.b","proof":"x","valid_until":"2027-01-01T00:00:00Z"}}"#;
    assert_eq!(
        parse_response(ok).unwrap().licence_text,
        "SPOT-LICENCE-1.a.b"
    );
    let revoked = br#"{"data":{"licence":"t","revoked":["lic-1"]}}"#;
    assert_eq!(parse_response(revoked).unwrap().revoked, ["lic-1"]);
    for bad in [
        &b"{}"[..],
        b"not json",
        br#"{"data":{"licence":""}}"#,
        br#"{"data":{}}"#,
    ] {
        assert_eq!(parse_response(bad), Err(FetchError::Malformed));
    }
    assert!(interpret(200, ok).is_ok());
    assert_eq!(interpret(404, b"").unwrap_err(), FetchError::NotAvailable);
    for status in [429, 500, 503, 401] {
        assert_eq!(interpret(status, b"").unwrap_err(), FetchError::Unreachable);
    }
}

// ---- the heartbeat signal ----------------------------------------------------------------------

#[test]
fn the_licence_state_signal_maps_every_standing() {
    assert_eq!(state_code_of(Standing::Unlicensed), None);
    assert_eq!(state_code_of(Standing::Invalid), Some(2));
    assert_eq!(state_code_of(Standing::Licensed(Status::Active)), Some(0));
    assert_eq!(
        state_code_of(Standing::Licensed(Status::RenewalDue)),
        Some(0)
    );
    assert_eq!(
        state_code_of(Standing::Licensed(Status::Grace { until: 1 })),
        Some(1)
    );
    assert_eq!(
        state_code_of(Standing::Licensed(Status::ClockSuspect { until: 1 })),
        Some(1)
    );
    assert_eq!(
        state_code_of(Standing::Licensed(Status::Restricted)),
        Some(2)
    );
    assert_eq!(state_code_of(Standing::Licensed(Status::Expired)), Some(2));
    assert_eq!(
        state_code_of(Standing::Licensed(Status::NotYetValid)),
        Some(2)
    );
}

// ---- registration ------------------------------------------------------------------------------

struct Egress(bool);
impl LicenceEgress for Egress {
    fn allows(&self, _host: &str, _port: u16) -> bool {
        self.0
    }
}

fn table(site: &Site, extra: &[(&str, toml::Value)]) -> BTreeMap<String, toml::Value> {
    let mut t = toml::map::Map::new();
    t.insert(
        "licence_file".into(),
        site.licence_file().display().to_string().into(),
    );
    t.insert(
        "state_file".into(),
        site.state_file().display().to_string().into(),
    );
    t.insert(
        "trusted_keys_file".into(),
        site.dir
            .path()
            .join("keys.txt")
            .display()
            .to_string()
            .into(),
    );
    for (k, v) in extra {
        t.insert((*k).to_owned(), v.clone());
    }
    let mut all = BTreeMap::new();
    all.insert("licence".to_owned(), toml::Value::Table(t));
    all
}

fn ctx(settings: BTreeMap<String, toml::Value>, clock: Arc<ManualClock>) -> SiteCtx<'static> {
    SiteCtx::new("acme", Duration::from_secs(1), clock, settings, true)
}

#[test]
fn registration_provides_the_gate_and_the_signal_and_sets_the_level_from_required() {
    for (required, level) in [(false, Level::Optional), (true, Level::Required)] {
        let site = Site::new("register");
        site.put(&sub(1));
        let mut c = ctx(
            table(&site, &[("required", toml::Value::Boolean(required))]),
            clock_at(ISSUED + 100),
        );
        register(&mut c).unwrap();
        let gate = c.port::<dyn LicenceGate>().unwrap();
        assert!(gate.decide(Action::CreateDocument).is_allow());
        assert!(c.port::<dyn LicenceStateSource>().unwrap().state_code() == Some(0));
        let assembly = c.finish(Router::new()).assembly.unwrap();
        assert_eq!(assembly.services().len(), 1);
        assert_eq!(assembly.services()[0].name(), "licence");
        assert_eq!(assembly.services()[0].level(), level);
    }
}

#[tokio::test]
async fn a_tampered_licence_degrades_readiness_when_optional_and_refuses_start_when_required() {
    let site = Site::new("regtamper");
    site.put(&tampered(&sub(1)));
    let mut optional = ctx(table(&site, &[]), clock_at(ISSUED + 100));
    register(&mut optional).unwrap();
    let assembly = optional.finish(Router::new()).assembly.unwrap();
    let check = assembly.services()[0].check().await;
    assert!(check.unwrap_err().contains("licence."));

    let mut required = ctx(
        table(&site, &[("required", toml::Value::Boolean(true))]),
        clock_at(ISSUED + 100),
    );
    assert!(register(&mut required).is_err());
}

fn online(site: &Site, key_mode: u32) -> BTreeMap<String, toml::Value> {
    for name in ["host.crt", "ca.crt", "host.key"] {
        write_plain(&site.dir.path().join(name), "pem");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            site.dir.path().join("host.key"),
            std::fs::Permissions::from_mode(key_mode),
        )
        .unwrap();
    }
    let _ = key_mode;
    let p = |n: &str| toml::Value::String(site.dir.path().join(n).display().to_string());
    table(
        site,
        &[
            ("control_plane", "https://cp.example.net:8443".into()),
            ("host_cert", p("host.crt")),
            ("host_key", p("host.key")),
            ("ca_file", p("ca.crt")),
        ],
    )
}

fn ports(allow: bool) -> Arc<LicencePorts> {
    Arc::new(LicencePorts {
        fetcher: Plane::new(Err(FetchError::Unreachable)) as Arc<dyn LicenceFetcher>,
        egress: Arc::new(Egress(allow)),
    })
}

#[test]
fn the_control_plane_missing_from_the_egress_allow_list_refuses_start_naming_the_entry() {
    let site = Site::new("egress");
    site.put(&sub(1));
    let mut c = ctx(online(&site, 0o600), clock_at(ISSUED + 100));
    c.provide(ports(false)).unwrap();
    let error = register(&mut c).unwrap_err().to_string();
    assert!(error.contains("cp.example.net:8443"), "{error}");
    assert!(error.contains(code::EGRESS_NOT_ALLOWED), "{error}");
}

#[test]
fn a_control_plane_without_ports_or_with_an_unsafe_key_refuses_start() {
    let site = Site::new("noports");
    site.put(&sub(1));
    let mut c = ctx(online(&site, 0o600), clock_at(ISSUED + 100));
    let error = register(&mut c).unwrap_err().to_string();
    assert!(error.contains(code::NO_PORTS), "{error}");
    #[cfg(unix)]
    {
        let mut c = ctx(online(&site, 0o644), clock_at(ISSUED + 100));
        c.provide(ports(true)).unwrap();
        let error = register(&mut c).unwrap_err().to_string();
        assert!(error.contains("host.key"), "{error}");
    }
    let mut ok = ctx(online(&site, 0o600), clock_at(ISSUED + 100));
    ok.provide(ports(true)).unwrap();
    register(&mut ok).unwrap();
}

#[test]
fn settings_are_validated() {
    let bad = |s: Settings| s.validate().unwrap_err().to_string();
    assert!(
        bad(Settings {
            required: true,
            ..Settings::default()
        })
        .contains("trusted_keys_file")
    );
    assert!(
        bad(Settings {
            control_plane: "http://cp".into(),
            ..Settings::default()
        })
        .contains("control_plane")
    );
    assert!(
        bad(Settings {
            control_plane: "https://cp/x".into(),
            ..Settings::default()
        })
        .contains("control_plane")
    );
    assert!(
        bad(Settings {
            control_plane: "https://cp".into(),
            ..Settings::default()
        })
        .contains("host_cert")
    );
    assert!(
        bad(Settings {
            renew_check_hours: 10_000,
            ..Settings::default()
        })
        .contains("renew_check_hours")
    );
    assert!(Settings::default().validate().is_ok());
    assert_eq!(Settings::default().renew_hours(), 6);
    assert_eq!(
        Settings {
            control_plane: "https://cp.example.net".into(),
            ..Settings::default()
        }
        .control_plane_endpoint()
        .unwrap(),
        ("cp.example.net".to_owned(), 443)
    );
    let resolved = Settings::default()
        .resolve(Some(Path::new("/root")), "acme")
        .unwrap();
    assert_eq!(
        resolved.licence_file,
        Path::new("/root/licence/acme/licence.txt")
    );
    assert_eq!(
        resolved.state_file,
        Path::new("/root/licence/acme/clock.state")
    );
    assert!(Settings::default().resolve(None, "acme").is_err());
}

// ---- `spot licence` (process tests; PostgreSQL) -------------------------------------------------

fn configure(env: &support::Env) -> (PathBuf, PathBuf) {
    let dir = env.root.join("lic-files");
    std::fs::create_dir_all(&dir).unwrap();
    let keys = dir.join("keys.txt");
    write_plain(&keys, keys_text());
    let common = env.root.join("common.toml");
    let mut text = std::fs::read_to_string(&common).unwrap();
    text.push_str(&format!(
        "[services.licence]\ntrusted_keys_file = {:?}\ntenant = \"acme\"\n",
        keys.display().to_string()
    ));
    std::fs::write(&common, text).unwrap();
    (
        dir,
        env.root.join("licence").join(&env.site).join("licence.txt"),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spot_licence_install_status_and_exit_codes() {
    let Some(env) = support::Env::new().await else {
        return;
    };
    env.bare_site();
    let (dir, installed) = configure(&env);
    let run = |args: &[&str]| {
        let mut all = vec!["licence"];
        all.extend_from_slice(args);
        let output = env.spot(&all);
        (output.status.code().unwrap(), support::text(&output))
    };
    let site = env.site.as_str();

    let (code, out) = run(&["status", "--site", site]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("standing: unlicensed"), "{out}");

    // Usage: a missing --site.
    let (code, _) = run(&["status"]);
    assert_eq!(code, 1);

    let file = dir.join("lic.txt");
    std::fs::write(&file, sub(3)).unwrap();
    let (code, out) = run(&["install", file.to_str().unwrap(), "--site", site]);
    assert_eq!(code, 0, "{out}");
    assert!(installed.exists());

    // No downgrade: refused, exit 2, nothing changed.
    std::fs::write(&file, sub(2)).unwrap();
    let (code, out) = run(&["install", file.to_str().unwrap(), "--site", site]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("licence.stale"), "{out}");
    assert!(
        !out.contains("SPOT-LICENCE-1"),
        "no licence text in output: {out}"
    );

    // The real clock is far past the test licence's year-2026 expiry: restricted, exit 2.
    let (code, out) = run(&["status", "--site", site]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("restricted") || out.contains("expired"),
        "{out}"
    );
    let (code, out) = run(&["status", "--site", site, "--json"]);
    assert_eq!(code, 2, "{out}");
    let value: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(value["licence_id"], "lic-1");
    assert_eq!(value["serial"], 3);

    // Renew without a control plane is an operational failure, exit 3.
    let (code, _) = run(&["renew", "--site", site]);
    assert_eq!(code, 3);
}
