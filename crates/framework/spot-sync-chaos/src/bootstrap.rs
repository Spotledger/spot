//! The restored-node and many-nodes-return paths: `decide_start`, the snapshot session and
//! admission control (ADR-066 sections 4 and 7).

use std::collections::{BTreeMap, VecDeque};

use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{
    Admission, AdmissionSettings, Bootstrap, Budget, Chunk, Decision, HubHead, MemorySource,
    NodeState, Seams, StartMode, StartOptions, decide_start,
};
use spot_sync_closure::{Limits, NoRedaction};

use crate::catalog::references;
use crate::model::ChaosError;
use crate::world::World;

/// What starting a session for one node did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartReport {
    /// The hub's answer to the handshake.
    pub mode: StartMode,
    /// Whether the node was bootstrapped from a snapshot.
    pub bootstrapped: bool,
    /// Chunks the snapshot session served.
    pub chunks: usize,
    /// Records the node installed from them.
    pub installed: usize,
}

/// What a many-nodes-return run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScaleReport {
    /// Virtual milliseconds from the first request to the last completion.
    pub elapsed_ms: u64,
    /// The most bootstraps running at once.
    pub max_running: usize,
    /// Nodes that completed.
    pub completed: usize,
    /// Times a node was told to come back later.
    pub retries: usize,
    /// Times a chunk was paced by the token bucket.
    pub paced: usize,
    /// The order nodes were admitted in.
    pub admitted: Vec<usize>,
}

/// A bootstrap in flight: the chunks still to be applied.
struct Job {
    node: usize,
    chunks: VecDeque<Chunk>,
    next_ms: i64,
}

impl World {
    /// What a node reports about itself at the handshake.
    #[must_use]
    pub fn node_state(&self, node: usize) -> NodeState {
        NodeState {
            node_id: self.node_id(node).to_owned(),
            tenant: self.tenant().to_owned(),
            generation: self.node_generation(node),
            applied_position: self.node_reported_position(node),
        }
    }

    /// The hub's stream as it is now.
    #[must_use]
    pub fn hub_head(&self) -> HubHead {
        HubHead {
            generation: self.generation(),
            head_position: self.head(),
        }
    }

    /// The positions of the snapshots the hub holds: one, at its head.
    #[must_use]
    pub fn snapshot_positions(&self) -> Vec<u64> {
        vec![self.head()]
    }

    /// The hub's answer to `state`, for a state the test crafts (a position past the head, a
    /// generation from the future).
    #[must_use]
    pub fn decide(&self, state: &NodeState) -> StartMode {
        decide_start(state, self.hub_head(), &self.snapshot_positions())
    }

    /// Wipe a node: a new or replaced machine with nothing.
    ///
    /// # Errors
    /// `no node` for an index the world does not have.
    pub fn wipe_node(&mut self, node: usize) -> Result<(), ChaosError> {
        self.reset_node(node, self.generation(), None, None)
    }

    /// Start a session for one node: the hub decides, and a bootstrap runs when it must.
    ///
    /// # Errors
    /// A refusal from the snapshot session or an unknown node.
    pub fn start_session(&mut self, node: usize) -> Result<StartReport, ChaosError> {
        let state = self.node_state(node);
        let mode = self.decide(&state);
        let StartMode::Bootstrap {
            snapshot_position, ..
        } = mode
        else {
            return Ok(StartReport {
                mode,
                bootstrapped: false,
                chunks: 0,
                installed: 0,
            });
        };
        let chunks = self.snapshot_chunks(node, snapshot_position)?;
        let count = chunks.len();
        let mut installed = 0;
        for chunk in &chunks {
            installed += self.install_chunk(node, chunk)?;
        }
        self.note_bootstrapped(node)?;
        Ok(StartReport {
            mode,
            bootstrapped: true,
            chunks: count,
            installed,
        })
    }

    /// Bring `nodes` back at once through admission control. Every node is wiped, asks to
    /// bootstrap at a seeded offset, and is told to come back later when the hub is full.
    ///
    /// # Errors
    /// A refusal from the snapshot session or the admission settings.
    pub fn return_all(
        &mut self,
        nodes: &[usize],
        settings: AdmissionSettings,
    ) -> Result<ScaleReport, ChaosError> {
        let seed = self.seed();
        let mut admission = Admission::new(settings)
            .map_err(|error| ChaosError::new(seed, format!("admission: {error}")))?;
        let started = self.clock().elapsed_ms();
        let position = self.head();
        // Every node is wiped; each asks at its own seeded offset into the first second.
        let mut waiting: BTreeMap<usize, i64> = BTreeMap::new();
        for &node in nodes {
            self.wipe_node(node)?;
            let offset = i64::try_from(self.draw_below(1_000)).unwrap_or(0);
            waiting.insert(node, self.clock().hub_wall_ms().saturating_add(offset));
        }
        let mut jobs: Vec<Job> = Vec::new();
        let mut report = ScaleReport::default();
        let mut guard = 0_u64;
        while !waiting.is_empty() || !jobs.is_empty() {
            guard += 1;
            if guard > 10_000_000 {
                return Err(ChaosError::new(seed, "the nodes did not all return"));
            }
            let now = self.clock().hub_wall_ms();
            // Nodes whose retry time has come ask again, in node order.
            let ready: Vec<usize> = waiting
                .iter()
                .filter(|(_, at)| **at <= now)
                .map(|(node, _)| *node)
                .collect();
            for node in ready {
                match admission.admit(self.node_id(node), now) {
                    Decision::Admit => {
                        waiting.remove(&node);
                        report.admitted.push(node);
                        let chunks = self.snapshot_chunks(node, position)?;
                        jobs.push(Job {
                            node,
                            chunks: chunks.into(),
                            next_ms: now,
                        });
                    }
                    Decision::RetryAfter(ms) => {
                        report.retries += 1;
                        waiting.insert(node, now.saturating_add(i64::try_from(ms).unwrap_or(0)));
                    }
                }
                report.max_running = report.max_running.max(admission.running());
            }
            // Admitted nodes take a chunk when their token bucket allows.
            let mut finished: Vec<usize> = Vec::new();
            for (index, job) in jobs.iter_mut().enumerate() {
                if job.next_ms > now {
                    continue;
                }
                let id = self.node_id(job.node).to_owned();
                match admission
                    .chunk(&id, now)
                    .map_err(|error| ChaosError::new(seed, format!("chunk {id}: {error}")))?
                {
                    Decision::Admit => {
                        if let Some(chunk) = job.chunks.pop_front() {
                            self.install_chunk(job.node, &chunk)?;
                        }
                        if job.chunks.is_empty() {
                            finished.push(index);
                        }
                    }
                    Decision::RetryAfter(ms) => {
                        report.paced += 1;
                        job.next_ms = now.saturating_add(i64::try_from(ms).unwrap_or(1));
                    }
                }
            }
            for index in finished.into_iter().rev() {
                let job = jobs.remove(index);
                admission.release(self.node_id(job.node));
                self.note_bootstrapped(job.node)?;
                report.completed += 1;
            }
            // Jump to the next moment anything can happen.
            let next = waiting
                .values()
                .copied()
                .chain(jobs.iter().map(|job| job.next_ms))
                .min();
            match next {
                Some(at) if at > now => {
                    self.clock_mut()
                        .advance_by(u64::try_from(at.saturating_sub(now)).unwrap_or(1));
                }
                Some(_) => self.clock_mut().advance_by(1),
                None => {}
            }
        }
        report.elapsed_ms = self.clock().elapsed_ms().saturating_sub(started);
        Ok(report)
    }

    /// Plan the snapshot at `position` for `node` and serve it to the end.
    fn snapshot_chunks(&self, node: usize, position: u64) -> Result<Vec<Chunk>, ChaosError> {
        let seed = self.seed();
        let records: Vec<ImageRecord> = self
            .images_at(position)
            .into_iter()
            .map(|(key, image)| ImageRecord {
                entity: key.entity,
                id: key.id,
                version: image.position,
                entity_schema: 1,
                image: image.image,
            })
            .collect();
        let info = SnapshotInfo {
            position,
            generation: self.generation(),
            verified: true,
            taken_at_millis: self.clock().hub_wall_ms(),
        };
        let source = MemorySource::new(self.tenant(), info, records);
        let scope = self
            .scope(node)
            .ok_or_else(|| ChaosError::new(seed, format!("no node {node}")))?;
        let refs = references();
        let redactor = NoRedaction;
        let seams = Seams {
            scope,
            references: &refs,
            redactor: &redactor,
        };
        let state = NodeState {
            node_id: self.node_id(node).to_owned(),
            tenant: self.tenant().to_owned(),
            generation: self.generation(),
            applied_position: None,
        };
        let mut session = Bootstrap::start(
            &source,
            seams,
            Limits::default(),
            &state,
            self.hub_head(),
            StartOptions::default(),
        )
        .map_err(|error| ChaosError::new(seed, format!("bootstrap start: {error}")))?;
        let budget = Budget {
            max_bytes: 4_096,
            max_records: 1,
        };
        let mut chunks = Vec::new();
        while let Some(chunk) = session
            .next_chunk(budget)
            .map_err(|error| ChaosError::new(seed, format!("bootstrap chunk: {error}")))?
        {
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}
