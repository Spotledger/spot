//! The long-poll loop and `serve`: poll, hand each command to the [`Pipeline`], report
//! results, heartbeat, back off with jitter on failure, stop at once on `agent.revoked`,
//! and finish or time-box the running command on shutdown.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::backoff::Backoff;
use super::client::{ChannelError, Client};
use super::intake::{Pipeline, PipelineParts};
use super::store::{FileCommandStore, FileNonceStore, nonce_path};
use super::transport::{TlsTransport, Transport, parse_pem_certificates};
use crate::config::AgentConfig;
use crate::enrol::EnrolGateway;
use crate::error::AgentError;
use crate::logging::{Logger, build_redactor};
use crate::run::{Runtime, Shutdown};
use crate::state::{StateDir, atomic_write};

/// The persisted `status` data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStatus {
    /// Unix seconds of the last successful poll.
    pub last_poll_at: Option<i64>,
    /// The last command id handled.
    pub last_command: Option<String>,
    /// Accepted-but-unfinished commands plus unacknowledged results.
    pub backlog: usize,
    /// The learned gateway clock offset, seconds.
    pub clock_skew: i64,
}

fn status_path(state: &StateDir) -> std::path::PathBuf {
    state.root().join("channel-status.json")
}

/// Extra `status` lines from the persisted channel state.
#[must_use]
pub fn status_lines(state: &StateDir) -> Vec<String> {
    let Some(status) = std::fs::read_to_string(status_path(state))
        .ok()
        .and_then(|t| serde_json::from_str::<ChannelStatus>(&t).ok())
    else {
        return vec!["last poll: never".to_owned()];
    };
    vec![
        format!(
            "last poll: {}",
            status
                .last_poll_at
                .map_or_else(|| "never".to_owned(), super::timefmt::rfc3339)
        ),
        format!(
            "last command: {}",
            status.last_command.as_deref().unwrap_or("none")
        ),
        format!("backlog: {}", status.backlog),
        format!("clock skew: {}s", status.clock_skew),
    ]
}

/// What the loop needs besides the pipeline and client; time and jitter are injected.
pub struct LoopCtx {
    /// The client.
    pub client: Arc<Client>,
    /// The pipeline.
    pub pipeline: Arc<Pipeline>,
    /// The state directory (status file, key deletion on revocation).
    pub state: StateDir,
    /// The logger.
    pub logger: Logger,
    /// The shutdown signal.
    pub shutdown: Shutdown,
    /// Pause between polls.
    pub poll_pause: Duration,
    /// Unix seconds now.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Jitter in per mille (0..=1000).
    pub jitter: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Stop after this many polls (tests); `None` runs until shutdown.
    pub max_polls: Option<u64>,
}

/// How the loop ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopEnd {
    /// Shutdown was requested.
    Shutdown,
    /// The gateway revoked this agent; the local keys were deleted.
    Revoked,
    /// `max_polls` reached.
    PollLimit,
}

async fn sleep_or_shutdown(shutdown: &Shutdown, d: Duration) -> bool {
    tokio::select! {
        () = shutdown.wait() => true,
        () = tokio::time::sleep(d) => false,
    }
}

/// Deletes the local keys and enrolment record after revocation.
fn delete_local_keys(state: &StateDir) {
    let key = state.key_path();
    let mut ident = key.clone().into_os_string();
    ident.push(".identity");
    for path in [key, std::path::PathBuf::from(ident), state.trust_path()] {
        let _ = std::fs::remove_file(path);
    }
}

async fn report_all(ctx: &LoopCtx, now: i64) -> Result<(), ChannelError> {
    let pending = ctx
        .pipeline
        .unreported()
        .map_err(|_| ChannelError::Retryable("state".to_owned()))?;
    for report in pending {
        let client = ctx.client.clone();
        let sent = report.clone();
        let outcome = tokio::task::spawn_blocking(move || client.report(&sent, now))
            .await
            .map_err(|_| ChannelError::Retryable("join".to_owned()))?;
        outcome?;
        let _ = ctx.pipeline.mark_reported(&report.command_id);
    }
    Ok(())
}

fn write_status(ctx: &LoopCtx, status: &ChannelStatus) {
    if let Ok(text) = serde_json::to_string(status) {
        let _ = atomic_write(&status_path(&ctx.state), text.as_bytes());
    }
}

/// Runs the loop.
///
/// # Errors
/// [`AgentError::StateIo`] when a command record cannot be persisted (the loop stops
/// rather than execute without evidence).
pub async fn run_loop(ctx: LoopCtx) -> Result<LoopEnd, AgentError> {
    let mut backoff = Backoff::new();
    let mut status = ChannelStatus::default();
    let mut polls = 0u64;
    let now0 = (ctx.now)();
    if let Err(e) = ctx.pipeline.recover(now0).await {
        ctx.logger.error("recover_failed", &e.to_string());
        return Err(e);
    }
    loop {
        if ctx.shutdown.is_set() {
            return Ok(LoopEnd::Shutdown);
        }
        if ctx.max_polls.is_some_and(|m| polls >= m) {
            return Ok(LoopEnd::PollLimit);
        }
        let now = (ctx.now)();
        let last_seq = ctx.pipeline.store().last_reported_seq()?;
        let client = ctx.client.clone();
        let poll = tokio::task::spawn_blocking(move || client.poll(last_seq, now));
        let polled = tokio::select! {
            () = ctx.shutdown.wait() => return Ok(LoopEnd::Shutdown),
            joined = poll => joined.unwrap_or_else(|_| Err(ChannelError::Retryable("join".to_owned()))),
        };
        polls += 1;
        match polled {
            Ok(commands) => {
                backoff.reset();
                status.last_poll_at = Some(now);
                for command in &commands {
                    let at = (ctx.now)();
                    if let Some(id) = super::intake::command_id_of(command) {
                        status.last_command = Some(id);
                    }
                    ctx.pipeline.process(command, at).await?;
                    if ctx.shutdown.is_set() {
                        break;
                    }
                }
                if let Err(e) = report_all(&ctx, (ctx.now)()).await {
                    if e == ChannelError::Revoked {
                        delete_local_keys(&ctx.state);
                        ctx.logger.error(
                            "revoked",
                            "the gateway revoked this agent; local keys deleted",
                        );
                        return Ok(LoopEnd::Revoked);
                    }
                    ctx.logger.warn("report_failed", &e.to_string());
                }
                status.backlog = ctx.pipeline.backlog().unwrap_or(0);
                status.clock_skew = ctx.client.clock_skew();
                write_status(&ctx, &status);
                {
                    let client = ctx.client.clone();
                    let series = vec![
                        ("agent_backlog".to_owned(), status.backlog.to_string()),
                        (
                            "clock_skew_seconds".to_owned(),
                            status.clock_skew.to_string(),
                        ),
                    ];
                    let at = (ctx.now)();
                    let state = if status.backlog > 0 { "busy" } else { "idle" };
                    let beat =
                        tokio::task::spawn_blocking(move || client.heartbeat(&series, state, at))
                            .await;
                    if let Ok(Err(e)) = beat {
                        ctx.logger.warn("heartbeat_failed", &e.to_string());
                    }
                }
                if sleep_or_shutdown(&ctx.shutdown, ctx.poll_pause).await {
                    return Ok(LoopEnd::Shutdown);
                }
            }
            Err(ChannelError::Revoked) => {
                delete_local_keys(&ctx.state);
                ctx.logger.error(
                    "revoked",
                    "the gateway revoked this agent; local keys deleted",
                );
                return Ok(LoopEnd::Revoked);
            }
            Err(error) => {
                let delay = backoff.next_delay((ctx.jitter)());
                ctx.logger.warn(
                    "poll_failed",
                    &format!("{error}; retrying in {}s", delay.as_secs()),
                );
                status.backlog = ctx.pipeline.backlog().unwrap_or(0);
                write_status(&ctx, &status);
                if sleep_or_shutdown(&ctx.shutdown, delay).await {
                    return Ok(LoopEnd::Shutdown);
                }
            }
        }
    }
}

/// The TLS enrolment transport for `url`: trusts exactly the certificates in the CA file.
///
/// # Errors
/// [`AgentError::Transport`] when no CA file is given (there is no system trust store
/// fallback) or it is unusable.
pub fn enrol_gateway(
    url: &str,
    ca: Option<&std::path::Path>,
) -> Result<Box<dyn EnrolGateway>, AgentError> {
    let ca = ca.ok_or_else(|| {
        AgentError::Transport(
            "--ca is required: the agent has no system trust store fallback".to_owned(),
        )
    })?;
    let pem = std::fs::read_to_string(ca)
        .map_err(|_| AgentError::Transport("cannot read the CA file".to_owned()))?;
    let _ = parse_pem_certificates(&pem)?;
    let transport: Arc<dyn Transport> = Arc::new(TlsTransport::new(url, &pem)?);
    Ok(Box::new(super::client::HttpsEnrolGateway::new(transport)))
}

/// The production entry: builds the TLS transport from the enrolment record, the pipeline
/// with the durable replay window, and runs the loop until shutdown.
///
/// # Errors
/// Configuration, state and transport errors; revocation ends with `Ok`.
pub async fn serve(rt: Runtime) -> Result<(), AgentError> {
    let config: &AgentConfig = &rt.config;
    let ca_path = config.ca.as_deref().ok_or_else(|| {
        AgentError::ConfigInvalid(
            "`ca` is required: there is no system trust store fallback".to_owned(),
        )
    })?;
    let pem = std::fs::read_to_string(ca_path)
        .map_err(|_| AgentError::ConfigInvalid("cannot read the CA file".to_owned()))?;
    let transport: Arc<dyn Transport> = Arc::new(TlsTransport::new(&config.gateway, &pem)?);
    let client = Arc::new(Client::new(
        transport,
        rt.key_store.clone(),
        rt.identity.agent_id.clone(),
    ));
    let now_fn: Arc<dyn Fn() -> i64 + Send + Sync> = Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .unwrap_or(0)
    });
    let nonce = Arc::new(FileNonceStore::open(nonce_path(rt.state.root()), now_fn())?);
    let catalogue = crate::handlers::build_catalogue().map_err(|_| {
        AgentError::ConfigInvalid("the operation catalogue is inconsistent".to_owned())
    })?;
    let redactor =
        Arc::new(build_redactor(&[]).ok_or_else(|| {
            AgentError::ConfigInvalid("the redactor could not be built".to_owned())
        })?);
    let pipeline = Arc::new(Pipeline::new(PipelineParts {
        agent_id: rt.identity.agent_id.clone(),
        catalogue,
        trust: rt.trust.clone(),
        policy: spot_agent_core::Policy::default(),
        nonce,
        critical: Box::new(|| Box::new(spot_agent_core::NoCritical)),
        log: rt.audit.clone(),
        handlers: rt.handlers.clone(),
        store: FileCommandStore::open(rt.state.commands_dir())?,
        redactor,
        logger: rt.logger.clone(),
        drain: Duration::from_secs(30),
        shutdown: rt.shutdown.clone(),
    }));
    let jitter: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| {
        let mut b = [0u8; 2];
        rand::Rng::fill(&mut rand::rng(), &mut b);
        u64::from(u16::from_le_bytes(b)) % 1001
    });
    rt.logger.info(
        "channel",
        "connecting outbound to the gateway (no listening port)",
    );
    let end = run_loop(LoopCtx {
        client,
        pipeline,
        state: rt.state.clone(),
        logger: rt.logger.clone(),
        shutdown: rt.shutdown.clone(),
        poll_pause: Duration::from_secs(config.poll_seconds),
        now: now_fn,
        jitter,
        max_polls: None,
    })
    .await?;
    rt.logger.info("channel_end", &format!("{end:?}"));
    Ok(())
}
