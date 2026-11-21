//! The `network-revert <id>` subcommand and the start-up network recovery
//! (WP-ADM-04.T8).
//!
//! `spot-agent-linux-sys` persists a revert ticket and schedules an independent
//! `systemd-run` timer that runs `<exe> network-revert <id>`. This module is the host side of
//! that contract (see the module doc of `spot_agent_linux_sys::host`):
//!
//! * [`network_revert_main`] is the whole subcommand: exactly one decimal `u64`, no
//!   enrolment, no gateway, no network; exit `0` for every outcome and `1` for an error.
//! * [`startup_recovery`] is what `run` calls before the channel client starts and before
//!   any plan is accepted; its result is written to the local audit log and the structured
//!   log, and a failure is remembered in a marker file that `status` reports
//!   ([`recovery_status_lines`]) until the next successful recovery.
//! * [`build_handler`] is the one constructor of the Linux system handler for this path, so
//!   the command vector the timer runs is always `revert_command_for(current_exe)` and the
//!   network state directory is the one the registered handler uses (`<state>/network`).
//!
//! The file is deliberately free of `crate::` paths (it names the library as `spot_agent::`)
//! so the binary (`mod revert;`) and the integration tests (`#[path]`) compile the same
//! source. It reads no clock itself: time is an argument or an injected [`Clock`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_agent::audit::{LocalAuditLog, event_record};
use spot_agent::logging::Logger;
use spot_agent::state::{StateDir, atomic_write, default_state_dir};
use spot_agent_core::ResultLog as _;
use spot_agent_linux_sys::{
    CommandRunner, FileSystem, LinuxSysHandler, REVERT_SUBCOMMAND, RevertOutcome,
    revert_command_for, run_revert,
};
use spot_core::Clock;

/// The message `network-revert` prints on a platform without the Linux network module.
pub const UNSUPPORTED_MESSAGE: &str = "network-revert is not supported on this platform";

/// The stable code of a revert command that cannot be built because the executable path is
/// unknown (a change that cannot be reverted must not start).
pub const NO_EXECUTABLE_CODE: &str = "agent.revert_executable_unknown";

/// The marker file in the state directory that records a failed start-up recovery.
pub const RECOVERY_MARKER: &str = "network-recover.failed";

/// The network revert state directory under the agent state directory: the same path the
/// registered handler uses (`handlers/linux.rs`).
#[must_use]
pub fn network_state_dir(state_root: &Path) -> PathBuf {
    state_root.join("network")
}

/// The shared constructor of the Linux system handler for the revert path: state under
/// `<state_root>/network`, revert command `revert_command_for(exe)`.
#[must_use]
pub fn build_handler(
    state_root: &Path,
    exe: &Path,
    runner: Arc<dyn CommandRunner>,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
) -> LinuxSysHandler {
    LinuxSysHandler::new(runner, fs, clock)
        .with_state_dir(network_state_dir(state_root))
        .with_revert_command(revert_command_for(exe))
}

/// The path of the running executable, for [`build_handler`].
///
/// # Errors
/// A short message (with [`NO_EXECUTABLE_CODE`]) when the path cannot be resolved.
pub fn current_executable() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|_| format!("{NO_EXECUTABLE_CODE}: the agent executable path cannot be resolved"))
}

/// What the `network-revert` subcommand wants done: the line to print and the exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevertExit {
    /// Process exit code: 0 for every outcome, 1 for an error.
    pub code: u8,
    /// The one line to print (stdout for 0, stderr for 1).
    pub line: String,
}

/// The one printed line of an outcome.
#[must_use]
pub fn outcome_line(outcome: RevertOutcome) -> &'static str {
    match outcome {
        RevertOutcome::Reverted => "network revert: reverted",
        RevertOutcome::AlreadyDone => "network revert: already done",
        RevertOutcome::NotThisTicket => "network revert: not this ticket",
    }
}

/// Runs the subcommand against `handler`: `args` is what follows `network-revert`. The error
/// message names what was wrong (the library's messages carry ids and interface names, never
/// a command line with a secret).
#[must_use]
pub fn run_network_revert(args: &[String], handler: &LinuxSysHandler) -> RevertExit {
    match run_revert(args, handler) {
        Ok(outcome) => RevertExit {
            code: 0,
            line: outcome_line(outcome).to_owned(),
        },
        Err(error) => RevertExit {
            code: 1,
            line: format!("network revert failed: {error}"),
        },
    }
}

/// Splits a command line `[--state-dir DIR] network-revert <args...>`: the state directory
/// override and the arguments after the subcommand, or `None` when the command is another
/// one (the ordinary parser handles it).
#[must_use]
pub fn split_revert_args(args: &[String]) -> Option<(Option<PathBuf>, &[String])> {
    let (state_dir, rest) = match args {
        [flag, dir, rest @ ..] if flag == "--state-dir" => (Some(PathBuf::from(dir)), rest),
        _ => (None, args),
    };
    match rest {
        [name, tail @ ..] if name == REVERT_SUBCOMMAND => Some((state_dir, tail)),
        _ => None,
    }
}

/// The whole subcommand on the real host. Needs no enrolment, no gateway and no network;
/// the timer runs it as root with no environment worth trusting, so nothing but the single
/// argument is read. On a platform without the Linux network module it exits 1 with
/// [`UNSUPPORTED_MESSAGE`].
#[must_use]
pub fn network_revert_main(state_dir: Option<PathBuf>, args: &[String]) -> RevertExit {
    if !cfg!(target_os = "linux") {
        return RevertExit {
            code: 1,
            line: UNSUPPORTED_MESSAGE.to_owned(),
        };
    }
    let exe = match current_executable() {
        Ok(exe) => exe,
        Err(message) => {
            return RevertExit {
                code: 1,
                line: message,
            };
        }
    };
    let root = state_dir.unwrap_or_else(default_state_dir);
    let handler = build_handler(
        &root,
        &exe,
        Arc::new(spot_agent_linux_sys::StdCommandRunner::new()),
        Arc::new(spot_agent_linux_sys::StdFileSystem::new()),
        Arc::new(spot_core::SystemClock::new()),
    );
    run_network_revert(args, &handler)
}

/// The result of the start-up recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Whether the recovery ran without error.
    pub ok: bool,
    /// The one-line summary (`Recovery::summary`) or the failure text.
    pub summary: String,
}

/// Deals with a network change left pending by a previous run, before the channel client
/// starts and before any plan is accepted. The summary goes to the local audit log (when
/// there is one) and to the structured log as one event. A failure is logged, remembered in
/// [`RECOVERY_MARKER`] and returned, but never stops the agent starting (the independent
/// timer is the safety net); the next successful recovery removes the marker.
pub fn startup_recovery(
    handler: &LinuxSysHandler,
    state: &StateDir,
    audit: Option<&LocalAuditLog>,
    agent_id: &str,
    logger: &Logger,
    now: i64,
) -> RecoveryReport {
    let mut code: Option<String> = None;
    let report = match handler.recover_network() {
        Ok(recovery) => RecoveryReport {
            ok: true,
            summary: recovery.summary(),
        },
        Err(error) => {
            code = Some(error.code.to_owned());
            RecoveryReport {
                ok: false,
                summary: format!("network recovery failed: {error}"),
            }
        }
    };
    let marker = state.root().join(RECOVERY_MARKER);
    if report.ok {
        let _ = std::fs::remove_file(&marker);
        logger.info("network.recover", &report.summary);
    } else {
        logger.warn("network.recover", &report.summary);
        let _ = atomic_write(&marker, report.summary.as_bytes());
    }
    if let Some(audit) = audit {
        // The outcome token is a fixed word (the chain kind is built from it); the summary,
        // which names ids, interfaces and seconds only, rides in `op`.
        let outcome = if report.ok {
            "recovered"
        } else {
            "recover_failed"
        };
        let mut record = event_record(
            agent_id,
            "agent.network_recover",
            outcome,
            code.as_deref(),
            now,
        );
        record.op = format!("agent.network_recover: {}", report.summary);
        let _ = audit.append(&record);
    }
    report
}

/// The lines `status` adds while the last start-up recovery failed.
#[must_use]
pub fn recovery_status_lines(state: &StateDir) -> Vec<String> {
    match std::fs::read_to_string(state.root().join(RECOVERY_MARKER)) {
        Ok(text) => vec![format!("network recovery: FAILED ({})", text.trim())],
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_unsupported_message_is_stable() {
        assert_eq!(
            UNSUPPORTED_MESSAGE,
            "network-revert is not supported on this platform"
        );
    }

    #[test]
    fn the_subcommand_is_split_off_with_or_without_a_state_dir() {
        let a = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let plain = a(&["network-revert", "5"]);
        let (dir, rest) = split_revert_args(&plain).unwrap();
        assert!(dir.is_none());
        assert_eq!(rest, ["5"]);
        let with = a(&["--state-dir", "/x", "network-revert"]);
        let (dir, rest) = split_revert_args(&with).unwrap();
        assert_eq!(dir.unwrap(), PathBuf::from("/x"));
        assert!(rest.is_empty());
        assert!(split_revert_args(&a(&["run"])).is_none());
        assert!(split_revert_args(&a(&[])).is_none());
        assert!(split_revert_args(&a(&["--state-dir", "/x", "run"])).is_none());
    }
}
