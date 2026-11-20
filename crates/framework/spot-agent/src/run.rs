//! `spot-agent run`: check state, then hand over to the channel loop (idle in T2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_agent_core::{HandlerRegistry, ResultLog as _, TrustStore};
use spot_agent_enroll::identity::AgentIdentity;
use spot_agent_enroll::keys::KeyStore;
use tokio::sync::Notify;

use crate::audit::{LocalAuditLog, event_record};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::keystore::{KeyStoreKind, open_key_store};
use crate::logging::Logger;
use crate::state::StateDir;
use crate::trust::TrustFile;

/// A cooperative shutdown signal (SIGTERM, service stop).
#[derive(Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Shutdown {
    /// A fresh, unset signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests shutdown.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Whether shutdown was requested.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Completes when shutdown is requested.
    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

/// Everything the channel loop and the handlers need, loaded and checked.
pub struct Runtime {
    /// The state directory.
    pub state: StateDir,
    /// The validated configuration.
    pub config: AgentConfig,
    /// The enrolment record.
    pub identity: AgentIdentity,
    /// The identity key store.
    pub key_store: Arc<dyn KeyStore>,
    /// The trusted administrator keys.
    pub trust: TrustStore,
    /// The decision policy for this host, from `trust.toml` (set at enrolment).
    pub policy: spot_agent_core::Policy,
    /// The local audit log.
    pub audit: Arc<LocalAuditLog>,
    /// The registered handlers.
    pub handlers: Arc<HandlerRegistry>,
    /// The logger.
    pub logger: Logger,
    /// The shutdown signal.
    pub shutdown: Shutdown,
}

/// Loads and checks everything `run` needs.
///
/// # Errors
/// [`AgentError::NotEnrolled`], [`AgentError::StatePermissions`] (a 0644 key file refuses
/// start), [`AgentError::ConfigInvalid`] (unknown config key), the key store errors.
pub fn load_runtime(
    state: &StateDir,
    logger: Logger,
    shutdown: Shutdown,
) -> Result<Runtime, AgentError> {
    if !state.root().exists() || !state.is_enrolled() {
        return Err(AgentError::NotEnrolled);
    }
    state.check_permissions()?;
    let text = std::fs::read_to_string(state.config_path()).map_err(|_| AgentError::NotEnrolled)?;
    let config = AgentConfig::parse(&text)?;
    let key_store: Arc<dyn KeyStore> =
        Arc::from(open_key_store(KeyStoreKind::File, &state.key_path())?);
    let identity = AgentIdentity::load(state.key_path()).map_err(|e| match e {
        spot_agent_enroll::error::EnrolError::KeyFilePermissions => {
            AgentError::StatePermissions("the identity file must be mode 0600".to_owned())
        }
        _ => AgentError::ConfigInvalid("the enrolment record is malformed".to_owned()),
    })?;
    let trust_file = TrustFile::load(&state.trust_path())?;
    let trust = trust_file.to_store()?;
    let policy = trust_file.to_policy();
    let audit = Arc::new(LocalAuditLog::open(
        state.audit_path(),
        identity.agent_id.clone(),
    )?);
    let handlers = Arc::new(crate::handlers::build_registry(&config));
    Ok(Runtime {
        state: state.clone(),
        config,
        identity,
        key_store,
        trust,
        policy,
        audit,
        handlers,
        logger,
        shutdown,
    })
}

/// Runs the agent until shutdown.
///
/// # Errors
/// Whatever [`load_runtime`] or the channel loop returns.
pub async fn run(
    state: &StateDir,
    logger: Logger,
    shutdown: Shutdown,
    now: i64,
) -> Result<(), AgentError> {
    let rt = load_runtime(state, logger, shutdown)?;
    let agent = rt.identity.agent_id.clone();
    let audit = rt.audit.clone();
    let log = rt.logger.clone();
    log.info("start", &format!("agent {agent} starting"));
    let _ = audit.append(&event_record(&agent, "agent.run", "started", None, now));
    let result = crate::channel::serve(rt).await;
    let _ = audit.append(&event_record(&agent, "agent.run", "stopped", None, now));
    log.info("stop", "agent stopped");
    result
}

/// Spawns the SIGTERM / Ctrl-C listener that triggers `shutdown` (Unix); on Windows the
/// service host triggers it.
pub fn watch_signals(shutdown: &Shutdown) {
    #[cfg(unix)]
    {
        let sd = shutdown.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut term) = signal(SignalKind::terminate()) {
                tokio::select! {
                    _ = term.recv() => {},
                    _ = tokio::signal::ctrl_c() => {},
                }
                sd.trigger();
            }
        });
    }
    #[cfg(not(unix))]
    {
        let sd = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            sd.trigger();
        });
    }
}
