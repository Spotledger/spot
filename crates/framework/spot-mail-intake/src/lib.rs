//! The incoming-mail engine (WP-PLT-11.T12).
//!
//! `spot-mail` fetches new messages out of an IMAP mailbox and parses them; the cursor is left
//! to the caller. This crate is the caller: one poll step per mailbox account ([`poll_once`])
//! that is safe to repeat, to crash and to run on two nodes, and the ports it needs. The host
//! (WP-PLT-11.T13) runs the step in a loop; the PostgreSQL stores are `spot-mail-intake-pg`.
//!
//! - [`Account`]: where to poll, how often, the limits. A credential reference, never a secret.
//! - Ports, each with an in-memory fake in [`memory`] and, for the stores, a contract suite in
//!   `contract` (feature `testing`): [`CursorStore`], [`SeenStore`], [`IntakeSink`],
//!   [`IntakeAudit`], [`CredentialSource`] and [`MailFetcher`].
//! - [`poll_once`]: lease, load the cursor, fetch, drop auto-replies, own mail and over-cap mail,
//!   skip what the seen store knows, hand the rest to the sink, advance the cursor past handled
//!   mail only. Reports and audit entries hold counts, UID positions and reason codes: never an
//!   address, a subject, a body or a Message-ID.
//!
//! The crate never marks, moves or deletes mail on the server, never reads a clock (time is an
//! argument) and offers no plaintext IMAP.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod account;
#[cfg(any(test, feature = "testing"))]
pub mod contract;
pub mod fetch;
pub mod marker;
pub mod memory;
pub mod poll;
pub mod ports;
pub mod report;

pub use account::{Account, AccountError, ImapEndpoint, IntakeLimits, is_storage_name};
pub use fetch::GuardedImapFetcher;
pub use marker::OwnMailKey;
pub use poll::{
    BASE_BACKOFF, MAX_BACKOFF, MORE_DELAY, OUTGOING_MARKER_HEADER, PollDeps, PollState, backoff,
    dedupe_key, poll_once,
};
pub use ports::{
    Accepted, CredentialError, CredentialSource, CursorStore, DedupeKey, IntakeAudit, IntakeSink,
    MailFetcher, SeenStore, SinkError, StoreError,
};
pub use report::{AuditEvent, DropCounts, PollOutcome, PollReport};

/// The stable error and reason codes of this crate (`spot-errors` catalogue `mail_intake.*`).
pub mod codes {
    /// An account breaks a rule of the model.
    pub const ACCOUNT_INVALID: &str = "mail_intake.account_invalid";
    /// The server refused the credentials; polling stopped until an operator acts.
    pub const AUTH_FAILED: &str = "mail_intake.auth_failed";
    /// The mailbox's `UIDVALIDITY` changed.
    pub const MAILBOX_RESET: &str = "mail_intake.mailbox_reset";
    /// The sink rejected a message as a poison message.
    pub const SINK_REJECTED: &str = "mail_intake.sink_rejected";
    /// A cursor or seen store could not be reached.
    pub const STORE_UNAVAILABLE: &str = "mail_intake.store_unavailable";
    /// A message came back that this site sent itself (a valid `X-Spot-Outgoing` marker); it was
    /// dropped before the sink.
    pub const OWN_MAIL: &str = "mail_intake.own_mail";
    /// The account's daily cap was reached.
    pub const DAILY_CAP_REACHED: &str = "mail_intake.daily_cap_reached";
}

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mail-intake";
