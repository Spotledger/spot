//! Unattended-support basics (WP-PRD-13.T3): one consent file, one crash spool, and the
//! process-wide crash-reporting service `spot-unattended-support` and `spot-redact` are
//! wired into.
//!
//! This is a plain helper module (it declares no `ORDER`/`register`, so `build.rs` does not
//! add it to the per-site services registry): crash reporting is a process-wide concern, not
//! a per-site one, and the brief asks for it "optional, off by default" rather than wired
//! into `spot serve`. `main.rs` is outside this task's owned paths, so the panic hook this
//! module can install is not called from the long-running host by this change; see the task
//! report's Open questions. What is delivered and tested here:
//!
//! - [`read_consent`] / [`write_consent`]: the single consent file (mode 0600) `spot support
//!   consent` reads and changes.
//! - [`spool`]: the local crash spool under the host root.
//! - [`UnattendedSupport`]: a process-wide service bundling a [`ConsentStore`], a [`Spool`]
//!   and a [`CrashCollector`], with [`UnattendedSupport::arm`] to install the panic hook and
//!   [`UnattendedSupport::upload_pending`] to send what consent allows to an injected
//!   [`Uploader`] (the host's egress-controlled HTTP client; `None` means no endpoint is
//!   configured, so nothing is ever sent).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_unattended_support::consent::{Consent, ConsentKind, ConsentState, ConsentStore};
use spot_unattended_support::crash::{CrashCollector, CrashContext, install_panic_hook};
use spot_unattended_support::scrub::Scrubber;
use spot_unattended_support::spool::Spool;
use spot_unattended_support::telemetry::Uploader;
use spot_unattended_support::SupportError;

/// The single file under the host root holding the two consent switches, mode 0600.
pub const CONSENT_FILE: &str = "support-consent.toml";
/// The folder under the host root holding spooled crash reports.
pub const SPOOL_DIR: &str = "support-crash-spool";

/// The on-disk shape of [`CONSENT_FILE`]: exactly the two [`Consent::as_str`] spellings, or
/// empty for "not yet written" (first run).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConsentFile {
    crash: String,
    telemetry: String,
}

/// Reads the two switches from [`CONSENT_FILE`] under `root`. A missing file is first run
/// (both switches `ask`, nothing allowed); the file never holds anything but the two
/// settings spellings, so it carries no secret.
///
/// # Errors
/// A read error other than "not found", invalid TOML, or a consent value that is not
/// `off`, `ask` or `on`.
pub fn read_consent(root: &Path) -> crate::Result<ConsentState> {
    let path = root.join(CONSENT_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConsentState::first_run());
        }
        Err(e) => return Err(crate::Error(format!("{}: {e}", path.display()))),
    };
    let file: ConsentFile = toml::from_str(&text)
        .map_err(|e| crate::Error(format!("{}: invalid TOML: {e}", path.display())))?;
    fn present(v: &str) -> Option<&str> {
        (!v.is_empty()).then_some(v)
    }
    ConsentState::from_settings(present(&file.crash), present(&file.telemetry))
        .map_err(|e| crate::Error(e.to_string()))
}

/// Writes the two switches to [`CONSENT_FILE`] under `root`, mode 0600 on Unix. The history
/// is not persisted: it exists only for the lifetime of the process that changed it (the
/// file is read fresh on every invocation), so a `spot support consent status` after a
/// restart shows the current switches without a change log.
///
/// # Errors
/// A write failure.
pub fn write_consent(root: &Path, state: &ConsentState) -> crate::Result<()> {
    let file = ConsentFile {
        crash: state.get(ConsentKind::Crash).as_str().to_owned(),
        telemetry: state.get(ConsentKind::Telemetry).as_str().to_owned(),
    };
    let text = toml::to_string(&file)
        .map_err(|e| crate::Error(format!("cannot encode the consent file: {e}")))?;
    write_private(&root.join(CONSENT_FILE), text.as_bytes())
        .map_err(|e| crate::Error(format!("{}: {e}", root.join(CONSENT_FILE).display())))
}

/// Writes `bytes` to `path`, private to the user (mode 0600) on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// The crash spool under `root`.
#[must_use]
pub fn spool(root: &Path) -> Spool {
    Spool::new(root.join(SPOOL_DIR))
}

/// What this host tells the collector about itself. `version` and `platform` are always
/// known; the rest default to empty, which the collector accepts.
#[must_use]
pub fn host_context(version: &str) -> CrashContext {
    CrashContext {
        version: version.to_owned(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        build_id: String::new(),
        config_summary: std::collections::BTreeMap::new(),
        deployment_model_digest: String::new(),
    }
}

/// The process-wide unattended-support service: one consent store, one spool, one collector,
/// and (if the host configured one) an uploader.
///
/// Nothing in here is installed by default. A host that wants crash reporting live builds
/// one of these at start-up (after reading [`read_consent`] into a [`ConsentStore`]) and
/// calls [`UnattendedSupport::arm`] once.
pub struct UnattendedSupport {
    consent: Arc<ConsentStore>,
    spool: Arc<Spool>,
    collector: Arc<CrashCollector>,
    /// `None` means no endpoint is configured: [`UnattendedSupport::upload_pending`] then
    /// always returns `Ok(0)`, whatever consent says.
    uploader: Option<Arc<dyn Uploader + Send + Sync>>,
}

impl UnattendedSupport {
    /// A service over `consent` and a spool rooted at `spool_dir`, collecting with `scrubber`
    /// for `context`. `uploader` is the host's egress-controlled HTTP client, or `None` when
    /// no endpoint is configured.
    pub fn new(
        consent: Arc<ConsentStore>,
        spool_dir: impl Into<PathBuf>,
        context: CrashContext,
        scrubber: Scrubber,
        uploader: Option<Arc<dyn Uploader + Send + Sync>>,
    ) -> Self {
        Self {
            consent,
            spool: Arc::new(Spool::new(spool_dir)),
            collector: Arc::new(CrashCollector::new(context, scrubber)),
            uploader,
        }
    }

    /// The consent store (so a host can also route `spot support consent` changes through
    /// the same copy a running process is reading).
    #[must_use]
    pub fn consent(&self) -> &Arc<ConsentStore> {
        &self.consent
    }

    /// The spool (so a host can list, view or clear it).
    #[must_use]
    pub fn spool(&self) -> &Arc<Spool> {
        &self.spool
    }

    /// Whether an upload endpoint is configured at all.
    #[must_use]
    pub fn has_endpoint(&self) -> bool {
        self.uploader.is_some()
    }

    /// Installs the panic hook: a panic records a redacted report (when the consent lock's
    /// last-known value allows it) and then runs the previous hook. `now` supplies the time
    /// (this crate and `spot-unattended-support` never read the wall clock themselves).
    ///
    /// Idempotent in the sense that `std::panic::set_hook` always replaces the previous hook;
    /// call this once per process.
    pub fn arm(&self, now: fn() -> u64) {
        install_panic_hook(
            self.collector.clone(),
            self.consent.clone(),
            self.spool.clone(),
            now,
        );
    }

    /// Uploads whatever consent allows right now: everything spooled when crash reporting is
    /// `on`, only user-approved reports when it is `ask`, nothing when it is `off` or no
    /// endpoint is configured. Returns how many reports were sent.
    ///
    /// # Errors
    /// `spool_failed`, or `upload_failed` at the first report the uploader refuses (the rest
    /// stay spooled, same as [`spot_unattended_support::crash::upload_spooled`]).
    pub fn upload_pending(&self) -> Result<usize, SupportError> {
        let Some(uploader) = &self.uploader else {
            return Ok(0);
        };
        let state = self.consent.state();
        spot_unattended_support::crash::upload_spooled(&state, &self.spool, uploader.as_ref())
    }
}

/// Applies `on`/`off` to both switches at once (WP-PRD-13.T3's CLI surface: `spot support
/// consent on|off|status` names no switch separately), at `now`, returning the updated state.
/// `on` means crash reporting, and the spooling it allows, may run without asking for every
/// report; telemetry follows the same switch so there is exactly one decision to make.
#[must_use]
pub fn set_both(mut state: ConsentState, allow: bool, now: u64) -> ConsentState {
    let to = if allow { Consent::On } else { Consent::Off };
    state.set(ConsentKind::Crash, to, now);
    state.set(ConsentKind::Telemetry, to, now);
    state
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spot-unattended-support-svc-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_missing_consent_file_is_first_run() {
        let dir = temp_dir("missing");
        let state = read_consent(&dir).expect("first run");
        assert_eq!(state.get(ConsentKind::Crash), Consent::Ask);
        assert_eq!(state.get(ConsentKind::Telemetry), Consent::Ask);
    }

    #[test]
    fn consent_round_trips_through_the_file_and_is_private() {
        let dir = temp_dir("roundtrip");
        let state = set_both(ConsentState::first_run(), true, 10);
        write_consent(&dir, &state).expect("write");
        let read_back = read_consent(&dir).expect("read");
        assert!(read_back.may_upload_crash());
        assert!(read_back.may_send_telemetry());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(CONSENT_FILE))
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let off = set_both(read_back, false, 11);
        write_consent(&dir, &off).expect("write off");
        let read_off = read_consent(&dir).expect("read off");
        assert!(!read_off.may_upload_crash());
        assert!(!read_off.may_send_telemetry());
    }

    /// The panic hook is process-wide (`std::panic::set_hook`); serialise the tests that
    /// install one so two hooks installed by concurrently-running tests cannot chain into
    /// each other's spool.
    static HOOK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Recording(std::sync::Mutex<Vec<(String, Vec<u8>)>>);

    impl Uploader for Recording {
        fn upload(&self, kind: &str, body: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((kind.to_owned(), body.to_vec()));
            Ok(())
        }
    }

    fn scrubber() -> Scrubber {
        Scrubber::new(&[], &[]).expect("scrubber")
    }

    #[test]
    fn consent_off_sends_nothing_even_with_an_endpoint_configured() {
        let _guard = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("off-upload");
        let consent = Arc::new(ConsentStore::new(set_both(ConsentState::first_run(), false, 1)));
        let uploader: Arc<dyn Uploader + Send + Sync> = Arc::new(Recording(Default::default()));
        let service = UnattendedSupport::new(
            consent.clone(),
            dir.join("spool"),
            host_context("1.0.0"),
            scrubber(),
            Some(uploader.clone()),
        );
        service.arm(|| 100);
        let raised = std::panic::catch_unwind(|| panic!("off, with an endpoint configured"));
        assert!(raised.is_err());
        assert_eq!(
            service.upload_pending().expect("upload"),
            0,
            "nothing is sent while consent is off"
        );
    }

    #[test]
    fn consent_on_spools_and_uploads_a_redacted_report() {
        let _guard = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("on-upload");
        let consent = Arc::new(ConsentStore::new(set_both(ConsentState::first_run(), true, 1)));
        let recording = Arc::new(Recording(Default::default()));
        let uploader: Arc<dyn Uploader + Send + Sync> = recording.clone();
        let service = UnattendedSupport::new(
            consent,
            dir.join("spool"),
            host_context("1.0.0"),
            scrubber(),
            Some(uploader),
        );
        service.arm(|| 200);
        let raised = std::panic::catch_unwind(|| {
            panic!("on, secret token sk_live_aaaaaaaaaaaaaaaaaaaaaaaa must not leave the box")
        });
        assert!(raised.is_err());
        assert_eq!(service.upload_pending().expect("upload"), 1);
        let sent = recording.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "crash");
        let body = String::from_utf8_lossy(&sent[0].1);
        assert!(!body.contains("sk_live_aaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn no_endpoint_configured_never_uploads_however_consent_is_set() {
        let _guard = HOOK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("no-endpoint");
        let consent = Arc::new(ConsentStore::new(set_both(ConsentState::first_run(), true, 1)));
        let service = UnattendedSupport::new(
            consent,
            dir.join("spool"),
            host_context("1.0.0"),
            scrubber(),
            None,
        );
        assert!(!service.has_endpoint());
        service.arm(|| 300);
        let raised = std::panic::catch_unwind(|| panic!("on, but no endpoint"));
        assert!(raised.is_err());
        // A report was still spooled locally (consent is on); only the upload is withheld.
        assert_eq!(service.spool.list().expect("list").len(), 1);
        assert_eq!(service.upload_pending().expect("upload"), 0);
    }
}
