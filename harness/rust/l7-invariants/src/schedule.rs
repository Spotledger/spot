//! The deterministic operation schedule of a run: which worker does what, in which order.
//!
//! The schedule is a pure function of the seed, the number of workers and the number of
//! operations, so a failing seed replays the same plan. What the plan cannot fix is the
//! interleaving of the workers on real threads: replay determinism covers the *schedule* only.

use crate::history::{IncrementMode, Level, OpKind};

/// A small, seeded xorshift generator (the pattern of `spot-postgres/tests/query_parity.rs`).
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator for `seed`; any seed, including 0, works.
    pub fn new(seed: u64) -> Rng {
        // splitmix64 step, so that nearby seeds give unrelated streams and 0 is not special.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(if z == 0 { 0x2545_F491_4F6C_DD1D } else { z })
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..n` (0 when `n` is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

/// How big a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// 8 workers, about 2,000 operations: fast, runs in `cargo test`.
    Smoke,
    /// 32 workers, 50,000 operations.
    Standard,
    /// 200 workers, 1,000,000 operations: long.
    Nightly,
}

impl Profile {
    /// The profile's name as used on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Smoke => "smoke",
            Profile::Standard => "standard",
            Profile::Nightly => "nightly",
        }
    }

    /// Parses a profile name.
    pub fn parse(s: &str) -> Option<Profile> {
        match s {
            "smoke" => Some(Profile::Smoke),
            "standard" => Some(Profile::Standard),
            "nightly" => Some(Profile::Nightly),
            _ => None,
        }
    }

    /// The profile named by `SPOT_L7_PROFILE`, or `smoke` when unset or unknown.
    pub fn from_env() -> Profile {
        std::env::var("SPOT_L7_PROFILE")
            .ok()
            .and_then(|s| Profile::parse(&s))
            .unwrap_or(Profile::Smoke)
    }

    /// Concurrent workers.
    pub fn workers(self) -> u32 {
        match self {
            Profile::Smoke => 8,
            Profile::Standard => 32,
            Profile::Nightly => 200,
        }
    }

    /// Operations over all workers.
    pub fn operations(self) -> u64 {
        match self {
            Profile::Smoke => 2_000,
            Profile::Standard => 50_000,
            Profile::Nightly => 1_000_000,
        }
    }
}

/// Relative weights of the operation mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weights {
    /// Create a party.
    pub create_party: u32,
    /// Create an order.
    pub create_order: u32,
    /// Create an order linked to a party.
    pub create_linked: u32,
    /// Update an order.
    pub update_order: u32,
    /// Delete an order.
    pub delete_order: u32,
    /// Try to delete a party.
    pub delete_party: u32,
    /// Increment a counter.
    pub increment: u32,
    /// Allocate a sequence number.
    pub allocate: u32,
    /// Enqueue outbox events.
    pub emit: u32,
    /// Read-only snapshot check.
    pub read: u32,
}

impl Default for Weights {
    fn default() -> Weights {
        Weights {
            create_party: 4,
            create_order: 14,
            create_linked: 12,
            update_order: 16,
            delete_order: 5,
            delete_party: 2,
            increment: 20,
            allocate: 14,
            emit: 5,
            read: 8,
        }
    }
}

impl Weights {
    fn total(&self) -> u64 {
        [
            self.create_party,
            self.create_order,
            self.create_linked,
            self.update_order,
            self.delete_order,
            self.delete_party,
            self.increment,
            self.allocate,
            self.emit,
            self.read,
        ]
        .iter()
        .map(|w| u64::from(*w))
        .sum()
    }
}

/// How many series the allocation operations spread over.
pub const SERIES_COUNT: u32 = 3;

/// One planned operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedOp {
    /// What to do.
    pub kind: OpKind,
    /// The isolation level it declares.
    pub isolation: Level,
    /// A random number the driver turns into a choice of target (modulo the number of known
    /// candidates at that moment) and into retry jitter.
    pub pick: u64,
}

/// The plan of a whole run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The seed it was generated from.
    pub seed: u64,
    /// The operations of each worker, in order.
    pub workers: Vec<Vec<PlannedOp>>,
}

impl Schedule {
    /// Plans `operations` operations over `workers` workers with the given mix.
    pub fn generate(seed: u64, workers: u32, operations: u64, weights: &Weights) -> Schedule {
        let workers = workers.max(1);
        let mut plan = Vec::new();
        for w in 0..workers {
            let mut rng = Rng::new(seed ^ (u64::from(w) + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93));
            let share = operations / u64::from(workers)
                + u64::from(u64::from(w) < operations % u64::from(workers));
            let ops = (0..share).map(|_| plan_one(&mut rng, weights)).collect();
            plan.push(ops);
        }
        Schedule {
            seed,
            workers: plan,
        }
    }

    /// Total operations planned.
    pub fn total(&self) -> u64 {
        self.workers.iter().map(|w| w.len() as u64).sum()
    }
}

fn level(rng: &mut Rng) -> Level {
    match rng.below(10) {
        0..=4 => Level::ReadCommitted,
        5..=8 => Level::RepeatableRead,
        _ => Level::Serializable,
    }
}

fn plan_one(rng: &mut Rng, w: &Weights) -> PlannedOp {
    let mut roll = rng.below(w.total().max(1));
    let mut take = |weight: u32| -> bool {
        let weight = u64::from(weight);
        if roll < weight {
            true
        } else {
            roll -= weight;
            false
        }
    };
    let pick = rng.next_u64();
    let (kind, isolation) = if take(w.create_party) {
        (OpKind::CreateParty, Level::ReadCommitted)
    } else if take(w.create_order) {
        let lines = rng.below(4) as u32;
        (OpKind::CreateOrder { lines }, level(rng))
    } else if take(w.create_linked) {
        let lines = rng.below(4) as u32;
        let watch = rng.chance(1, 3);
        (OpKind::CreateLinkedOrder { lines, watch }, level(rng))
    } else if take(w.update_order) {
        (OpKind::UpdateOrder, level(rng))
    } else if take(w.delete_order) {
        (OpKind::DeleteOrder, level(rng))
    } else if take(w.delete_party) {
        (OpKind::DeleteParty, Level::ReadCommitted)
    } else if take(w.increment) {
        let mode = if rng.chance(1, 2) {
            IncrementMode::VersionCheck
        } else {
            IncrementMode::RowLock
        };
        (OpKind::IncrementCounter { mode }, level(rng))
    } else if take(w.allocate) {
        let series = rng.below(u64::from(SERIES_COUNT)) as u32;
        let rollback = rng.chance(1, 4);
        (OpKind::AllocateSequence { series, rollback }, level(rng))
    } else if take(w.emit) {
        let events = rng.below(3) as u32;
        (OpKind::Emit { events }, Level::ReadCommitted)
    } else {
        (OpKind::Read, Level::RepeatableRead)
    };
    PlannedOp {
        kind,
        isolation,
        pick,
    }
}
