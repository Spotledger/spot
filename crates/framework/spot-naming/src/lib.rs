//! `spot-naming` — document naming and gapless number allocation (WP-DOC-05; ADR-46,
//! ADR-053, ADR-059 "Numbering").
//!
//! The document engine does not parse naming patterns: [`spot_plan`] compiles a DocType's
//! naming property into a [`NamingPlan`](spot_plan::NamingPlan) once. This crate turns that
//! plan plus a [`Document`] into a record name, and takes the counter from the storage layer's
//! transactional allocator ([`Sequences`]).
//!
//! # Strategies
//!
//! The compiled plan decides the strategy:
//!
//! | Plan | Strategy | Name |
//! | --- | --- | --- |
//! | A pattern with a counter (`INV-.YYYY-.#####`) | [`Strategy::Series`] | prefix, date parts and a zero-padded number from the series |
//! | A lone `field:<name>` part | [`Strategy::Field`] | the field's value, validated as a legal name |
//! | A lone `hash` part | [`Strategy::Hash`] | a short id derived from a UUIDv7 |
//! | The literal `prompt` | [`Strategy::Prompt`] | the name the caller supplies |
//!
//! A counter that outgrows its declared width widens rather than truncating.
//!
//! # Series keys and resets
//!
//! A series is keyed by its resolved prefix, date parts included, plus an optional scope key
//! (`SeriesKey::new`, at most 140 bytes; a longer derived key is hashed with a stable prefix).
//! A yearly pattern therefore starts again at 1 each year; a pattern with no date parts never
//! resets.
//!
//! # Gapless allocation
//!
//! [`Namer::name_for`] allocates inside the caller's business transaction: a rollback returns
//! the number, so committed numbers are exactly `1..=n` even when transactions abort. The
//! [`GapPolicy`] decides what callers may *skip*: [`GapPolicy::GaplessStrict`] refuses
//! [`Namer::skip`] and [`Namer::reserve`] (the caller commits a [`NamingTicket`] with
//! [`NamingTicket::commit_name`]); [`GapPolicy::Reserved`] hands out a [`Block`] for a bulk
//! operation that fails once more numbers are consumed than were allocated. No PostgreSQL
//! `SEQUENCE`, in-memory counter or allocation outside the caller's transaction is used.
//!
//! # What this crate reads
//!
//! It never reads the system clock or the server's local zone: the date parts come from
//! [`NamingContext::now`] in [`NamingContext::zone`] (or a business date the caller passes),
//! as ADR-077 requires.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use spot_naming::{Namer, StaticPlans};
//! use spot_plan::NamingPlan;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let plans = StaticPlans::new().with("sales_invoice", NamingPlan::parse("INV-.YYYY-.#####")?);
//! let namer = Namer::new(Arc::new(plans));
//! assert!(namer.strategy("sales_invoice").is_ok());
//! # Ok(())
//! # }
//! ```
//!
//! # Not here
//!
//! The engine integration (calling this from the lifecycle) is WP-DOC-04's job; the Foundation
//! configuration tables that name a policy per company are later (ADR-060). This crate
//! implements the mechanism with the policy passed in.

pub mod error;
pub mod fiscal;
pub mod idsource;
pub mod name;
pub mod namer;
pub mod registry;
pub mod series;
pub mod strategy;
pub mod ticket;

pub use error::NamingError;
pub use fiscal::{CalendarFiscalYear, FiscalYear, FiscalYearStart};
pub use idsource::{FixedIdSource, NamingIdSource, UuidV7Source};
pub use name::RecordName;
pub use namer::{Namer, NamingContext};
pub use registry::{PlanRegistry, StaticPlans};
pub use series::stable_hash_hex;
pub use strategy::Strategy;
pub use ticket::{Block, GapPolicy, NamingTicket};

/// The literal naming pattern that means "the caller supplies the name".
pub const PROMPT: &str = "prompt";

/// The literal part that means a fiscal year (see `strategy` for why it is recognised as text).
pub const FISCAL_YEAR_PART: &str = ".FY";
