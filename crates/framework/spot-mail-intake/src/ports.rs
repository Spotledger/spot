//! The ports of the engine: the cursor store, the seen store, the sink, the audit trail, the
//! credential source and the mailbox fetcher. Each has an in-memory fake in [`crate::memory`]
//! and, for the two stores, a contract suite in [`crate::contract`] (feature `testing`).

use async_trait::async_trait;
use spot_coordinator::FencingToken;
use spot_mail::{FetchBatch, FetchLimits, IncomingMessage, MailError, MailboxCursor, Secret};
use time::OffsetDateTime;

use crate::account::Account;
use crate::marker::OwnMailKey;
use crate::report::AuditEvent;

/// Why a store call failed. Carries no detail on purpose: the text of a failing database can
/// hold a connection string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The store cannot be reached (`mail_intake.store_unavailable`).
    #[error("mail intake store unavailable")]
    Unavailable,
    /// The caller's fencing token is older than the one the store has already seen: another
    /// node has taken over the account.
    #[error("stale fencing token")]
    StaleFence,
}

/// Keeps the UID cursor of each account between polls.
#[async_trait]
pub trait CursorStore: Send + Sync {
    /// The stored cursor of `account`, if any poll ever stored one.
    async fn load(&self, account: &str) -> Result<Option<MailboxCursor>, StoreError>;

    /// Stores `cursor` for `account` under `fence`. A fence lower than the one already stored
    /// is refused with [`StoreError::StaleFence`] and changes nothing; an equal or higher one
    /// is accepted and becomes the stored fence. The check and the write are one step.
    async fn store(
        &self,
        account: &str,
        cursor: MailboxCursor,
        fence: FencingToken,
    ) -> Result<(), StoreError>;
}

/// Remembers which messages an account has already handled, by a digest of the Message-ID.
#[async_trait]
pub trait SeenStore: Send + Sync {
    /// Whether `digest` was recorded for `account`. A read; records nothing.
    async fn contains(&self, account: &str, digest: &str) -> Result<bool, StoreError>;

    /// Records `digest` for `account` at `now`. `true` when it was new, `false` when an earlier
    /// call recorded it (the first record, and its time, stay).
    async fn first_time(
        &self,
        account: &str,
        digest: &str,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError>;

    /// Forgets every digest recorded before `cutoff`, returning how many. The cutoff must leave
    /// a margin longer than any mailbox rescan (a reset mailbox is read again from UID 1), or
    /// an old message can be imported twice.
    async fn purge_older_than(&self, cutoff: OffsetDateTime) -> Result<u64, StoreError>;
}

/// The key a message is deduplicated by: a hex SHA-256 digest of the normalised Message-ID (or,
/// for a message without one, of its mailbox position). Never the id itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupeKey(pub String);

impl DedupeKey {
    /// The hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What the sink did with a message it did not refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// A new document or record was made; the id is the sink's own and is never reported.
    Created(String),
    /// The sink already had this message (it keeps the [`DedupeKey`] with what it creates).
    Duplicate,
    /// The sink chose not to keep the message, for a reason code (a short, fixed word).
    Dropped(&'static str),
}

/// Why the sink did not accept a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SinkError {
    /// Try again later: the sink is unavailable. The batch stops at this message.
    #[error("sink temporarily unavailable")]
    Retryable,
    /// The message can never be accepted (a poison message): it is recorded and skipped. The
    /// word is a fixed reason code.
    #[error("sink rejected the message")]
    Rejected(&'static str),
}

/// Takes a message in: the host turns it into a document.
#[async_trait]
pub trait IntakeSink: Send + Sync {
    /// Accepts one message of `account`. `key` is the deduplication key; a sink that stores it
    /// together with what it creates is idempotent even across a crash between this call and
    /// the engine's own record of it.
    async fn accept(
        &self,
        account: &Account,
        message: &IncomingMessage,
        key: &DedupeKey,
    ) -> Result<Accepted, SinkError>;
}

/// Records events that an operator reviews. Entries hold counts, UID positions and reason
/// codes only.
pub trait IntakeAudit: Send + Sync {
    /// Records one event of `account`.
    fn record(&self, account: &str, event: AuditEvent);
}

/// Why a credential could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The reference resolves to nothing. An operator must fix the account.
    #[error("credential not found")]
    NotFound,
    /// The secret store could not be reached; try again later.
    #[error("credential service unavailable")]
    Unavailable,
}

/// Turns an account's credential reference into its password.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Resolves `credential_ref`. The secret is never logged or stored by the engine.
    async fn resolve(&self, credential_ref: &str) -> Result<Secret, CredentialError>;

    /// The key that verifies the `X-Spot-Outgoing` marker of mail this site sent (WP-PLT-11.T16),
    /// derived from the site's mail secret; `None` when the site has none, in which case own
    /// mail is recognised by the `From` address only. The default is `None`.
    async fn own_mail_key(&self) -> Option<OwnMailKey> {
        None
    }
}

/// Fetches one batch from a mailbox. The production implementation is
/// [`crate::fetch::GuardedImapFetcher`]; tests script their own.
#[async_trait]
pub trait MailFetcher: Send + Sync {
    /// One batch past `cursor`, within `limits`, for `account` logged in with `secret`.
    async fn fetch(
        &self,
        account: &Account,
        secret: Secret,
        cursor: Option<MailboxCursor>,
        limits: FetchLimits,
    ) -> Result<FetchBatch, MailError>;
}
