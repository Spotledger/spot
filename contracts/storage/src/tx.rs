//! Transactions: the unit of consistency (ADR-46, ADR-51).

use std::future::Future;
use std::time::Duration;

use time::OffsetDateTime;

use crate::commands::CommandRecord;
use crate::document::DocumentStore;
use crate::error::{ErrorKind, StorageError, StorageResult};
use crate::feed::Outbox;
use crate::graph::GraphTraversal;
use crate::id::Name;
use crate::locks::Locks;
use crate::query::QueryExecutor;
use crate::schema::Catalog;
use crate::sequence::Sequences;

/// Isolation levels. Each transaction type declares its level; nothing assumes the database
/// default (ADR-46).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// Each statement sees data committed before it started. Use with explicit row and guard
    /// locks for posting paths.
    ReadCommitted,
    /// The whole transaction sees one snapshot; concurrent updates to rows it writes fail with
    /// `SerializationFailure`.
    RepeatableRead,
    /// Behaves as if transactions ran one after another; conflicts fail with
    /// `SerializationFailure`.
    Serializable,
}

/// Read-write or read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// May write.
    ReadWrite,
    /// Any write fails with `ReadOnlyTransaction`.
    ReadOnly,
}

/// How a transaction runs. Built per transaction type, for example "post sales invoice".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOptions {
    /// The transaction type, for tracing, metrics and retry policy.
    pub label: Name,
    /// Declared isolation level.
    pub isolation: Isolation,
    /// Read-write or read-only.
    pub access: Access,
    /// Longest wait for any lock taken with [`Wait::Block`](crate::Wait::Block).
    pub lock_timeout: Duration,
    /// Longest time for any single operation.
    pub statement_timeout: Duration,
    /// Most records the transaction may insert, update or delete, counting child rows and
    /// outbox events (ADR-45). Exceeding it fails with `TransactionTooLarge`; bulk work is
    /// chunked into several transactions.
    pub max_writes: u32,
}

impl TxOptions {
    /// Read-write options with the standard limits: 5 s lock timeout, 30 s statement timeout,
    /// 10,000 writes. The isolation level has no default: it must be declared.
    pub fn new(label: Name, isolation: Isolation) -> Self {
        Self {
            label,
            isolation,
            access: Access::ReadWrite,
            lock_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(30),
            max_writes: 10_000,
        }
    }

    /// The same options, read-only.
    pub fn read_only(mut self) -> Self {
        self.access = Access::ReadOnly;
        self
    }
}

/// A savepoint inside a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Savepoint(pub u32);

/// One open transaction against one tenant's store.
///
/// Everything done through it (documents, queries, traversals, locks, sequence numbers, outbox
/// events) commits or rolls back together. Dropping it without `commit` rolls it back. After any
/// error the transaction may still be used only if the error kind is `NotFound`, `StaleVersion`,
/// `AlreadyExists`, `UniqueViolation`, `InvalidValue`, `InvalidQuery`, `UnknownEntity`,
/// `UnknownField` or `LockNotAvailable`, and only when the failed operation ran inside a
/// savepoint that was rolled back; otherwise roll back the whole transaction.
pub trait Transaction:
    DocumentStore + QueryExecutor + GraphTraversal + Locks + Sequences + Outbox + Send
{
    /// The catalog snapshot this transaction runs against.
    fn catalog(&self) -> &Catalog;

    /// The options it was opened with.
    fn options(&self) -> &TxOptions;

    /// Reads the command record for `(principal, command_id)` written by a transaction that
    /// committed, or by this transaction. `None` when there is none (never written, pruned, or
    /// rolled back). Added in v0.3.0.
    ///
    /// The default answers `Unsupported`, so an implementer that has not adopted the command
    /// ledger (WP-DOC-04.T8) still compiles; the document engine treats `Unsupported` as "no
    /// ledger on this backend" and refuses to claim replay.
    fn command_get(
        &self,
        principal: &str,
        command_id: &str,
    ) -> impl Future<Output = StorageResult<Option<CommandRecord>>> + Send {
        let _ = (principal, command_id);
        async { Err(command_ledger_unsupported()) }
    }

    /// Inserts a command record in this transaction; it commits or rolls back with it. A second
    /// insert of the same `(principal, command_id)` fails with `UniqueViolation`. The record is
    /// validated first ([`CommandRecord::validate`]). The backend never reads the clock: the
    /// writer sets `recorded_at` and `retain_until`. Default: `Unsupported`.
    fn command_put(
        &mut self,
        record: CommandRecord,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        let _ = record;
        async { Err(command_ledger_unsupported()) }
    }

    /// Deletes every committed record whose `retain_until` is at or before `before` (`retain_until <= before`), in this
    /// transaction, and returns how many. It looks only at `retain_until`, never at
    /// `recorded_at`, so a record written under a longer policy than today's setting is kept
    /// until its own date. Default: `Unsupported`.
    fn command_prune(
        &mut self,
        before: OffsetDateTime,
    ) -> impl Future<Output = StorageResult<u64>> + Send {
        let _ = before;
        async { Err(command_ledger_unsupported()) }
    }

    /// Marks a point to roll back to.
    fn savepoint(&mut self) -> impl Future<Output = StorageResult<Savepoint>> + Send;

    /// Undoes everything since `savepoint` (including outbox events and sequence numbers) and
    /// discards later savepoints. `savepoint` itself stays usable.
    fn rollback_to(
        &mut self,
        savepoint: Savepoint,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Forgets a savepoint and all later ones, keeping their work.
    fn release(&mut self, savepoint: Savepoint) -> impl Future<Output = StorageResult<()>> + Send;

    /// Commits. `CommitOutcomeUnknown` means the result must be checked before retrying.
    fn commit(self) -> impl Future<Output = StorageResult<()>> + Send;

    /// Rolls back everything.
    fn rollback(self) -> impl Future<Output = StorageResult<()>> + Send;
}

fn command_ledger_unsupported() -> StorageError {
    StorageError::new(
        ErrorKind::Unsupported,
        "this backend does not implement the command ledger (storage contract 0.3.0)",
    )
}
