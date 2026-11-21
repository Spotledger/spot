//! The production [`UpdateAction`](super::schedule::UpdateAction): the self-updater of
//! [`runner`](super::runner) joined to the real filesystem, the pinned or system-rooted
//! channel, the trust anchor the package installed and the service manager.
//!
//! Files, all outside the version directories (so no update replaces them):
//!
//! - `<install root>/update-trust.json`: the update trust anchor the package installs, read
//!   on every check (a missing or malformed file is a failed check, never a skipped
//!   signature check);
//! - `<state dir>/update-revocations.json`: the newest verified revocation list;
//! - `<install root>/versions/<version>/spot-agent[.exe]`: what the probe runs, as
//!   `spot-agent --state-dir <dir> probe`.
//!
//! Nothing here decides whether an update is acceptable: every check is `spot-updater`'s,
//! run by [`SelfUpdater`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use semver::Version;
use spot_agent_enroll::keys::KeyStore;
use spot_updater::{Layout, StdFileSystem};

use super::error::UpdateError;
use super::runner::{SelfUpdater, UpdateConfig, UpdateReport, UpdateStatus};
use super::schedule::UpdateAction;
use super::service::{AgentService, ProcessProbe, QuiesceFlag, Restarter, SystemRestarter};
use super::source::ChannelSource;
use super::trust::{FileRevocationStore, load_anchor};
use super::{CODE_FAILED, CODE_NEEDS_ATTENTION, CODE_ROLLED_BACK};
use crate::channel::client::Client;
use crate::channel::transport::Transport;

/// The anchor file name inside the install root.
pub const ANCHOR_FILE: &str = "update-trust.json";
/// The revocation list file name inside the state directory.
pub const REVOCATIONS_FILE: &str = "update-revocations.json";

/// The running version, as the build says it.
///
/// # Errors
/// [`UpdateError::Layout`] when the package version is not semver (a build error in
/// practice).
pub fn running_version() -> Result<Version, UpdateError> {
    Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| UpdateError::Layout("the running version is not a semantic version".into()))
}

/// What [`HostUpdate::new`] is built from.
pub struct HostParts {
    /// The install root.
    pub install_root: PathBuf,
    /// The state directory (revocation list, probe argument).
    pub state_root: PathBuf,
    /// The channel client (gateway calls, signed).
    pub client: Arc<Client>,
    /// The transport the payload is fetched over.
    pub transport: Arc<dyn Transport>,
    /// The identity key signing every request.
    pub keys: Arc<dyn KeyStore>,
    /// This agent's id.
    pub agent_id: String,
    /// The version of the running process.
    pub running: Version,
}

/// The self-update of this host.
pub struct HostUpdate {
    config: UpdateConfig,
    anchor_path: PathBuf,
    revocations: FileRevocationStore,
    source: ChannelSource,
    service: AgentService,
    fs: StdFileSystem,
    restarter: SystemRestarter,
}

impl HostUpdate {
    /// Assembles the update seams for this host.
    #[must_use]
    pub fn new(parts: HostParts) -> Self {
        let HostParts {
            install_root,
            state_root,
            client,
            transport,
            keys,
            agent_id,
            running,
        } = parts;
        let exe = if cfg!(windows) {
            "spot-agent.exe"
        } else {
            "spot-agent"
        };
        let probe = ProcessProbe::new(
            Layout::new(install_root.clone()),
            exe,
            vec![
                "--state-dir".to_owned(),
                state_root.to_string_lossy().into_owned(),
                "probe".to_owned(),
            ],
        );
        let service = AgentService::new(
            running.clone(),
            Arc::new(QuiesceFlag::new()),
            Arc::new(probe),
            Arc::new(spot_core::SystemClock::new()),
        );
        Self {
            anchor_path: install_root.join(ANCHOR_FILE),
            revocations: FileRevocationStore::new(state_root.join(REVOCATIONS_FILE)),
            source: ChannelSource::new(client, transport, keys, agent_id),
            config: UpdateConfig::new(running, install_root),
            service,
            fs: StdFileSystem::new(),
            restarter: SystemRestarter::platform_default(),
        }
    }

    /// The anchor file this host reads.
    #[must_use]
    pub fn anchor_path(&self) -> &Path {
        &self.anchor_path
    }

    fn failed(error: &UpdateError) -> UpdateReport {
        UpdateReport {
            status: UpdateStatus::Failed(error.detail()),
            code: Some(CODE_FAILED),
            detail: error.detail(),
            needs_restart: false,
        }
    }
}

impl UpdateAction for HostUpdate {
    fn boot(&self, now: i64) -> Option<UpdateReport> {
        // Without an anchor there is nothing the engine could verify: leave an interrupted
        // update to the next start with a working install.
        let trust = load_anchor(&self.anchor_path).ok()?;
        let updater = SelfUpdater::new(
            &self.config,
            &self.fs,
            &self.service,
            &self.source,
            &self.restarter,
            &trust,
            &self.revocations,
        );
        let report = updater.boot(now);
        matches!(
            report.status,
            UpdateStatus::RolledBack { .. }
                | UpdateStatus::Failed(_)
                | UpdateStatus::NeedsAttention(_)
                | UpdateStatus::Applied(_)
                | UpdateStatus::Kept(_)
        )
        .then_some(report)
    }

    fn check(&self, now: i64) -> UpdateReport {
        let trust = match load_anchor(&self.anchor_path) {
            Ok(trust) => trust,
            Err(error) => return Self::failed(&error),
        };
        let updater = SelfUpdater::new(
            &self.config,
            &self.fs,
            &self.service,
            &self.source,
            &self.restarter,
            &trust,
            &self.revocations,
        );
        // The probation window of a committed update closes here; a rollback or an
        // attention result is the report of this round (the next round looks again).
        let tick = updater.tick(now);
        if tick.code == Some(CODE_ROLLED_BACK) || tick.code == Some(CODE_NEEDS_ATTENTION) {
            return tick;
        }
        updater.check(now)
    }

    fn restart(&self) -> Result<(), UpdateError> {
        self.restarter.restart()
    }
}
