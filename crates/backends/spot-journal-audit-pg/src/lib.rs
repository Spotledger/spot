//! The PostgreSQL sides of the journal audit (WP-JRN-07.T6, ADR-061 s8 and s9).
//!
//! `spot-journal-audit-run` defines the two sides it audits through
//! [`LiveSide`](spot_journal_audit_run::LiveSide) and
//! [`ScratchSide`](spot_journal_audit_run::ScratchSide), and only in-memory ones existed. This crate supplies the real ones, so the
//! scheduled job can audit a tenant's database:
//!
//! * [`PgLiveSide`] reads the tenant's **real** database. It begins one `REPEATABLE READ`
//!   **read-only** transaction and keeps it for its whole life, reads the change feed's head
//!   inside it, and answers its head, summaries and fingerprints from that one snapshot. A position other than the recorded head is
//!   refused: the live database cannot be read in the past. Record commits that happen during the
//!   run do not count, because the snapshot is fixed. Read-only is the transaction's own mode, and
//!   no statement this crate sends for the live side writes.
//!
//! * [`PgScratchSide`] is a throwaway **schema** in the tenant's own database
//!   (`spot_scratch_<tenant>_<run>`) holding the catalogue's tables. The rebuild applies its
//!   images into it through [`ApplySink`](spot_journal_rebuild::ApplySink); the side then answers `summaries()` and
//!   `fingerprints()` from those tables by the same code the live side uses. It is dropped by
//!   `ScratchSide::discard`, by its `Drop` (the guard: every path, including an unwind), and,
//!   for a process that died first, by [`PgScratchSide::sweep`].
//!
//! # One digest, one order
//!
//! Both sides digest a record exactly as `spot-journal-audit-run`'s in-memory sides do: the audit
//! digest (`spot_journal_audit::content_digest`) of the canonical JSON of the record's image body,
//! the ADR-055 value encoding ([`crate::encode`] states every rule and matches the native API's
//! `codec`). Records stream sorted by entity, then id, in bounded keyset pages, so memory stays
//! constant in the size of the tables. A child table is part of its parent's body and never a
//! record of its own.
//!
//! # Financial fingerprints
//!
//! The named numeric sets of WP-MIG-06 (Trial Balance, Stock Balance, AR and AP ageing) are
//! declared, not hard-coded: [`FingerprintQuery`] names the entity, its key columns and the summed
//! column, and the amount is an exact decimal or integer minor units — never a float (ADR-12). A
//! declaration whose entity the catalogue does not hold is skipped, so a tenant stage without the
//! ledger answers **no** fingerprints and the compare covers records only; Foundation and ERP
//! register the real declarations when they exist (WP-MIG-06.T1's engine).
//!
//! # The held session and the bridge
//!
//! The sides are the synchronous, class-C case of ADR-082: `spot-pg-bridge` is the only way they
//! reach PostgreSQL, and no private `block_on` exists here. Two details follow from the bridge's
//! shape and are worth knowing before reading [`crate::session`]:
//!
//! * the bridge hands a connection to a call and takes it back when the call returns, and a call
//!   cannot outlive the bridge's `max_deadline`; a transaction therefore cannot be held *across*
//!   calls. The live side keeps **one call open for its whole life** instead: a thread it owns
//!   runs it, the transaction is begun inside it, and requests reach it over a channel. Sizing
//!   `max_deadline` past the longest audit of the host is the host's job (see the README).
//! * the audit runner is driven inside a tokio runtime context (`Handle::block_on`), where the
//!   bridge refuses a call (`pg_bridge.wrong_thread`). Every statement of these sides therefore
//!   runs on a plain thread the side owns, never on the caller's thread.
//!
//! # Tenant isolation
//!
//! One database per tenant (ADR-34), so no statement carries a tenant column: a side carries the
//! tenant it was opened for and refuses any call that names another
//! ([`crate::error::TENANT_MISMATCH`]), and the scratch schema lives in that tenant's own
//! database.

#![deny(missing_docs)]

mod ddl;
mod encode;
mod error;
mod live;
mod pager;
mod scratch;
mod session;
mod sql;

pub use crate::encode::body_digest;
pub use crate::error::PgSideError;
pub use crate::live::{DEFAULT_DEADLINE, PgLiveSide};
pub use crate::scratch::{PgScratchSide, SCHEMA_PREFIX};
pub use crate::sql::{AmountForm, FingerprintQuery};

/// Rows per keyset page when a side is opened with [`PgLiveSide::open`] or
/// [`PgScratchSide::create`]: small enough that a page of images is a bounded amount of memory,
/// large enough that a table of any size is a handful of round trips per thousand records.
pub const DEFAULT_PAGE_SIZE: i64 = crate::live::DEFAULT_PAGE_SIZE;
