//! The rolling release upgrade, driven through the real `spot-sync-skew` crate.
//!
//! The World of `spot-sync-chaos` serves one event major and has no per-node release. So the
//! upgrade is run next to it, day by day, with the skew crate's own functions
//! ([`session_decision`], [`filter_outgoing`], [`release_for_upgrade`]) and a stream of
//! synthetic entries whose positions grow with the days. Nothing here reimplements a decision:
//! the model only keeps each node's [`HoldSet`] and queue, and asserts what ADR-066 says must
//! hold.
//!
//! * Release N is software 1.0.0, N+1 is 1.1.0 (one release is one minor step). The hub moves to
//!   N+1 on the first day of the rollout. Release N+1 carries one event type
//!   ([`BUMPED_TOPIC`]) whose major is bumped from 1 to 2: a node at N cannot decode major 2.
//! * Nodes upgrade over the rollout days, a few stay at N for the whole run, and one node starts
//!   one release behind N (0.9.0), so it is within the skew limit (ADR-066 default 1) until the
//!   hub moves and beyond it afterwards: it must be refused with `NODE_TOO_OLD` and the upgrade
//!   channel only, never served.
//! * A record of the bumped type is **held** for an N node and **released in position order** when
//!   the node upgrades; no later version of a key is sent while an earlier one is held.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spot_sync_chaos::Rng;
use spot_sync_skew::{
    Decision, HoldSet, NodeReport, Outgoing, RecordRef, Retry, Settings, SoftwareVersion,
    filter_outgoing, release_for_upgrade, session_decision,
};

use super::plan::SoakSettings;

/// One release behind release N.
pub const RELEASE_N_MINUS_1: SoftwareVersion = SoftwareVersion::new(0, 9, 0);
/// The release every node starts on (except the too-old one).
pub const RELEASE_N: SoftwareVersion = SoftwareVersion::new(1, 0, 0);
/// The release the hub and the rolling upgrade move to.
pub const RELEASE_N_PLUS_1: SoftwareVersion = SoftwareVersion::new(1, 1, 0);
/// The event type whose major is bumped in release N+1.
pub const BUMPED_TOPIC: &str = "soak_ledger_posted";
/// An event type that did not change.
pub const STABLE_TOPIC: &str = "soak_note_saved";
/// The catalogue code a too-old node is refused with.
pub const TOO_OLD_CODE: &str = spot_sync_skew::NODE_TOO_OLD;

/// Who upgrades when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseSchedule {
    /// Per node: the day it upgrades to N+1, or `None` when it never does.
    pub upgrade_day: Vec<Option<u32>>,
    /// The node that starts one release behind N and is refused once the hub moves.
    pub too_old: Option<usize>,
    /// The nodes that stay at N for the whole run.
    pub late: Vec<usize>,
    /// The first day of the rollout (the hub upgrades at its start).
    pub start_day: u32,
    /// The last day a node upgrades on.
    pub end_day: u32,
}

impl ReleaseSchedule {
    /// Draws the schedule: one node too old, about one in twenty late, the rest spread evenly
    /// over the rollout days (about a tenth of the fleet a day for the full run).
    #[must_use]
    pub fn generate(settings: &SoakSettings, rng: &mut Rng) -> Self {
        let mut order: Vec<usize> = (0..settings.nodes).collect();
        rng.shuffle(&mut order);
        let mut upgrade_day: Vec<Option<u32>> = vec![None; settings.nodes];
        let too_old = (settings.nodes >= 3).then(|| order[0]);
        let skip = usize::from(too_old.is_some());
        let late_count = if settings.nodes >= 5 {
            (settings.nodes / 20).max(1)
        } else {
            0
        };
        let late: Vec<usize> = order.iter().skip(skip).take(late_count).copied().collect();
        let rest: Vec<usize> = order.iter().skip(skip + late_count).copied().collect();
        let span = usize::try_from(settings.release_end_day - settings.release_start_day + 1)
            .unwrap_or(1)
            .max(1);
        let per_day = rest.len().div_ceil(span).max(1);
        for (index, node) in rest.into_iter().enumerate() {
            let offset = u32::try_from(index / per_day).unwrap_or(0);
            let day = (settings.release_start_day + offset).min(settings.release_end_day);
            upgrade_day[node] = Some(day);
        }
        Self {
            upgrade_day,
            too_old,
            late,
            start_day: settings.release_start_day,
            end_day: settings.release_end_day,
        }
    }

    /// The schedule as text.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut by_day: BTreeMap<u32, usize> = BTreeMap::new();
        for day in self.upgrade_day.iter().flatten() {
            *by_day.entry(*day).or_insert(0) += 1;
        }
        format!(
            "release rollout days {}..={} upgrades per day {by_day:?} late {:?} too-old {:?}\n",
            self.start_day, self.end_day, self.late, self.too_old
        )
    }
}

/// A node's report at a release: it decodes the bumped type's major 1, and major 2 from N+1 on.
fn report(version: SoftwareVersion) -> NodeReport {
    let majors: BTreeSet<u32> = if version >= RELEASE_N_PLUS_1 {
        BTreeSet::from([1, 2])
    } else {
        BTreeSet::from([1])
    };
    let mut decodable = BTreeMap::new();
    decodable.insert(BUMPED_TOPIC.to_owned(), majors);
    decodable.insert(STABLE_TOPIC.to_owned(), BTreeSet::from([1]));
    NodeReport {
        software_version: version,
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: decodable,
    }
}

type Key = (String, String);

fn key_of(record: &RecordRef) -> Key {
    (record.entity.clone(), record.id.clone())
}

/// One node as the hub's relay sees it: its report, its holds and what is queued for it.
struct ModelNode {
    report: NodeReport,
    holds: HoldSet,
    /// Versions currently held (identity only, never content).
    held: BTreeSet<(String, String, u64)>,
    queue: VecDeque<Outgoing>,
    delivered: Vec<Outgoing>,
    last_version: BTreeMap<Key, u64>,
    upgrade_offers: u64,
    /// How many records were delivered when the hub first refused this node (its queue is then
    /// frozen: a refused node is served nothing more).
    delivered_at_refusal: Option<usize>,
    /// Refused today: its queue is not drained.
    refused_today: bool,
}

/// What a day of the model did, for the trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayStats {
    /// Nodes that upgraded today.
    pub upgraded: usize,
    /// Entries released by those upgrades.
    pub released: usize,
    /// Entries held for nodes today.
    pub held: usize,
    /// Entries delivered to nodes today.
    pub delivered: usize,
    /// Upgrade offers made to the too-old node today.
    pub offers: u64,
}

/// The rolling upgrade, node by node.
pub struct ReleaseModel {
    schedule: ReleaseSchedule,
    skew: Settings,
    nodes: Vec<ModelNode>,
    hub_report: NodeReport,
    next_position: u64,
    versions: BTreeMap<Key, u64>,
    all_positions: BTreeSet<u64>,
    keys: usize,
    per_day: usize,
    rng: Rng,
}

impl ReleaseModel {
    /// A model of `settings.nodes` nodes on the starting releases of `schedule`.
    #[must_use]
    pub fn new(settings: &SoakSettings, schedule: ReleaseSchedule) -> Self {
        let nodes = (0..settings.nodes)
            .map(|index| ModelNode {
                report: report(if schedule.too_old == Some(index) {
                    RELEASE_N_MINUS_1
                } else {
                    RELEASE_N
                }),
                holds: HoldSet::default(),
                held: BTreeSet::new(),
                queue: VecDeque::new(),
                delivered: Vec::new(),
                last_version: BTreeMap::new(),
                upgrade_offers: 0,
                delivered_at_refusal: None,
                refused_today: false,
            })
            .collect();
        Self {
            schedule,
            skew: Settings::default(),
            nodes,
            hub_report: report(RELEASE_N),
            next_position: 0,
            versions: BTreeMap::new(),
            all_positions: BTreeSet::new(),
            keys: 8,
            per_day: 20,
            rng: Rng::new(settings.seed ^ 0x5E1E_A5E0_0000_0002),
        }
    }

    /// One simulated day. `reachable(node)` says whether the node is connected at the end of the
    /// day (its queue drains then); the hub filters and queues for it whether it is or not.
    ///
    /// # Errors
    /// The first invariant that does not hold, in words.
    pub fn step_day(
        &mut self,
        day: u32,
        reachable: &dyn Fn(usize) -> bool,
    ) -> Result<DayStats, String> {
        let mut stats = DayStats::default();
        if day >= self.schedule.start_day {
            self.hub_report = report(RELEASE_N_PLUS_1);
        }
        let hub_major: u32 = if day >= self.schedule.start_day { 2 } else { 1 };

        // Nodes upgrade first: the hub releases what each can now decode, in position order.
        for index in 0..self.nodes.len() {
            if self.schedule.upgrade_day[index] != Some(day) {
                continue;
            }
            let node = &mut self.nodes[index];
            node.report = report(RELEASE_N_PLUS_1);
            let released = release_for_upgrade(&node.report, &mut node.holds);
            let mut last: Option<u64> = None;
            for entry in &released {
                if last.is_some_and(|before| entry.position <= before) {
                    return Err(format!(
                        "node-{index}: the release after its upgrade is not in position order \
                         ({} after {last:?})",
                        entry.position
                    ));
                }
                last = Some(entry.position);
                if !node.report.can_decode(&entry.topic, entry.major) {
                    return Err(format!(
                        "node-{index}: released a record of {} major {} it cannot decode",
                        entry.topic, entry.major
                    ));
                }
                node.held.remove(&(
                    entry.record.entity.clone(),
                    entry.record.id.clone(),
                    entry.record.version,
                ));
            }
            if !node.held.is_empty() {
                return Err(format!(
                    "node-{index}: {} records are still held after an upgrade to a release that \
                     decodes every major",
                    node.held.len()
                ));
            }
            stats.upgraded += 1;
            stats.released += released.len();
            node.queue.extend(released);
        }

        // The day's entries, in position order, over a small set of keys.
        let mut entries: Vec<Outgoing> = Vec::with_capacity(self.per_day);
        for _ in 0..self.per_day {
            let key_index = self.rng.below(u64::try_from(self.keys).unwrap_or(1));
            let id = format!("k{key_index}");
            let version = self
                .versions
                .entry(("Soak".to_owned(), id.clone()))
                .and_modify(|v| *v += 1)
                .or_insert(1);
            self.next_position += 1;
            let bumped = self.rng.chance(500);
            entries.push(Outgoing {
                position: self.next_position,
                record: RecordRef {
                    entity: "Soak".to_owned(),
                    id,
                    version: *version,
                },
                topic: if bumped { BUMPED_TOPIC } else { STABLE_TOPIC }.to_owned(),
                major: if bumped { hub_major } else { 1 },
            });
            self.all_positions.insert(self.next_position);
        }

        // The hub decides per node whether to serve it and filters what it queues.
        for index in 0..self.nodes.len() {
            let decision = session_decision(&self.hub_report, &self.nodes[index].report, self.skew);
            let node = &mut self.nodes[index];
            node.refused_today = false;
            match decision {
                Decision::Run => {
                    let result = filter_outgoing(&entries, &node.report, &mut node.holds);
                    if !result.unstored.is_empty() {
                        return Err(format!(
                            "node-{index}: the hold set filled up ({} unstored); the default \
                             limit is far above what a soak writes",
                            result.unstored.len()
                        ));
                    }
                    for hold in &result.held {
                        node.held.insert((
                            hold.record.entity.clone(),
                            hold.record.id.clone(),
                            hold.record.version,
                        ));
                    }
                    stats.held += result.held.len();
                    for sent in &result.send {
                        if !node.report.can_decode(&sent.topic, sent.major) {
                            return Err(format!(
                                "node-{index} at release {:?} was sent {} major {} it cannot decode",
                                node.report.software_version, sent.topic, sent.major
                            ));
                        }
                        let earlier_held = node.held.iter().any(|(entity, id, version)| {
                            entity == &sent.record.entity
                                && id == &sent.record.id
                                && *version < sent.record.version
                        });
                        if earlier_held {
                            return Err(format!(
                                "node-{index} was sent {}/{} v{} while an earlier version of \
                                 that record is held",
                                sent.record.entity, sent.record.id, sent.record.version
                            ));
                        }
                    }
                    node.queue.extend(result.send);
                }
                Decision::Refuse {
                    reason_key,
                    retry: Retry::UpgradeChannelOnly,
                } => {
                    if reason_key != TOO_OLD_CODE {
                        return Err(format!(
                            "node-{index} was refused with {reason_key}, not {TOO_OLD_CODE}"
                        ));
                    }
                    node.upgrade_offers += 1;
                    node.refused_today = true;
                    if node.delivered_at_refusal.is_none() {
                        node.delivered_at_refusal = Some(node.delivered.len());
                    }
                    stats.offers += 1;
                }
                Decision::Refuse { reason_key, .. } => {
                    return Err(format!(
                        "node-{index} was told to wait ({reason_key}): the soak's nodes are all on \
                         a verified epoch and not migrating"
                    ));
                }
            }
        }

        // Connected nodes drain their queues in order.
        for index in 0..self.nodes.len() {
            let node = &mut self.nodes[index];
            if !reachable(index) || node.refused_today {
                continue;
            }
            while let Some(entry) = node.queue.pop_front() {
                if !node.report.can_decode(&entry.topic, entry.major) {
                    return Err(format!(
                        "node-{index} received {} major {} it cannot decode (release {:?})",
                        entry.topic, entry.major, node.report.software_version
                    ));
                }
                let key = key_of(&entry.record);
                let slot = node.last_version.entry(key).or_insert(0);
                if entry.record.version <= *slot {
                    return Err(format!(
                        "node-{index}: {}/{} v{} arrived after v{}",
                        entry.record.entity, entry.record.id, entry.record.version, slot
                    ));
                }
                *slot = entry.record.version;
                node.delivered.push(entry);
                stats.delivered += 1;
            }
        }

        // The too-old node is served nothing at all.
        if let Some(index) = self.schedule.too_old {
            let node = &self.nodes[index];
            if node
                .delivered_at_refusal
                .is_some_and(|frozen| node.delivered.len() != frozen)
            {
                return Err(format!(
                    "node-{index} was refused as too old yet records were delivered to it after"
                ));
            }
            if day >= self.schedule.start_day && node.upgrade_offers == 0 {
                return Err(format!(
                    "node-{index} is too old and was not offered the upgrade"
                ));
            }
        }
        Ok(stats)
    }

    /// The end of the run: every entry a served node is owed is either delivered or still held
    /// and comes out, in order, the moment it upgrades. Nothing was lost and nothing is twice.
    ///
    /// # Errors
    /// The first node whose account does not balance.
    pub fn finish(&self) -> Result<(), String> {
        for (index, node) in self.nodes.iter().enumerate() {
            if self.schedule.too_old == Some(index) {
                // Served before the hub moved, refused (the upgrade channel only) after: its
                // account is the frozen count, and it must have been offered the upgrade.
                if node
                    .delivered_at_refusal
                    .is_some_and(|frozen| node.delivered.len() != frozen)
                {
                    return Err(format!(
                        "node-{index} (too old) was served after the refusal"
                    ));
                }
                continue;
            }
            let mut holds = node.holds.clone();
            let released = release_for_upgrade(&report(RELEASE_N_PLUS_1), &mut holds);
            let mut seen: BTreeSet<u64> = BTreeSet::new();
            for position in node
                .delivered
                .iter()
                .chain(node.queue.iter())
                .map(|e| e.position)
                .chain(released.iter().map(|e| e.position))
            {
                if !seen.insert(position) {
                    return Err(format!(
                        "node-{index}: position {position} is delivered or held twice"
                    ));
                }
            }
            if seen != self.all_positions {
                return Err(format!(
                    "node-{index}: {} of {} entries are neither delivered nor held",
                    self.all_positions.len() - seen.len(),
                    self.all_positions.len()
                ));
            }
        }
        Ok(())
    }

    /// Held entries at the end, per node, for the trace.
    #[must_use]
    pub fn held_total(&self) -> usize {
        self.nodes.iter().map(|n| n.held.len()).sum()
    }

    /// Upgrade offers made to the too-old node.
    #[must_use]
    pub fn upgrade_offers(&self) -> u64 {
        self.nodes.iter().map(|n| n.upgrade_offers).sum()
    }
}
