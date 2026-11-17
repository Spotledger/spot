//! The journal shipper (WP-RTB-01.T3, ADR-061 sections 1, 4, 5 and 10).
//!
//! "The journal is the outbox stream, sealed; there is no second pipeline." The relay, the
//! manifest, the segment format, the store seam and the commit driver are libraries; this crate
//! is the loop that reads the outbox and gives it to them:
//!
//! - [`Shipper::step`] reads up to `batch_max_entries` entries after the persisted watermark
//!   `(generation, seq)`, runs each through a [`Protector`] (personal fields sealed), calls the
//!   relay and advances the watermark **only** to a position the relay reports durable.
//! - One writer: the shipper steps only while its [`WriterLease`] is held.
//! - A generation change in the outbox opens a new relay generation through the relay's
//!   `open_fork`; the old generation stays in the journal forever.
//! - After every seal and at least every ten minutes the stored chain is light-verified; a
//!   failure stops the watermark and raises `JOURNAL_GAP`.
//! - [`ProgressHandle`] is what the acknowledgement gate (WP-RTB-01.T4) waits on.
//!
//! The ports ([`ports`]) keep the loop testable without a database: [`memory`] holds in-memory
//! implementations, [`pg`] the PostgreSQL ones.
#![forbid(unsafe_code)]

pub mod check;
pub mod error;
pub mod keys;
pub mod lease;
pub mod memory;
pub mod pg;
pub mod ports;
pub mod progress;
pub mod protect;
pub mod relay_seam;
pub mod scoped;
pub mod shipper;
pub mod target;

pub use check::StoreChecker;
pub use error::ShipFault;
pub use keys::{DerivedSigner, JournalKeys};
pub use lease::CoordinatorLease;
pub use ports::{
    Alarm, AlarmKind, Alarms, ChainChecker, ChainState, ForkMark, KeyBackup, Mark, OutboxEntry,
    OutboxRead, OutboxSource, PortFuture, Protector, RelayFactory, ShipRelay, WatermarkStore,
    WriterLease,
};
pub use progress::{Progress, ProgressHandle};
pub use protect::PassThrough;
pub use relay_seam::{SignerFn, StoreRelayFactory};
pub use scoped::TenantScoped;
pub use shipper::{Ports, Shipper, ShipperConfig, Step};
pub use target::check_folder_target;
