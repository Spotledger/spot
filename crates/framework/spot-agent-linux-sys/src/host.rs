//! The host entry points for the persisted network revert (WP-ADM-04.T6).
//!
//! WP-ADM-04.T5 persists the revert ticket and schedules an independent `systemd-run`
//! timer, but a library cannot run a timer's command. This module is what the future agent
//! binary calls; it is deliberately a library surface, not a binary.
//!
//! # The host's contract
//!
//! 1. Build the [`LinuxSysHandler`] with [`LinuxSysHandler::with_revert_command`] given
//!    [`revert_command_for`] of the agent's own executable. The timer then runs
//!    `<exe> network-revert <ticket id>`.
//! 2. Route the `network-revert` subcommand ([`REVERT_SUBCOMMAND`]) to [`run_revert`] with
//!    the arguments that follow it, and map the result to an exit code: `0` for every
//!    [`RevertOutcome`], `1` for an error (so the timer unit shows the failure and the
//!    ticket stays for another run).
//! 3. Call [`LinuxSysHandler::recover_network`] **before accepting any plan**, and log its
//!    [`crate::Recovery::summary`] as an audit event. It is idempotent: a second call finds
//!    nothing pending.
//!
//! The host binary task (WP-ADM-04.T8) is written once the binary exists.
//!
//! Nothing here reads a clock, runs a shell or touches the machine except through the
//! handler's injected seams.

use std::path::Path;

use crate::error::{INVALID_ARGUMENT, SysError, SysResult};
use crate::handler::LinuxSysHandler;
use crate::network::RevertRun;

/// The subcommand name the timer runs; defined once so the scheduler and the agent binary
/// cannot disagree.
pub const REVERT_SUBCOMMAND: &str = "network-revert";

/// What a `network-revert <id>` run did. Every variant is a success (exit code 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevertOutcome {
    /// The ticket was pending and the previous configuration is restored.
    Reverted,
    /// Nothing is pending: the change was confirmed, or already reverted. Nothing was done.
    AlreadyDone,
    /// A different ticket is pending (this timer is stale). Nothing was done.
    NotThisTicket,
}

/// The command vector for [`LinuxSysHandler::with_revert_command`]: the agent executable
/// and the revert subcommand. The scheduler appends the ticket id.
#[must_use]
pub fn revert_command_for(exe: &Path) -> Vec<String> {
    vec![
        exe.to_string_lossy().into_owned(),
        REVERT_SUBCOMMAND.to_owned(),
    ]
}

/// Runs `network-revert`: `args` are what follows the subcommand, exactly one decimal
/// ticket id.
///
/// # Errors
/// [`INVALID_ARGUMENT`] for anything but exactly one decimal `u64` (the message says what
/// was wrong), and the handler's failures reverting (the ticket stays, so it can run again).
pub fn run_revert(args: &[String], handler: &LinuxSysHandler) -> SysResult<RevertOutcome> {
    let id = parse_ticket_id(args)?;
    Ok(match handler.revert_network_ticket_checked(id)? {
        RevertRun::Reverted(_) => RevertOutcome::Reverted,
        RevertRun::NothingPending => RevertOutcome::AlreadyDone,
        RevertRun::OtherTicket => RevertOutcome::NotThisTicket,
    })
}

/// Parses `[<ticket id>]`: one argument of ASCII digits that fits a `u64`.
fn parse_ticket_id(args: &[String]) -> SysResult<u64> {
    let [text] = args else {
        return Err(SysError::new(
            INVALID_ARGUMENT,
            format!(
                "network-revert takes exactly one ticket id, got {} arguments",
                args.len()
            ),
        ));
    };
    if text.is_empty() {
        return Err(SysError::new(
            INVALID_ARGUMENT,
            "the ticket id is empty; it must be a decimal number",
        ));
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SysError::new(
            INVALID_ARGUMENT,
            format!(
                "the ticket id {} is not a plain decimal number",
                shown(text)
            ),
        ));
    }
    text.parse::<u64>().map_err(|_| {
        SysError::new(
            INVALID_ARGUMENT,
            format!("the ticket id {} is too large", shown(text)),
        )
    })
}

/// An argument quoted and escaped for a message, cut to a readable length.
fn shown(text: &str) -> String {
    let cut: String = text.chars().take(24).collect();
    if cut.len() < text.len() {
        format!("{cut:?}...")
    } else {
        format!("{cut:?}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_command_vector_is_the_executable_and_the_subcommand() {
        assert_eq!(
            revert_command_for(Path::new("/usr/bin/spot-agent")),
            ["/usr/bin/spot-agent", "network-revert"]
        );
    }

    #[test]
    fn only_one_plain_decimal_u64_is_an_id() {
        assert_eq!(parse_ticket_id(&args(&["42"])).unwrap(), 42);
        assert_eq!(
            parse_ticket_id(&args(&["18446744073709551615"])).unwrap(),
            u64::MAX
        );
        for bad in [
            args(&[]),
            args(&[""]),
            args(&["-1"]),
            args(&["+1"]),
            args(&["1;2"]),
            args(&["0x10"]),
            args(&["1 2"]),
            args(&["18446744073709551616"]),
            args(&["1", "2"]),
        ] {
            let error = parse_ticket_id(&bad).unwrap_err();
            assert_eq!(error.code, INVALID_ARGUMENT, "{bad:?}");
        }
    }
}
