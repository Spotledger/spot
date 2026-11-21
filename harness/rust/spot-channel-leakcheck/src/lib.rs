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
    (Channel::Print, covered("print_never_leaks", "spot-http")),
    (
        Channel::Report,
        covered("report_never_leaks", "spot-reports"),
    ),
    (
        Channel::ListColumn,
        covered("list_column_never_leaks", "spot-api-services"),
    ),
    (
        Channel::LinkTitle,
        covered("link_title_never_leaks", "spot-api-services"),
    ),
    (
        Channel::Search,
        covered("search_never_leaks", "spot-retrieval"),
    ),
    (
        Channel::Notification,
        covered("notification_never_leaks", "spot-realtime-bridge"),
    ),
    (
        Channel::Journal,
        covered("journal_never_leaks", "spot-journal-relay"),
    ),
    (
        Channel::ApiScim,
        covered("api_scim_never_leaks", "spot-iam-scim"),
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
