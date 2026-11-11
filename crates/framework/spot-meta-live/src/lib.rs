//! `spot-meta-live` — the versioned metadata snapshot the document engine reads through
//! (WP-MET-03.T1; ADR-05, ADR-20).
//!
//! `spot-meta` merges and validates definitions into an immutable [`EffectiveMeta`], and
//! `spot-plan` compiles it into a [`PlanSet`]. This crate is what makes that pair **live**:
//! the engine reads one handle, [`LiveMeta`], and a change on any node becomes a whole new
//! snapshot built off to the side, validated, and swapped in atomically.
//!
//! - [`LiveMeta`] holds `Arc<Snapshot>` — the effective metadata, its compiled plans, the
//!   site version and a content hash. [`LiveMeta::current`] is a lock-free pointer read, so a
//!   request that started on version N finishes on N (it keeps the `Arc` it took).
//!   [`LiveMeta::apply`] builds and validates the new snapshot **before** swapping: a failed
//!   build leaves the old snapshot serving and returns every error at once.
//! - [`InvalidationBus`] tells the other nodes a snapshot changed. [`InProcessBus`] is the
//!   single-node default (an in-process broadcast); [`PollingBus`] writes and reads a
//!   [`VersionStore`] the database implements later (the `storage` feature of this crate is one
//!   such store). The polling backend runs **no timers**: the host loop calls
//!   [`PollingBus::poll_once`] and [`Follower::poll_once`] at the interval [`PollSchedule`]
//!   derives from the injected [`Clock`](spot_core::Clock).
//! - [`Follower`] is one node: given the bus, the store and a [`MetaLoader`], it loads,
//!   builds and swaps on each invalidation, and reconciles from the store on a bus gap or a
//!   restart. It never goes backwards.
//!
//! # Divergence
//!
//! Versions rise strictly per site, and each snapshot carries a content hash of its
//! definitions. Two nodes that hold the *same version number with different content* refuse to
//! guess: the receiving node raises [`LiveError::Divergence`] (`metalive.divergence`) and
//! keeps serving its own snapshot. The hash is deliberately independent of the version, so a
//! version-only comparison cannot mask a real difference.
//!
//! # What this crate does not do
//!
//! It does not run a background thread or timer, read the system clock or sleep; the host
//! drives [`Follower::deliver_next`] and [`Follower::poll_once`]. It does not persist
//! definitions (WP-MET-01.T2); a [`MetaLoader`] supplies them by version. It does not add a
//! Redis dependency — that is the optional plugin of WP-PLT-19, which implements the same
//! [`VersionStore`] and [`InvalidationBus`] traits.
//!
//! # Example
//!
//! ```
//! use spot_core::SiteId;
//! use spot_meta::Definition;
//! use spot_meta_live::{LiveMeta, MetaChange};
//!
//! let site = SiteId::new("acme.example").unwrap_or_else(|e| panic!("valid site: {e}"));
//! let live = LiveMeta::new(site, vec![Definition::new("widget")])
//!     .unwrap_or_else(|e| panic!("the initial metadata builds: {e}"));
//! let before = live.current().version();
//! live.apply(MetaChange::SetProperty {
//!     doctype: "widget".into(),
//!     field: None,
//!     property: spot_meta::Property::Label("Widget".into()),
//! })
//! .unwrap_or_else(|e| panic!("a label override builds: {e}"));
//! assert!(live.current().version() > before);
//! ```
//!
//! [`EffectiveMeta`]: spot_meta::EffectiveMeta
//! [`PlanSet`]: spot_plan::PlanSet

pub mod bus;
pub mod error;
pub mod follower;
pub mod invalidation;
pub mod live;
pub mod polling;
pub mod snapshot;

#[cfg(feature = "storage")]
pub mod storage;

pub use bus::{
    BusFuture, DEFAULT_BUS_CAPACITY, InProcessBus, InvalidationBus, InvalidationStream, next_event,
};
pub use error::{LiveError, LiveResult};
pub use follower::{Follower, LoadFuture, MetaLoader};
pub use invalidation::{BusEvent, Invalidation, InvalidationKind};
pub use live::{Adopt, LiveMeta, MetaChange};
pub use polling::{MemoryVersionStore, PollSchedule, PollingBus, StoreFuture, VersionStore};
pub use snapshot::{Definitions, MetaHash, Snapshot, Version};

#[cfg(feature = "storage")]
pub use storage::StorageVersionStore;
