//! The simulator's event handlers: production, chaos, handshakes, session steps, delivery and
//! the convergence checks. Split from `engine` so neither file is oversized; the handlers see the
//! parent module's private state through the child-module privacy rule.

use super::*;
use crate::ids::{CURRENT_PROTOCOL, ProtocolRange};
use crate::message::{Action, Offer, Outcome};

impl Sim {
    pub(super) fn dispatch(&mut self, kind: EvKind) {
        match kind {
            EvKind::Produce => self.on_produce(),
            EvKind::Chaos => self.on_chaos(),
            EvKind::Handshake { edge, generation } => self.on_handshake(edge, generation),
            EvKind::PullStep {
                edge,
                source,
                generation,
            } => self.on_pull_step(edge, source, generation),
            EvKind::PushStep {
                edge,
                source,
                generation,
            } => self.on_push_step(edge, source, generation),
            EvKind::Deliver { to, msg } => self.on_deliver(to, msg),
        }
    }

    pub(super) fn on_produce(&mut self) {
        if self.produced >= self.total {
            return;
        }
        let (source_index, record) = self.plan[self.produced].clone();
        self.produced += 1;
        match self.config.direction {
            Direction::Pull => self.hub_streams[source_index].push(record),
            Direction::Push => {
                let edge = source_index / self.config.sources;
                let local = source_index % self.config.sources;
                self.edges[edge].outbox[local].push(record);
            }
        }
        if self.produced < self.total {
            self.schedule(
                self.clock + self.config.produce_every.max(1),
                EvKind::Produce,
            );
        }
    }

    pub(super) fn on_chaos(&mut self) {
        if self.produced >= self.total {
            return;
        }
        if self.rng.chance(self.config.network.partition_permille) {
            let edge = self.rng.below(self.config.edges as u64) as usize;
            let until = self.clock + self.config.network.partition_ticks;
            if until > self.partitioned_until[edge] {
                self.partitioned_until[edge] = until;
            }
        }
        let every = self.config.network.chaos_every.max(1);
        self.schedule(self.clock + every, EvKind::Chaos);
    }

    pub(super) fn on_handshake(&mut self, edge: usize, generation: u64) {
        if self.edges[edge].handshake_gen != generation || self.edges[edge].negotiated {
            return;
        }
        if self.crash(Endpoint::Edge(edge)) {
            return;
        }
        let awaiting = self.edges[edge].handshake_awaiting;
        match awaiting {
            Some(deadline) if !deadline.is_expired(self.now()) => {
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::Handshake { edge, generation },
                );
            }
            _ => {
                let hello = Hello::new(
                    self.edges[edge].node.clone(),
                    ProtocolRange::current(),
                    self.hub_hello.metadata_epoch,
                );
                self.transmit(
                    Endpoint::Edge(edge),
                    Endpoint::Hub,
                    Msg::Hello {
                        from: Endpoint::Edge(edge),
                        hello,
                    },
                );
                let deadline = self.retry_deadline();
                self.edges[edge].handshake_awaiting = Some(deadline);
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::Handshake { edge, generation },
                );
            }
        }
    }

    pub(super) fn on_pull_step(&mut self, edge: usize, source: usize, generation: u64) {
        if self.config.direction != Direction::Pull
            || self.edges[edge].pull_gen[source] != generation
        {
            return;
        }
        if !self.edges[edge].negotiated {
            return;
        }
        if self.crash(Endpoint::Edge(edge)) {
            return;
        }
        let source_id = self.all_sources[source].clone();
        let from_seq = self.edges[edge]
            .receiver
            .stored_checkpoint(&source_id)
            .map_or(1, |checkpoint| checkpoint.next_expected());
        let awaiting = self.edges[edge].pull_awaiting[source];
        match awaiting {
            Some(deadline) if !deadline.is_expired(self.now()) => {
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::PullStep {
                        edge,
                        source,
                        generation,
                    },
                );
            }
            _ => {
                self.transmit(
                    Endpoint::Edge(edge),
                    Endpoint::Hub,
                    Msg::Pull {
                        from: Endpoint::Edge(edge),
                        source,
                        from_seq,
                    },
                );
                let deadline = self.retry_deadline();
                self.edges[edge].pull_awaiting[source] = Some(deadline);
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::PullStep {
                        edge,
                        source,
                        generation,
                    },
                );
            }
        }
    }

    pub(super) fn on_push_step(&mut self, edge: usize, source: usize, generation: u64) {
        if self.config.direction != Direction::Push
            || self.edges[edge].push_gen[source] != generation
        {
            return;
        }
        if !self.edges[edge].negotiated {
            return;
        }
        if self.crash(Endpoint::Edge(edge)) {
            return;
        }
        let local = source - self.edges[edge].base;
        let source_id = self.all_sources[source].clone();
        let pending = self.edges[edge].push_awaiting[source].take();
        match pending {
            Some((batch, deadline)) if !deadline.is_expired(self.now()) => {
                self.edges[edge].push_awaiting[source] = Some((batch, deadline));
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::PushStep {
                        edge,
                        source,
                        generation,
                    },
                );
            }
            Some((batch, _)) => {
                self.transmit(
                    Endpoint::Edge(edge),
                    Endpoint::Hub,
                    Msg::Push {
                        edge,
                        source,
                        batch: batch.clone(),
                    },
                );
                let deadline = self.retry_deadline();
                self.edges[edge].push_awaiting[source] = Some((batch, deadline));
                self.schedule(
                    Self::ticks_until(deadline.at()),
                    EvKind::PushStep {
                        edge,
                        source,
                        generation,
                    },
                );
            }
            None => {
                let records = self.edges[edge].outbox[local].clone();
                let offer = Offer::new(source_id.clone(), records);
                match self.edges[edge].sender.next(&offer) {
                    Action::Send(batch) => {
                        self.transmit(
                            Endpoint::Edge(edge),
                            Endpoint::Hub,
                            Msg::Push {
                                edge,
                                source,
                                batch: batch.clone(),
                            },
                        );
                        let deadline = self.retry_deadline();
                        self.edges[edge].push_awaiting[source] = Some((batch, deadline));
                        self.schedule(
                            Self::ticks_until(deadline.at()),
                            EvKind::PushStep {
                                edge,
                                source,
                                generation,
                            },
                        );
                    }
                    Action::Wait => {
                        // Nothing to send right now: either every produced record is already in
                        // flight, or the window is full. Production continues, so keep polling;
                        // the run ends when the hub has applied everything.
                        let at = self.clock + self.config.poll_every.max(1);
                        self.schedule(
                            at,
                            EvKind::PushStep {
                                edge,
                                source,
                                generation,
                            },
                        );
                    }
                }
            }
        }
    }

    pub(super) fn on_deliver(&mut self, to: Endpoint, msg: Msg) {
        match (to, msg) {
            (Endpoint::Hub, Msg::Hello { from, hello }) => match self.hub_hello.negotiate(&hello) {
                Ok(version) => self.transmit(Endpoint::Hub, from, Msg::HelloReply { version }),
                Err(_) => self.transmit(Endpoint::Hub, from, Msg::HelloNack),
            },
            (Endpoint::Edge(edge), Msg::HelloReply { version }) => {
                if self.crash(Endpoint::Edge(edge)) {
                    return;
                }
                if version == CURRENT_PROTOCOL {
                    self.edges[edge].negotiated = true;
                    self.edges[edge].handshake_awaiting = None;
                    self.schedule_steps(edge);
                }
            }
            (Endpoint::Edge(_), Msg::HelloNack) => {
                // No common version: this edge never syncs. A real caller raises the condition;
                // here the run simply fails to converge and reports its seed.
            }
            (Endpoint::Edge(edge), Msg::Batch { source, batch }) => {
                if self.config.direction != Direction::Pull {
                    return;
                }
                if self.crash(Endpoint::Edge(edge)) {
                    return;
                }
                self.edges[edge].pull_awaiting[source] = None;
                match self.edges[edge].receiver.on_batch(&batch) {
                    Outcome::Ack(ack) => {
                        self.transmit(Endpoint::Edge(edge), Endpoint::Hub, Msg::Ack { edge, ack });
                        self.schedule_pull(edge, source, self.clock + 1);
                    }
                    Outcome::Nack(nack) => {
                        self.transmit(
                            Endpoint::Edge(edge),
                            Endpoint::Hub,
                            Msg::Nack { edge, nack },
                        );
                        self.schedule_pull(edge, source, self.clock + 1);
                    }
                    Outcome::Failed(_) => {
                        self.schedule_pull(edge, source, self.clock + 1);
                    }
                }
            }
            (Endpoint::Edge(edge), Msg::Empty { source }) => {
                if self.config.direction != Direction::Pull {
                    return;
                }
                if self.crash(Endpoint::Edge(edge)) {
                    return;
                }
                self.edges[edge].pull_awaiting[source] = None;
                self.schedule_pull(edge, source, self.clock + self.config.poll_every.max(1));
            }
            (
                Endpoint::Hub,
                Msg::Pull {
                    from,
                    source,
                    from_seq,
                },
            ) => {
                if self.config.direction != Direction::Pull {
                    return;
                }
                if self.crash(Endpoint::Hub) {
                    // The hub's volatile cursor reset is harmless: the next request carries the
                    // receiver's checkpoint again.
                }
                let Endpoint::Edge(edge) = from else {
                    return;
                };
                let source_id = self.all_sources[source].clone();
                let cap = self.config.max_batch;
                let records: Vec<Record> = self.hub_streams[source]
                    .iter()
                    .filter(|record| record.seq >= from_seq)
                    .take(cap)
                    .cloned()
                    .collect();
                let offer = Offer::new(source_id.clone(), records);
                self.hub_senders[edge].resume(&source_id, from_seq);
                match self.hub_senders[edge].next(&offer) {
                    Action::Send(batch) => {
                        self.transmit(
                            Endpoint::Hub,
                            Endpoint::Edge(edge),
                            Msg::Batch { source, batch },
                        );
                    }
                    Action::Wait => {
                        self.transmit(Endpoint::Hub, Endpoint::Edge(edge), Msg::Empty { source });
                    }
                }
            }
            (Endpoint::Hub, Msg::Ack { edge, ack }) => {
                if self.config.direction != Direction::Pull {
                    return;
                }
                self.hub_senders[edge].on_ack(&ack);
            }
            (Endpoint::Hub, Msg::Nack { edge, nack }) => {
                if self.config.direction != Direction::Pull {
                    return;
                }
                self.hub_senders[edge].on_nack(&nack);
            }
            (
                Endpoint::Hub,
                Msg::Push {
                    edge,
                    source,
                    batch,
                },
            ) => {
                if self.config.direction != Direction::Push {
                    return;
                }
                self.crash(Endpoint::Hub);
                let _ = source;
                let outcome = match &mut self.hub_receiver {
                    Some(receiver) => receiver.on_batch(&batch),
                    None => return,
                };
                match outcome {
                    Outcome::Ack(ack) => {
                        self.transmit(Endpoint::Hub, Endpoint::Edge(edge), Msg::PushAck { ack });
                    }
                    Outcome::Nack(nack) => {
                        self.transmit(Endpoint::Hub, Endpoint::Edge(edge), Msg::PushNack { nack });
                    }
                    // A durable-apply or store failure is dropped: the edge retransmits on its
                    // timer, and the idempotent apply absorbs whatever landed.
                    Outcome::Failed(_) => {}
                }
            }
            (Endpoint::Edge(edge), Msg::PushAck { ack }) => {
                if self.config.direction != Direction::Push || self.crash(Endpoint::Edge(edge)) {
                    return;
                }
                if let Some(&global) = self.index_of.get(&ack.source) {
                    self.edges[edge].push_awaiting[global] = None;
                    self.edges[edge].sender.on_ack(&ack);
                    self.schedule_push(edge, global, self.clock + 1);
                }
            }
            (Endpoint::Edge(edge), Msg::PushNack { nack }) => {
                if self.config.direction != Direction::Push || self.crash(Endpoint::Edge(edge)) {
                    return;
                }
                let source = match &nack {
                    Nack::Gap { source, .. }
                    | Nack::Corrupt { source, .. }
                    | Nack::BatchTooLarge { source, .. } => Some(source.clone()),
                    Nack::VersionMismatch { .. } | Nack::MetadataEpoch { .. } => None,
                };
                if let Some(source) = source
                    && let Some(&global) = self.index_of.get(&source)
                {
                    self.edges[edge].push_awaiting[global] = None;
                    self.edges[edge].sender.on_nack(&nack);
                    self.schedule_push(edge, global, self.clock + 1);
                }
            }
            (Endpoint::Edge(_), Msg::Pull { .. }) | (Endpoint::Hub, Msg::Empty { .. }) => {}
            // A message a role never receives in this direction (a hub does not answer a hub, an
            // edge sends no push acknowledgement) is ignored.
            _ => {}
        }
    }

    pub(super) fn done(&self) -> bool {
        if self.produced < self.total {
            return false;
        }
        match self.config.direction {
            Direction::Pull => self
                .edges
                .iter()
                .all(|edge| edge.receiver.sink().len() == self.total),
            Direction::Push => self
                .hub_receiver
                .as_ref()
                .is_some_and(|receiver| receiver.sink().len() == self.total),
        }
    }

    pub(super) fn verify_sink(&self, sink: &MemorySink) -> Result<(), String> {
        if sink.len() != self.total {
            return Err(format!(
                "applied {} distinct records, expected {}",
                sink.len(),
                self.total
            ));
        }
        let mut per_source: Vec<Vec<u64>> = vec![Vec::new(); self.all_sources.len()];
        for applied in sink.order() {
            if let Some(&index) = self.index_of.get(&applied.source) {
                per_source[index].push(applied.seq);
            }
        }
        for (index, seqs) in per_source.iter().enumerate() {
            let expected = self.per_source_len[index];
            if seqs.len() as u64 != expected {
                return Err(format!(
                    "source {} applied {} records, expected {expected}",
                    self.all_sources[index],
                    seqs.len()
                ));
            }
            for (position, seq) in seqs.iter().enumerate() {
                if *seq != position as u64 + 1 {
                    return Err(format!(
                        "source {} applied out of order: position {position} holds sequence {seq}",
                        self.all_sources[index]
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn verify(&self) -> Result<(), String> {
        match self.config.direction {
            Direction::Pull => {
                for (edge, state) in self.edges.iter().enumerate() {
                    self.verify_sink(state.receiver.sink())
                        .map_err(|reason| format!("edge {edge}: {reason}"))?;
                }
            }
            Direction::Push => {
                let receiver = self
                    .hub_receiver
                    .as_ref()
                    .ok_or_else(|| "push run has no hub receiver".to_owned())?;
                self.verify_sink(receiver.sink())?;
            }
        }
        Ok(())
    }

    pub(super) fn applied(&self) -> Vec<usize> {
        match self.config.direction {
            Direction::Pull => self
                .edges
                .iter()
                .map(|edge| edge.receiver.sink().len())
                .collect(),
            Direction::Push => self
                .hub_receiver
                .as_ref()
                .map(|receiver| vec![receiver.sink().len()])
                .unwrap_or_default(),
        }
    }
}
