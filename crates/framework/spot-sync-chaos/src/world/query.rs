//! Read-only views of a world: settings, clock, stores, queues and the hub's expected state.

use std::collections::BTreeMap;

use spot_sync_closure::RecordKey;
use spot_sync_hub::SendQueue;

use super::{
    AckRecord, Clock, CutHit, EntityScope, MemStore, NackRecord, Step, World, WorldConfig,
};

impl World {
    /// The settings.
    #[must_use]
    pub fn config(&self) -> &WorldConfig {
        &self.cfg
    }

    /// The seed.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.cfg.seed
    }

    /// The tenant the hub serves.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.cfg.tenant
    }

    /// How many nodes pull.
    #[must_use]
    pub fn nodes(&self) -> usize {
        self.nodes.len()
    }

    /// A node's id.
    #[must_use]
    pub fn node_id(&self, node: usize) -> &str {
        self.nodes.get(node).map_or("", |n| n.id.as_str())
    }

    /// A node's scope.
    #[must_use]
    pub fn scope(&self, node: usize) -> Option<&EntityScope> {
        self.nodes.get(node).map(|n| &n.scope)
    }

    /// A node's durable store.
    #[must_use]
    pub fn store(&self, node: usize) -> Option<&MemStore> {
        self.nodes.get(node).map(|n| n.intake.applier())
    }

    /// The clock.
    #[must_use]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The clock, for a test that moves it.
    pub fn clock_mut(&mut self) -> &mut Clock {
        &mut self.clock
    }

    /// The highest stream position written.
    #[must_use]
    pub fn head(&self) -> u64 {
        self.entries.last().map_or(0, |entry| entry.position)
    }

    /// The relay's read cursor.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.hub.cursor()
    }

    /// The stream generation the hub is on.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.cfg.generation
    }

    /// How many rounds have run.
    #[must_use]
    pub fn rounds(&self) -> usize {
        self.round
    }

    /// The acknowledgements the nodes sent.
    #[must_use]
    pub fn acks(&self) -> &[AckRecord] {
        &self.acks
    }

    /// The refusals the nodes reported.
    #[must_use]
    pub fn nacks(&self) -> &[NackRecord] {
        &self.nacks
    }

    /// The cuts that fired.
    #[must_use]
    pub fn cut_hits(&self) -> &[CutHit] {
        &self.hits
    }

    /// Whether a cut fired at `step` on `node`, on either edge.
    #[must_use]
    pub fn cut_fired(&self, node: usize, step: Step) -> bool {
        self.hits
            .iter()
            .any(|hit| hit.node == node && hit.step == step)
    }

    /// The transcript: one line per thing that happened, in order.
    #[must_use]
    pub fn transcript(&self) -> &[String] {
        &self.transcript
    }

    /// The transcript as text. Two runs of one configuration produce the same bytes.
    #[must_use]
    pub fn transcript_text(&self) -> String {
        self.transcript.join("\n")
    }

    /// How many bundles are queued for a node.
    #[must_use]
    pub fn queued(&self, node: usize) -> usize {
        let id = self.node_id(node);
        self.hub
            .queue()
            .peek(id, usize::MAX)
            .map(|items| items.len())
            .unwrap_or(0)
    }

    /// The records a node holds for the hub's tenant.
    #[must_use]
    pub fn applied(&self, node: usize) -> BTreeMap<RecordKey, u64> {
        let tenant = self.cfg.tenant.clone();
        self.nodes
            .get(node)
            .map(|n| n.intake.applier().records_of(&tenant))
            .unwrap_or_default()
    }

    /// The extras the relay recorded for a node (records delivered outside its scope because a
    /// closure needed them). There are none in this world: the records declare no links.
    #[must_use]
    pub fn extras(&self, node: usize) -> Vec<RecordKey> {
        let id = self.node_id(node);
        self.hub
            .extras(id)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The tenants the stream carries.
    #[must_use]
    pub fn tenants(&self) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        for entry in &self.entries {
            if !found.contains(&entry.tenant) {
                found.push(entry.tenant.clone());
            }
        }
        found
    }

    /// The latest version of every record the hub holds for its tenant.
    #[must_use]
    pub fn expected(&self) -> BTreeMap<RecordKey, u64> {
        let mut expected: BTreeMap<RecordKey, u64> = BTreeMap::new();
        for entry in &self.entries {
            if entry.tenant != self.cfg.tenant {
                continue;
            }
            expected
                .entry(entry.key.clone())
                .and_modify(|version| *version = (*version).max(entry.version))
                .or_insert(entry.version);
        }
        expected
    }

    /// How many bytes the hub's stream holds at `position` — the snapshot a bootstrap reads.
    #[must_use]
    pub fn images_at(&self, position: u64) -> BTreeMap<RecordKey, spot_sync_closure::SourceImage> {
        self.images.at_or_below(position)
    }
}
