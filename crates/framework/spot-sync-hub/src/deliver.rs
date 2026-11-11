//! Routing one source transaction to one destination.

use crate::error::HubError;
use crate::model::{DestReport, EntryRole, PushOutcome, QueuedBundle, RelayConfig, StreamEntry};
use crate::relay::Destination;
use crate::traits::SendQueue;
use spot_sync_closure::{
    BundlePlan, DestinationKnows, ExtrasSet, ImageSource, Position, RecordKey, References,
    SourceImage, build,
};
use spot_sync_obligations::{Lane, Pending, lane_order};
use spot_sync_skew::{HoldSet, Outgoing, RecordRef, filter_outgoing};
use std::collections::{BTreeMap, BTreeSet};

/// Everything a destination needs the relay to remember.
pub(crate) struct DestState {
    pub dest: Destination,
    pub holds: HoldSet,
    pub held_entries: BTreeMap<u64, StreamEntry>,
    /// Entries withheld because a record their closure needs is held ("dependency held"), by
    /// position: the entry itself is in `held_entries`, the set here is the held records that
    /// block it. Released with the dependency in `on_node_upgraded`.
    pub held_by_dependency: BTreeMap<u64, BTreeSet<RecordKey>>,
    pub release_backlog: BTreeMap<u64, StreamEntry>,
    pub extras: ExtrasSet,
    pub acked: Option<u64>,
    pub catchup_from: Option<u64>,
    pub backpressured: bool,
}

impl DestState {
    pub(crate) fn new(dest: Destination, holds: HoldSet) -> Self {
        Self {
            dest,
            holds,
            held_entries: BTreeMap::new(),
            held_by_dependency: BTreeMap::new(),
            release_backlog: BTreeMap::new(),
            extras: ExtrasSet::new(),
            acked: None,
            catchup_from: None,
            backpressured: false,
        }
    }
}

/// The records this destination must not be sent yet: the entries a version-skew hold keeps back
/// and the dependents withheld behind them. The closure never pulls one of them in as an extra.
fn held_keys(st: &DestState) -> BTreeSet<RecordKey> {
    let mut keys: BTreeSet<RecordKey> = st.held_entries.values().map(|e| e.key.clone()).collect();
    // The hub-side hold queue is the authority; the held entries are what it kept. Read both, so
    // the two can never drift apart unnoticed.
    keys.extend(
        st.holds
            .pending_holds()
            .iter()
            .map(|h| RecordKey::new(h.record.entity.clone(), h.record.id.clone())),
    );
    keys
}

/// The relay's collaborators, borrowed for one pass.
pub(crate) struct Ctx<'a, Q, I> {
    pub cfg: &'a RelayConfig,
    pub queue: &'a mut Q,
    pub images: &'a I,
    pub refs: &'a dyn References,
}

struct Knows<'a> {
    dest: &'a Destination,
    acked: Option<Position>,
    extras: &'a ExtrasSet,
    held: BTreeSet<RecordKey>,
}

impl DestinationKnows for Knows<'_> {
    fn owns(&self, key: &RecordKey) -> bool {
        self.dest.scope.owns(key)
    }
    fn in_scope(&self, key: &RecordKey) -> bool {
        self.dest.scope.covers_key(key)
    }
    fn acked_position(&self) -> Option<Position> {
        self.acked
    }
    fn extras(&self) -> &ExtrasSet {
        self.extras
    }
    fn held(&self, key: &RecordKey) -> bool {
        self.held.contains(key)
    }
}

/// Group entries by source transaction, groups in order of their first position.
pub(crate) fn group_by_txn(entries: &[StreamEntry]) -> Vec<Vec<StreamEntry>> {
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let mut groups: Vec<Vec<StreamEntry>> = Vec::new();
    for e in entries {
        match index.get(e.txn.as_str()) {
            Some(i) => groups[*i].push(e.clone()),
            None => {
                index.insert(e.txn.as_str(), groups.len());
                groups.push(vec![e.clone()]);
            }
        }
    }
    groups
}

enum Delivered {
    Done,
    Behind { backpressure: bool },
}

/// Run `groups` for one destination. Returns the index of the first group that could not be
/// delivered (the destination is then behind from it), or `None` when all were handled.
pub(crate) fn run_groups<Q: SendQueue, I: ImageSource>(
    ctx: &mut Ctx<'_, Q, I>,
    st: &mut DestState,
    groups: &[Vec<StreamEntry>],
    used: &mut usize,
    rep: &mut DestReport,
) -> Option<usize> {
    for (i, group) in groups.iter().enumerate() {
        match deliver(ctx, st, group, used, rep) {
            Ok(Delivered::Done) => {}
            Ok(Delivered::Behind { backpressure }) => {
                st.backpressured = backpressure;
                return Some(i);
            }
            Err(e) => {
                rep.failed = Some(e.code());
                return Some(i);
            }
        }
    }
    None
}

fn matched<'a>(
    ctx_cfg: &RelayConfig,
    st: &DestState,
    group: &'a [StreamEntry],
) -> Vec<&'a StreamEntry> {
    group
        .iter()
        .filter(|e| e.tenant == ctx_cfg.tenant && e.source == ctx_cfg.source)
        .filter(|e| e.origin.as_deref() != Some(st.dest.id.as_str()))
        .filter(|e| st.dest.scope.covers(e) || st.extras.routed_later_versions(&e.key))
        .collect()
}

/// A replayed entry that a later held version of the same record already overtakes was sent
/// before: queueing it behind that hold would put an old version after a newer one.
fn overtaken_by_hold(st: &DestState, e: &StreamEntry) -> bool {
    !st.held_entries.contains_key(&e.position)
        && st
            .held_entries
            .values()
            .any(|h| h.key == e.key && h.position > e.position)
}

fn deliver<Q: SendQueue, I: ImageSource>(
    ctx: &mut Ctx<'_, Q, I>,
    st: &mut DestState,
    group: &[StreamEntry],
    used: &mut usize,
    rep: &mut DestReport,
) -> Result<Delivered, HubError> {
    let scoped: Vec<&StreamEntry> = matched(ctx.cfg, st, group)
        .into_iter()
        .filter(|e| !overtaken_by_hold(st, e))
        .collect();
    if scoped.is_empty() {
        return Ok(Delivered::Done);
    }
    if ctx.queue.queued_bytes(&st.dest.id)? >= ctx.cfg.max_queue_bytes {
        return Ok(Delivered::Behind { backpressure: true });
    }
    st.backpressured = false;

    let outgoing: Vec<Outgoing> = scoped
        .iter()
        .map(|e| Outgoing {
            position: e.position,
            record: RecordRef {
                entity: e.key.entity.clone(),
                id: e.key.id.clone(),
                version: e.version,
            },
            topic: e.topic.clone(),
            major: e.major,
        })
        .collect();
    let verdict = filter_outgoing(&outgoing, &st.dest.report, &mut st.holds);
    let send: BTreeSet<u64> = verdict.send.iter().map(|o| o.position).collect();
    let unstored: BTreeSet<u64> = verdict.unstored.iter().map(|o| o.position).collect();
    for e in &scoped {
        if !send.contains(&e.position) && !unstored.contains(&e.position) {
            st.held_entries.insert(e.position, (*e).clone());
        }
    }
    rep.held += scoped
        .iter()
        .filter(|e| !send.contains(&e.position) && !unstored.contains(&e.position))
        .count();
    rep.unstored += unstored.len();

    let acked = st.acked;
    let sent: Vec<&StreamEntry> = scoped
        .into_iter()
        .filter(|e| send.contains(&e.position) && acked.is_none_or(|a| e.position > a))
        .collect();
    if sent.is_empty() {
        return Ok(Delivered::Done);
    }
    let plan = plan_for(ctx, st, &sent)?;
    if *used > 0 && *used + plan.bundles.len() > ctx.cfg.max_bundles_per_tick_per_dest {
        return Ok(Delivered::Behind {
            backpressure: false,
        });
    }
    rep.held_by_dependency += withhold_dependents(st, &sent, &plan.withheld_by_hold);
    let (queued, dup) = push_plan(ctx, st, &sent, plan, false)?;
    *used += queued + dup;
    rep.bundles_queued += queued;
    rep.bundles_duplicate += dup;
    Ok(Delivered::Done)
}

/// Store the entries the closure withheld because a record they need is held like a held entry
/// (reason "dependency held"), with the held records that block each one, so `on_node_upgraded`
/// releases them with their dependency. Returns how many dependents were stored.
fn withhold_dependents(
    st: &mut DestState,
    sent: &[&StreamEntry],
    withheld: &[(RecordKey, RecordKey)],
) -> usize {
    let mut count = 0;
    let mut counted: BTreeSet<&RecordKey> = BTreeSet::new();
    for (dependent, blocker) in withheld {
        // The newest entry of the record in this group is the one that waits.
        let Some(entry) = sent
            .iter()
            .filter(|e| &e.key == dependent)
            .max_by_key(|e| e.position)
        else {
            continue;
        };
        let position = entry.position;
        st.held_entries
            .entry(position)
            .or_insert_with(|| (*entry).clone());
        st.held_by_dependency
            .entry(position)
            .or_default()
            .insert(blocker.clone());
        if counted.insert(dependent) {
            count += 1;
        }
    }
    count
}

fn image_of<I: ImageSource>(images: &I, e: &StreamEntry) -> Result<SourceImage, HubError> {
    images.image(&e.key)?.ok_or_else(|| HubError::ImageMissing {
        entity: e.key.entity.clone(),
        id: e.key.id.clone(),
    })
}

fn plan_for<Q: SendQueue, I: ImageSource>(
    ctx: &Ctx<'_, Q, I>,
    st: &DestState,
    sent: &[&StreamEntry],
) -> Result<BundlePlan, HubError> {
    let mut roots = Vec::new();
    let mut satellites = Vec::new();
    let mut seen: BTreeSet<&RecordKey> = BTreeSet::new();
    for e in sent {
        // Full-state events: one image per record, even when a transaction touches it twice.
        if !seen.insert(&e.key) {
            continue;
        }
        let image = image_of(ctx.images, e)?;
        match e.role {
            EntryRole::Root => roots.push(image),
            EntryRole::Satellite => satellites.push(image),
        }
    }
    let knows = Knows {
        dest: &st.dest,
        acked: st.acked,
        extras: &st.extras,
        held: held_keys(st),
    };
    build(
        &roots,
        &satellites,
        &knows,
        ctx.images,
        ctx.refs,
        st.dest.redactor.as_ref(),
        ctx.cfg.limits,
    )
    .map_err(HubError::from)
}

/// Push a plan; returns `(queued, duplicates)`. Out-of-scope extras are recorded in the
/// destination's extras set only after every push succeeded.
fn push_plan<Q: SendQueue, I: ImageSource>(
    ctx: &mut Ctx<'_, Q, I>,
    st: &mut DestState,
    sent: &[&StreamEntry],
    plan: BundlePlan,
    redelivery: bool,
) -> Result<(usize, usize), HubError> {
    let position = sent.iter().map(|e| e.position).max().unwrap_or(0);
    let lane = sent.iter().map(|e| e.lane).min().unwrap_or(Lane::Bulk);
    let mut items: Vec<Pending<QueuedBundle>> = plan
        .bundles
        .into_iter()
        .enumerate()
        .map(|(i, bundle)| Pending {
            lane,
            position,
            item: QueuedBundle {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                redelivery,
                bundle,
            },
        })
        .collect();
    items = lane_order(items);
    let (mut queued, mut dup) = (0, 0);
    for item in items {
        match ctx.queue.push(&st.dest.id, item)? {
            PushOutcome::Queued => queued += 1,
            PushOutcome::Duplicate => dup += 1,
        }
    }
    for key in plan.out_of_scope_extras {
        st.extras.record_delivered_extra(key);
    }
    Ok((queued, dup))
}

/// Requeue released holds, one source transaction at a time, as redeliveries. A failure leaves
/// the rest in the backlog for the next pass.
pub(crate) fn flush_releases<Q: SendQueue, I: ImageSource>(
    ctx: &mut Ctx<'_, Q, I>,
    st: &mut DestState,
    used: &mut usize,
    rep: &mut DestReport,
) {
    if st.release_backlog.is_empty() {
        return;
    }
    let entries: Vec<StreamEntry> = st.release_backlog.values().cloned().collect();
    for group in group_by_txn(&entries) {
        // The entitlement is asked again: a scope that shrank while an entry was held must not
        // let the held record out afterwards (scope filtering is the access control of sync).
        let refs: Vec<&StreamEntry> = matched(ctx.cfg, st, &group);
        if refs.is_empty() {
            for e in &group {
                st.release_backlog.remove(&e.position);
            }
            continue;
        }
        let result = plan_for(ctx, st, &refs).and_then(|plan| {
            // A dependent that is still blocked goes back to the held entries instead of being
            // dropped with the group: its dependency has not been released yet.
            withhold_dependents(st, &refs, &plan.withheld_by_hold);
            push_plan(ctx, st, &refs, plan, true)
        });
        match result {
            Ok((queued, dup)) => {
                *used += queued + dup;
                rep.bundles_queued += queued;
                rep.bundles_duplicate += dup;
                for e in &group {
                    st.release_backlog.remove(&e.position);
                }
            }
            Err(e) => {
                rep.failed = Some(e.code());
                return;
            }
        }
    }
}
