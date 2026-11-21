//! What a poll reports and what the audit trail records. Both hold counts, UID positions and
//! reason codes only: never an address, a subject, a body or a Message-ID.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

/// How one [`crate::poll_once`] call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PollOutcome {
    /// The account is disabled; nothing ran.
    Disabled,
    /// This node does not hold the account's lease (another node polls it, or the store
    /// refused a stale fencing token). Nothing else happened.
    NotLeader,
    /// The poll ran; see the counts and `reason` (a poll may stop early at a retryable sink
    /// error).
    Polled,
    /// The mailbox's `UIDVALIDITY` changed: the new validity was stored, nothing was imported.
    MailboxReset,
    /// The server refused the credentials. Polling stops until an operator clears the state.
    AuthFailed,
    /// The server refused the account's configuration (for example the mailbox does not
    /// exist) or egress refused the server. Polling stops until an operator acts.
    ConfigRejected,
    /// A transient failure; retry after `next_delay`.
    Failed,
}

/// Messages that never reached the sink, by reason code.
pub type DropCounts = BTreeMap<&'static str, u32>;

/// The reason code of an auto-reply or bulk message.
pub const DROP_AUTO_REPLY: &str = "auto_reply";
/// The reason code of a message the account sent itself.
pub const DROP_OWN_MAIL: &str = "own_mail";
/// The reason code of a message over the daily cap.
pub const DROP_DAILY_CAP: &str = "daily_cap";

/// The result of one poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PollReport {
    /// How the poll ended.
    pub outcome: PollOutcome,
    /// A stable reason code when the poll did not complete normally (`sink_retryable`,
    /// `store_unavailable`, `unreachable`, `credential_missing`, ...).
    pub reason: Option<&'static str>,
    /// Messages the server returned.
    pub fetched: u32,
    /// Messages the sink created.
    pub accepted: u32,
    /// Messages skipped or reported as already known.
    pub duplicates: u32,
    /// Messages dropped, by reason code.
    pub dropped: DropCounts,
    /// Messages the sink rejected as poison.
    pub rejected: u32,
    /// Messages that reached the sink as header-only stubs.
    pub truncated: u32,
    /// The lowest and highest UID of the batch, when it held any.
    pub uid_range: Option<(u32, u32)>,
    /// More mail waits in the mailbox than this poll took.
    pub more: bool,
    /// When to poll again; `None` means do not poll until an operator acts.
    pub next_delay: Option<Duration>,
}

impl PollReport {
    pub(crate) fn empty(outcome: PollOutcome) -> Self {
        Self {
            outcome,
            reason: None,
            fetched: 0,
            accepted: 0,
            duplicates: 0,
            dropped: DropCounts::new(),
            rejected: 0,
            truncated: 0,
            uid_range: None,
            more: false,
            next_delay: None,
        }
    }

    /// The number of messages dropped for any reason.
    pub fn dropped_total(&self) -> u32 {
        self.dropped.values().sum()
    }
}

/// One entry of the audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEvent {
    /// The server refused the credentials (`mail_intake.auth_failed`); recorded once until the
    /// state is cleared.
    AuthFailed,
    /// The mailbox's `UIDVALIDITY` changed (`mail_intake.mailbox_reset`).
    MailboxReset {
        /// The validity the cursor held, when there was one.
        old_validity: Option<u32>,
        /// The validity the server reports now.
        new_validity: u32,
    },
    /// The daily cap was reached (`mail_intake.daily_cap_reached`); one entry per day.
    DailyCapReached {
        /// Days since the Unix epoch (UTC).
        day: i64,
        /// The configured cap.
        cap: u32,
    },
    /// Messages carrying a valid own-mail marker were dropped (`mail_intake.own_mail`); one entry
    /// per poll that dropped any.
    OwnMail {
        /// How many messages of the poll were dropped as own mail by their marker.
        count: u32,
        /// The UID of the first of them.
        first_uid: u32,
    },
    /// The sink rejected a message as poison (`mail_intake.sink_rejected`).
    SinkRejected {
        /// The message's UID.
        uid: u32,
        /// The sink's reason code.
        reason: &'static str,
    },
}
