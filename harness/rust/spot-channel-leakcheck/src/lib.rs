//! `spot-channel-leakcheck` (WP-SEC-04.T3): the end-to-end proof that each real producer of
//! data calls the field guard.
//!
//! `spot-field-guard` proves its own component with a matrix. What it cannot prove is that the
//! producers *call* it. The tests of this crate drive each producing crate through its public
//! entry point with protected values planted, serialise everything that leaves, and hand it to
//! the guard's leak harness. A producer or a refactor that skips the guard turns the build red.
//!
//! The library holds only the table of channels ([`CHANNELS`]); the tests are in `tests/`.
//! This is a separate crate because it depends on many producers; **no production crate may
//! depend on it**.

use spot_field_guard::Channel;

/// How a channel is covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    /// Covered by the named test function (found by name in this crate's `tests/`), which
    /// drives the named producer crate.
    Test {
        /// The test function name.
        test: &'static str,
        /// The producing crate the test drives.
        producer: &'static str,
    },
    /// Not yet exercisable end to end; the guard prints these so the lead sees them.
    Pending {
        /// Why the channel cannot be exercised.
        reason: &'static str,
        /// The task that unblocks it.
        task: &'static str,
    },
}

/// A channel covered by `test`, which drives `producer`.
#[must_use]
pub const fn covered(test: &'static str, producer: &'static str) -> Coverage {
    Coverage::Test { test, producer }
}

/// A channel that cannot be exercised yet, with the reason and the task that unblocks it.
#[must_use]
pub const fn pending(reason: &'static str, task: &'static str) -> Coverage {
    Coverage::Pending { reason, task }
}

/// Every channel of the guard and how this crate covers it. The coverage test matches
/// [`Channel`] exhaustively, so a new variant fails to compile until it is registered here.
pub const CHANNELS: &[(Channel, Coverage)] = &[
    (
        Channel::Version,
        covered("version_history_never_leaks", "spot-doc-audit"),
    ),
    (
        Channel::Export,
        covered("export_never_leaks", "spot-bulk-data"),
    ),
    (
        Channel::Print,
        pending(
            "spot-print renders whatever context it is given and calls no guard; the host adapter that loads the document as the caller does not exist yet",
            "WP-PRD-01.T8",
        ),
    ),
    (
        Channel::Report,
        covered("report_never_leaks", "spot-reports"),
    ),
    (
        Channel::ListColumn,
        pending(
            "the list path (spot-api-services query) applies the permission engine itself and does not call the guard; it needs the full engine to drive end to end",
            "unregistered (lead to register)",
        ),
    ),
    (
        Channel::LinkTitle,
        pending(
            "the lookup endpoint is a port the host implements; no producer behind it calls the guard yet",
            "unregistered (lead to register)",
        ),
    ),
    (
        Channel::Search,
        pending(
            "spot-retrieval refuses to index protected fields by policy and re-checks reads through a host ReadCheck; no guard call to prove end to end yet",
            "unregistered (lead to register)",
        ),
    ),
    (
        Channel::Notification,
        pending(
            "spot-realtime-bridge forwards events and calls no guard; the notification payload builder does not exist yet",
            "unregistered (lead to register)",
        ),
    ),
    (
        Channel::Journal,
        pending(
            "the journal crates carry sealed record images and calls no guard; the names-only journal projection does not exist yet",
            "unregistered (lead to register)",
        ),
    ),
    (
        Channel::ApiScim,
        pending(
            "spot-iam-scim mirrors users and groups, not DocType records, and calls no guard",
            "unregistered (lead to register)",
        ),
    ),
];

/// The coverage of one channel, or `None` when it is not registered.
#[must_use]
pub fn coverage_of(channel: Channel) -> Option<Coverage> {
    CHANNELS
        .iter()
        .find(|(c, _)| *c == channel)
        .map(|(_, cov)| *cov)
}
