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
//! # Deletions
//!
//! A deleted record has no image to read, so an entry with [`StreamEntry::deleted`] is never
//! asked of the `ImageSource` (that would fail with `sync_hub.image_missing`, and the node would
//! keep the record for ever). The relay makes the tombstone from the entry's own key and position:
//! a bundle record with `deleted` set, no image and 0 bytes, which the node applies as a delete.
//! A tombstone follows the rules of any entry: the tenant and source checks, the destination's
//! scope (a delete for a record outside it is not sent), the origin rule, version-skew holds and
//! the extras set. A delete of a record the destination holds as an extra is sent even though the
//! record is out of scope, and the key then leaves the extras set. A record deleted and re-created
//! later is two entries with two positions: both are sent, in position order.
//!
//! # Security and tenancy
//!
//! A tombstone is made by the hub from its own stream, never from anything a node sent, so a
//! hostile node cannot cause a delete in another tenant.
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
//! satisfy each other's mandatory events. [`SharedBook`] is the handle that puts one ledger under
//! the relays of several tenants (one clone per relay, one lock per call).
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
//! # Serving the node's repair pull
//!
//! [`HubRelay::serve_pull`] answers a node's `PullRequest` (the keys a parked bundle lacks) and
//! [`HubRelay::serve_resend`] its `ScopedResend`. A pull is the one place a node names a record id
//! of its own choosing, so the hub re-checks everything for every key and answers
//! [`PullItem::NotAvailable`] identically for a key outside the destination's scope, a key it
//! owns, a held record, an unknown record and another tenant's record: the answer carries no
//! information about records the destination may not see. The operator's counts
//! ([`PullReport`]) are never part of the serialised answer and never carry a key. A served record
//! is built and redacted by the same function as the stream path, without its closure
//! prerequisites (the node asks again for what that record lacks). Work is bounded per request
//! (`RelayConfig::max_pull_keys`) and per destination per tick
//! (`RelayConfig::max_pull_keys_per_tick`, then `sync_hub.pull_throttled`). A re-send is accepted
//! only for the scope string the hub registered for the destination
//! ([`Destination::with_scope_identity`]); it lowers the destination's catch-up position and
//! never raises it, and its position is clamped to the hub's own head.
//!
//! # Acknowledgements and the send queue
//!
//! The queue numbers every item it is given with a **serve sequence** (per destination, strictly
//! increasing, no gaps, durable) and the node acknowledges the highest serve sequence it has
//! applied every earlier one for ([`HubRelay::on_ack_served_at`]). The hub trims by that number
//! only, so an acknowledgement can never claim more than the node applied, whatever the lane
//! order or how far a record's image ran ahead of its entry. The journal prefix the ledger sees
//! is derived by the hub from what is still queued ([`ServedAck::applied_prefix`]). Journal
//! positions order the versions of one record key and carry the obligations; they no longer trim
//! the queue. [`HubRelay::on_ack_at`] (a journal position) is for a position the hub itself can
//! trust, such as the end of a bootstrap; a node's own claim must go through the serve sequence.
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
mod pull;
mod relay;
mod shared;
mod state;
mod stream_contract;
mod traits;

pub use error::{
    ACK_BEYOND_HEAD, DUPLICATE_DESTINATION, HELD_BY_DEPENDENCY, HubError, IMAGE_MISSING,
    INVALID_CONFIG, LANE_NOT_ENTITY, PREFIX_AHEAD, PREFIX_REGRESSED, PULL_THROTTLED, QUEUE_FAILED,
    RESEND_SCOPE_MISMATCH, STAMP_REJECTED, STREAM_DISORDER, STREAM_ENTITY_LIMIT, STREAM_FAILED,
    STREAM_SPLIT_TXN, UNKNOWN_DESTINATION,
};
pub use hostile::{CatchUpOutcome, StampDecision};
pub use memory::{MemoryQueue, MemoryStream};
pub use model::{
    DestReport, EntryRole, PushOutcome, QueuedBundle, RelayConfig, ReleaseReport, ServedAck,
    StreamEntry, TickReport,
};
pub use pull::{PullAnswer, PullItem, PullReport};
pub use relay::{Destination, HubRelay};
pub use shared::{SharedBook, SharedLedgerBook};
pub use state::{DestSnapshot, RelayState};
pub use traits::{
    AckBook, AckObserver, DestinationScope, GapSource, HubAudit, HubStream, LedgerBook, NoAudit,
    NoObserver, SendQueue,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-hub";
