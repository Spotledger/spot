//! The hub relay of sync (ADR-062 section 3, ADR-063 section 5, WP-SYN-02.T1).
//!
//! [`HubRelay`] turns the hub's stream into what each destination node is sent. It composes
//! `spot-sync-closure` (bundles that never leave a record without its dependencies),
//! `spot-sync-skew` (hold what a node cannot decode yet) and `spot-sync-obligations` (the
//! mandatory lane is served first) behind traits: [`HubStream`], [`SendQueue`],
//! [`DestinationScope`] and [`AckObserver`], plus the closure's `ImageSource`, `References` and
//! `Redactor`. There is no network, clock, randomness, database or async runtime here.
//!
//! A record a version-skew hold keeps back never reaches the node by the side door: the relay
//! tells the closure which records are held, so none of them is pulled in as a dependency, and a
//! record that needs one is withheld with it ("dependency held",
//! [`DestReport::held_by_dependency`]) until the node upgrades and both are queued again in
//! position order.
//!
//! # Security and tenancy
//!
//! * Scope filtering is the access control of sync. A destination is sent an entry only when its
//!   [`DestinationScope`] covers it, or when it holds the record as an extra (the closure's
//!   extras set). A record of another tenant or another source stream is dropped for every
//!   destination, and an entry is never sent back to the node it came from.
//! * Redaction (`excluded_fields.down`) runs inside the closure build, before anything is
//!   queued: the queue never holds an unredacted image.
//! * Each destination has its own queue, hold set, extras set and acknowledgement. One
//!   destination's backlog, failure or full queue never changes what another receives.
//! * Errors and reports carry entity names, ids, positions and node ids only.
//!
//! # Trust
//!
//! Which value comes from where, for the three hostile-node bounds:
//!
//! | Value | From | Used for |
//! | --- | --- | --- |
//! | stream head | the hub: [`HubStream::head`], never the relay cursor | bounds every node-reported position (ack prefix, catch-up summary) |
//! | receipt time | the hub: the host clock port, an argument | the stamp window and the posting time |
//! | destination / node identity | the hub: the authenticated session | ledger key, scoping of the gap, finding node |
//! | tenant | the hub: [`RelayConfig::tenant`], never a node value | the tenant half of every ledger call (`on_ack`, `on_checkin`, `set_holds`, `register_node`, `register_mandatory`, `who_is_missing`) and the stream head it is bounded by |
//! | mandatory events | the hub: the ack ledger | stale-document findings |
//! | applied prefix, stamp offset, stamped time | the **node**: an argument to be bounded | never used unbounded; `u64::MAX` is refused, the offset is clamped, an unrepresentable stamp is rejected |
//!
//! The tenant is the relay's configuration, never a node value: two relays on one ledger can never
//! satisfy each other's mandatory events.
//!
//! A refused ack or summary marks the destination *suspect* in the next [`TickReport`]
//! (`suspect`, `suspect_events`); raising the condition is WP-SYN-09.
//!
//! The relay also verifies the [`HubStream`] contract instead of assuming it. Every read, in
//! [`HubRelay::tick`] and in its catch-up path, is checked before any bundle is built:
//!
//! * **positions strictly ascend** past the position the read started after
//!   (`sync_hub.stream_disorder`);
//! * **one source transaction is one contiguous run**, and it is never continued by a later read
//!   (`sync_hub.stream_split_txn`): the relay remembers the transaction of the last entry it
//!   processed (`RelayState::last_txn`);
//! * **an entry's lane is a function of its entity** (`sync_hub.lane_not_entity`), remembered per
//!   entity in `RelayState::lane_of_entity` and bounded by [`RelayConfig::max_entities`]
//!   (`sync_hub.stream_entity_limit`, failing closed).
//!
//! A read that breaks the contract is refused whole: nothing is queued, the cursor does not move,
//! the code is on [`TickReport::failed`] and on every destination's report, and what the relay
//! remembers of the stream is left as it was, so the next tick asks again. A transient adapter
//! fault therefore recovers, and a permanent one stays visible instead of being bundled.
//!
//! # Restart
//!
//! The caller persists [`RelayState`] (`snapshot`) and restores it after a restart. The cursor
//! may be older than what was queued: a replay produces the same bundles, and a queue push is
//! idempotent on `(destination, position, bundle index, redelivery)`.

#![warn(missing_docs)]

mod deliver;
mod error;
mod hostile;
mod ledger_calls;
mod memory;
mod model;
mod relay;
mod state;
mod stream_contract;
mod traits;

pub use error::{
    ACK_BEYOND_HEAD, DUPLICATE_DESTINATION, HELD_BY_DEPENDENCY, HubError, IMAGE_MISSING,
    INVALID_CONFIG, LANE_NOT_ENTITY, PREFIX_AHEAD, PREFIX_REGRESSED, QUEUE_FAILED, STAMP_REJECTED,
    STREAM_DISORDER, STREAM_ENTITY_LIMIT, STREAM_FAILED, STREAM_SPLIT_TXN, UNKNOWN_DESTINATION,
};
pub use hostile::{CatchUpOutcome, StampDecision};
pub use memory::{MemoryQueue, MemoryStream};
pub use model::{
    DestReport, EntryRole, PushOutcome, QueuedBundle, RelayConfig, ReleaseReport, StreamEntry,
    TickReport,
};
pub use relay::{Destination, HubRelay};
pub use state::{DestSnapshot, RelayState};
pub use traits::{
    AckBook, AckObserver, DestinationScope, GapSource, HubAudit, HubStream, LedgerBook, NoAudit,
    NoObserver, SendQueue,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-hub";
