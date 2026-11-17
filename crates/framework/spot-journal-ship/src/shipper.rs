//! The shipper: one step reads the outbox after the persisted watermark, protects each line,
//! hands the batch to the relay, and advances the watermark **only** to a position the relay
//! reports durable.
//!
//! # Why a crash at any boundary converges
//!
//! The order inside a step is read, protect, append, seal, save the watermark. A crash before
//! the append writes nothing. A crash after the append and before the watermark write leaves the
//! relay holding entries the watermark does not cover; the next step re-reads them, finds the
//! relay's `next_seq` ahead of the first entry, and drops the entries the relay already holds
//! (the relay refuses a repeat, and the shipper never offers one). A crash after the watermark
//! write simply continues. Under `local` and `replica-sync` the relay buffers an open segment in
//! memory: the watermark only reaches the last *sealed* position, so a restart re-reads the
//! buffered entries from the outbox, which the barrier keeps (`feed::cleanup`).
//!
//! # One writer
//!
//! Every step first asks [`WriterLease::hold`]. A node that loses the lease drops its relay and
//! stops shipping within one lease interval; the relay's own writer check (`split_brain`) is the
//! second line of defence.
//!
//! # Fail closed
//!
//! A chain that does not verify, a broken manifest or an outbox that cannot be shipped in order
//! stops the watermark; the shipper never skips an entry. A store outage is retried with
//! backoff and never drops an entry.

use std::sync::Arc;
use std::time::Duration;

use spot_events_contract::envelope::Position;
use spot_journal_commit::{ChunkRelay as _, ShipEntry, Shipped};
use spot_journal_manifest::Fork;
use tokio::sync::watch;

use crate::error::ShipFault;
use crate::ports::{
    Alarm, AlarmKind, Alarms, ChainChecker, ChainState, ForkMark, KeyBackup, Mark, OutboxEntry,
    OutboxRead, OutboxSource, Protector, RelayFactory, ShipRelay, WatermarkStore, WriterLease,
};
use crate::progress::{self, Progress, ProgressHandle};

/// The reason recorded in the fork record of a generation the outbox opened.
const FORK_REASON: &str = "the outbox generation changed (restore from backup)";

/// What one step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A batch was made durable.
    Shipped {
        /// How many entries the batch held.
        entries: usize,
        /// The durable position after the step.
        durable: Position,
    },
    /// Nothing to ship.
    Idle,
    /// Another node holds the lease: nothing was read or written.
    NotWriter,
    /// The step failed and will be retried (the code says why).
    Degraded {
        /// The fault's stable code.
        code: &'static str,
    },
    /// The shipper stopped and needs a person (a broken chain).
    Stopped {
        /// The code it stopped with.
        code: &'static str,
    },
}

/// The shipper's settings (the host builds them from the site's `[journal]` table).
#[derive(Clone, Debug)]
pub struct ShipperConfig {
    /// The journal source: the site name, or `<site>#<partition>`.
    pub source: String,
    /// The policy text for the readiness line and `RPO_BREACH`.
    pub policy: &'static str,
    /// Whether a batch accepted by the relay is already durable (`remote-sync`: a tail chunk).
    pub append_is_durable: bool,
    /// The most entries one step reads.
    pub batch_max_entries: usize,
    /// How long the shipper may fail before `RPO_BREACH` is raised, in milliseconds.
    pub rpo_grace_millis: i64,
    /// How often the chain is verified when nothing sealed, in milliseconds.
    pub verify_every_millis: i64,
}

impl ShipperConfig {
    /// A configuration with the defaults of the brief: 2,000 entries, 900 s grace, 10 minutes.
    #[must_use]
    pub fn new(source: &str, policy: &'static str, append_is_durable: bool) -> Self {
        Self {
            source: source.to_owned(),
            policy,
            append_is_durable,
            batch_max_entries: 2_000,
            rpo_grace_millis: 900_000,
            verify_every_millis: 600_000,
        }
    }

    fn validate(&self) -> Result<(), ShipFault> {
        if self.source.trim().is_empty() {
            return Err(ShipFault::Config("the source is empty"));
        }
        if self.batch_max_entries == 0 {
            return Err(ShipFault::Config("batch_max_entries must be at least 1"));
        }
        Ok(())
    }
}

/// The ports a shipper is built from.
pub struct Ports<F: RelayFactory> {
    /// The site's outbox.
    pub outbox: Arc<dyn OutboxSource>,
    /// The persisted watermark.
    pub marks: Arc<dyn WatermarkStore>,
    /// The one-writer lease.
    pub lease: Arc<dyn WriterLease>,
    /// The personal-field transform.
    pub protector: Arc<dyn Protector>,
    /// Where conditions are raised.
    pub alarms: Arc<dyn Alarms>,
    /// The relay factory.
    pub factory: F,
    /// Continuous verification; `None` turns it off (a test).
    pub checker: Option<Arc<dyn ChainChecker>>,
    /// Key backup status; `None` when the provider cannot tell.
    pub backup: Option<Arc<dyn KeyBackup>>,
}

/// The shipper of one site and source.
pub struct Shipper<F: RelayFactory> {
    cfg: ShipperConfig,
    outbox: Arc<dyn OutboxSource>,
    marks: Arc<dyn WatermarkStore>,
    lease: Arc<dyn WriterLease>,
    protector: Arc<dyn Protector>,
    alarms: Arc<dyn Alarms>,
    checker: Option<Arc<dyn ChainChecker>>,
    backup: Option<Arc<dyn KeyBackup>>,
    factory: F,
    relay: Option<F::Relay>,
    mark: Option<Mark>,
    /// The lease was lost: the watermark another writer moved is read again before shipping.
    reload: bool,
    next_batch_id: u64,
    needs_resume: bool,
    unshipped_since: Option<i64>,
    degraded_since: Option<i64>,
    behind_entries: u64,
    rpo_raised: bool,
    gap_raised: bool,
    backup_raised: bool,
    stopped: Option<&'static str>,
    last_verify: Option<i64>,
    last_seal: Option<i64>,
    shipped_total: u64,
    failures: u32,
    progress: watch::Sender<Progress>,
}

impl<F: RelayFactory> Shipper<F> {
    /// A shipper over `ports`, and the handle readers wait on.
    ///
    /// # Errors
    /// [`ShipFault::Config`] for an unusable configuration.
    pub fn new(cfg: ShipperConfig, ports: Ports<F>) -> Result<(Self, ProgressHandle), ShipFault> {
        cfg.validate()?;
        let (progress, handle) = progress::channel();
        let shipper = Self {
            cfg,
            outbox: ports.outbox,
            marks: ports.marks,
            lease: ports.lease,
            protector: ports.protector,
            alarms: ports.alarms,
            checker: ports.checker,
            backup: ports.backup,
            factory: ports.factory,
            relay: None,
            mark: None,
            reload: false,
            next_batch_id: 0,
            needs_resume: false,
            unshipped_since: None,
            degraded_since: None,
            behind_entries: 0,
            rpo_raised: false,
            gap_raised: false,
            backup_raised: false,
            stopped: None,
            last_verify: None,
            last_seal: None,
            shipped_total: 0,
            failures: 0,
            progress,
        };
        Ok((shipper, handle))
    }

    /// The source this shipper writes.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.cfg.source
    }

    /// How many steps in a row failed: the host's backoff is a function of it.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.failures
    }

    /// The backoff before the next step after `failures` failures in a row: doubling from the
    /// base, never above 30 seconds.
    #[must_use]
    pub fn backoff(base: Duration, failures: u32) -> Duration {
        let cap = Duration::from_secs(30);
        let shift = failures.saturating_sub(1).min(16);
        base.saturating_mul(1u32 << shift).min(cap)
    }

    /// One pass of the loop at `now_millis`.
    pub async fn step(&mut self, now_millis: i64) -> Step {
        if let Some(code) = self.stopped {
            self.publish(now_millis, false);
            return Step::Stopped { code };
        }
        if !self.lease.hold().await {
            // Not the writer: forget the relay so a later acquisition reopens and resumes it.
            self.relay = None;
            self.reload = true;
            self.publish(now_millis, false);
            return Step::NotWriter;
        }
        match self.ship_once(now_millis).await {
            Ok(step) => {
                self.failures = 0;
                self.recovered(now_millis).await;
                self.publish(now_millis, true);
                step
            }
            Err(fault) => {
                let step = self.failed(fault, now_millis).await;
                self.publish(now_millis, true);
                step
            }
        }
    }

    /// Seals what is open and saves the watermark at a clean stop, then gives the lease up.
    pub async fn shutdown(&mut self, now_millis: i64) {
        if self.stopped.is_none()
            && let Some(relay) = self.relay.as_mut()
        {
            // Best effort: a failure only means the next start re-reads from the watermark.
            let _ = relay.seal_if_due(now_millis).await;
            let _ = self.settle(now_millis, None).await;
        }
        self.relay = None;
        self.lease.release().await;
        self.publish(now_millis, false);
    }

    fn publish(&self, now: i64, writer: bool) {
        let durable = self.mark.as_ref().map_or(
            Position {
                generation: 0,
                seq: 0,
            },
            |m| m.position,
        );
        let since = |at: Option<i64>| {
            at.map_or(0, |at| {
                u64::try_from(now.saturating_sub(at) / 1_000).unwrap_or(0)
            })
        };
        let progress = Progress {
            durable,
            behind_entries: self.behind_entries,
            lag_secs: since(self.unshipped_since),
            writer,
            degraded: self.degraded_since.is_some(),
            degraded_secs: since(self.degraded_since),
            stopped: self.stopped,
            last_seal_millis: self.last_seal,
            shipped_total: self.shipped_total,
        };
        self.progress.send_replace(progress);
    }

    /// A step that worked: a failing spell is over.
    async fn recovered(&mut self, _now: i64) {
        self.degraded_since = None;
        if self.rpo_raised {
            self.rpo_raised = false;
            let subject = self.cfg.source.clone();
            self.alarms.clear(&subject, AlarmKind::RpoBreach).await;
        }
    }

    async fn failed(&mut self, fault: ShipFault, now: i64) -> Step {
        self.failures = self.failures.saturating_add(1);
        let code = fault.code();
        let since = *self.degraded_since.get_or_insert(now);
        match &fault {
            ShipFault::StoreDown { .. } => self.needs_resume = true,
            ShipFault::Stopped { code: relay_code } => {
                if *relay_code == "journal_relay.chain_broken"
                    || *relay_code == "journal_ship.outbox_gap"
                {
                    return self.stop_on_damage(relay_code, now).await;
                }
                // Another writer, a bad batch: drop the relay and start over from the store.
                self.relay = None;
            }
            ShipFault::ChainDamaged { code: finding } => {
                return self.stop_on_damage(finding, now).await;
            }
            ShipFault::OutboxInvalid(_) | ShipFault::Config(_) | ShipFault::TargetUnsafe(_) => {
                // Not curable by waiting; keep trying only so a repaired outbox is noticed, but
                // say so loudly through the RPO alarm below.
            }
            ShipFault::OutboxUnavailable | ShipFault::Protect { .. } | ShipFault::Watermark => {}
        }
        if now.saturating_sub(since) >= self.cfg.rpo_grace_millis && !self.rpo_raised {
            self.rpo_raised = true;
            let behind_secs = u64::try_from(now.saturating_sub(since) / 1_000).unwrap_or(0);
            let subject = self.cfg.source.clone();
            self.alarms
                .raise(
                    &subject,
                    Alarm::RpoBreach {
                        policy: self.cfg.policy,
                        behind_secs,
                    },
                )
                .await;
        }
        Step::Degraded { code }
    }

    async fn stop_on_damage(&mut self, finding: &'static str, _now: i64) -> Step {
        self.stopped = Some(finding);
        self.relay = None;
        if !self.gap_raised {
            self.gap_raised = true;
            let generation = self.mark.as_ref().map_or(0, |m| m.position.generation);
            let subject = self.cfg.source.clone();
            self.alarms
                .raise(
                    &subject,
                    Alarm::JournalGap {
                        source: subject.clone(),
                        generation,
                        range: finding.to_owned(),
                    },
                )
                .await;
        }
        Step::Stopped { code: finding }
    }

    async fn current_mark(&mut self, now: i64) -> Result<Mark, ShipFault> {
        if let Some(mark) = &self.mark
            && !self.reload
        {
            return Ok(mark.clone());
        }
        let mark = match self.marks.load(&self.cfg.source).await? {
            Some(mark) => mark,
            None => {
                // The row's existence is what switches the outbox barrier on: write it before
                // the first read, with nothing shipped.
                let mark = Mark::start();
                self.marks.save(&self.cfg.source, &mark, now).await?;
                mark
            }
        };
        self.mark = Some(mark.clone());
        self.reload = false;
        Ok(mark)
    }

    async fn ship_once(&mut self, now: i64) -> Result<Step, ShipFault> {
        let mark = self.current_mark(now).await?;
        // A restart or a regained lease: reopen the relay of the generation the mark names.
        if self.relay.is_none() && mark.position.generation > 0 {
            let fork = fork_record(mark.position.generation, mark.fork.as_ref());
            self.relay = Some(
                self.factory
                    .open(mark.position.generation, fork, now)
                    .await?,
            );
            self.needs_resume = false;
        }
        if self.needs_resume
            && let Some(relay) = self.relay.as_mut()
        {
            relay.resume().await.map_err(ShipFault::from)?;
            self.needs_resume = false;
        }
        // A chain that does not verify stops the shipper before it writes on top of it.
        self.check_chain(now, false).await?;
        let limit = self.cfg.batch_max_entries;
        // Read on from what the relay already holds, not from the durable watermark: under
        // `local` an open segment is buffered until it seals, and re-reading it every step would
        // stall a shipper whose batch is smaller than the buffer.
        let after = self.relay.as_ref().map_or(mark.position, |relay| Position {
            generation: relay.generation(),
            seq: relay.next_seq().saturating_sub(1),
        });
        let page = match self.outbox.read(after, limit).await? {
            OutboxRead::Entries(entries) => entries,
            OutboxRead::Resync => match self
                .outbox
                .read(
                    Position {
                        generation: 0,
                        seq: 0,
                    },
                    limit,
                )
                .await?
            {
                OutboxRead::Entries(entries) => entries,
                OutboxRead::Resync => {
                    return Err(ShipFault::OutboxInvalid(
                        "the outbox refused a read from its start",
                    ));
                }
            },
        };
        let Some(first) = page.first() else {
            self.unshipped_since = None;
            self.behind_entries = 0;
            self.maintain(now).await?;
            return Ok(Step::Idle);
        };
        let generation = first.position.generation;
        if page.iter().any(|e| e.position.generation != generation) {
            return Err(ShipFault::OutboxInvalid("a page spans two generations"));
        }
        self.unshipped_since.get_or_insert(now);
        self.ensure_generation(generation, &mark, now).await?;
        let page_len = page.len();
        let relay_next = self
            .relay
            .as_ref()
            .ok_or(ShipFault::Config("no relay is open"))?
            .next_seq();
        // Drop what the relay already holds (a crash between append and watermark write).
        let fresh: Vec<&OutboxEntry> = page
            .iter()
            .filter(|entry| entry.position.seq >= relay_next)
            .collect();
        let shipped = fresh.len();
        let mut durable_seq: Option<u64> = None;
        if let Some(first_fresh) = fresh.first() {
            if first_fresh.position.seq != relay_next {
                return Err(ShipFault::Stopped {
                    code: "journal_ship.outbox_gap",
                });
            }
            let mut batch = Vec::with_capacity(fresh.len());
            let mut expect = relay_next;
            for entry in &fresh {
                if entry.position.seq != expect {
                    return Err(ShipFault::OutboxInvalid("positions are not consecutive"));
                }
                expect = expect.saturating_add(1);
                batch.push(ShipEntry {
                    seq: entry.position.seq,
                    line: self.protector.protect(&entry.line)?,
                });
            }
            self.next_batch_id = self.next_batch_id.saturating_add(1);
            let batch_id = self.next_batch_id;
            let relay = self
                .relay
                .as_mut()
                .ok_or(ShipFault::Config("no relay is open"))?;
            let outcome = relay
                .ship(batch_id, &batch, millis_u64(now))
                .await
                .map_err(ShipFault::from)?;
            durable_seq = match outcome {
                Shipped::Durable { up_to_seq } => Some(up_to_seq),
                Shipped::Buffered => None,
            };
        } else if self.cfg.append_is_durable {
            // Everything read is already in the relay's tail chunks.
            durable_seq = Some(relay_next.saturating_sub(1));
        }
        let sealed = {
            let relay = self
                .relay
                .as_mut()
                .ok_or(ShipFault::Config("no relay is open"))?;
            relay.seal_if_due(now).await?
        };
        if sealed {
            self.last_seal = Some(now);
        }
        let durable = self.settle(now, durable_seq).await?;
        let last_read = page.last().map_or(0, |e| e.position.seq);
        self.shipped_total = self
            .shipped_total
            .saturating_add(u64::try_from(shipped).unwrap_or(0));
        if durable >= last_read {
            self.unshipped_since = None;
            self.behind_entries = 0;
        } else {
            self.behind_entries = u64::try_from(page_len).unwrap_or(0);
        }
        self.check_chain(now, sealed).await?;
        Ok(Step::Shipped {
            entries: shipped,
            durable: Position {
                generation,
                seq: durable,
            },
        })
    }

    /// Idle upkeep: seal what the limits say, save the watermark if sealing moved it, verify.
    async fn maintain(&mut self, now: i64) -> Result<(), ShipFault> {
        let mut sealed = false;
        let mut held = None;
        if let Some(relay) = self.relay.as_mut() {
            sealed = relay.seal_if_due(now).await?;
            // Under remote-sync everything the relay holds is in a tail chunk: durable.
            if self.cfg.append_is_durable {
                held = Some(relay.next_seq().saturating_sub(1));
            }
        }
        if sealed {
            self.last_seal = Some(now);
        }
        if self.relay.is_some() {
            self.settle(now, held).await?;
            self.check_chain(now, sealed).await?;
        }
        Ok(())
    }

    /// Opens the relay for `generation` when the open one is for another generation, recording
    /// the fork before the relay is opened so a crash in between reopens the same record.
    async fn ensure_generation(
        &mut self,
        generation: u64,
        mark: &Mark,
        now: i64,
    ) -> Result<(), ShipFault> {
        if self.relay.as_ref().map(|r| r.generation()) == Some(generation) {
            return Ok(());
        }
        if generation < mark.position.generation {
            return Err(ShipFault::OutboxInvalid(
                "the outbox went back to an older generation",
            ));
        }
        if mark.position.generation == 0 && generation != 1 {
            // A journal that never held generation 1 cannot be forked from it.
            return Err(ShipFault::Stopped {
                code: "journal_ship.generation_unmatched",
            });
        }
        let fork = if generation == 1 {
            None
        } else if mark.position.generation == generation {
            mark.fork.clone()
        } else {
            Some(ForkMark {
                forked_from: mark.position.seq,
                at_millis: now,
            })
        };
        if mark.position.generation != generation {
            let opened = Mark {
                position: Position { generation, seq: 0 },
                fork: fork.clone(),
            };
            self.marks.save(&self.cfg.source, &opened, now).await?;
            self.mark = Some(opened);
        }
        let record = fork_record(generation, fork.as_ref());
        self.relay = Some(self.factory.open(generation, record, now).await?);
        self.needs_resume = false;
        Ok(())
    }

    /// Computes the position that is durable now and persists it when it moved. `shipped` is the
    /// batch's own durable sequence (remote-sync).
    async fn settle(&mut self, now: i64, shipped: Option<u64>) -> Result<u64, ShipFault> {
        let Some(relay) = self.relay.as_ref() else {
            return Ok(self.mark.as_ref().map_or(0, |m| m.position.seq));
        };
        let generation = relay.generation();
        let sealed = relay.sealed_through();
        let mark = self.mark.clone().unwrap_or_else(Mark::start);
        let durable_seq = shipped.unwrap_or(0).max(sealed);
        let current = if mark.position.generation == generation {
            mark.position.seq
        } else {
            0
        };
        if durable_seq > current {
            let moved = Mark {
                position: Position {
                    generation,
                    seq: durable_seq,
                },
                fork: mark.fork,
            };
            self.marks.save(&self.cfg.source, &moved, now).await?;
            self.mark = Some(moved);
            Ok(durable_seq)
        } else {
            Ok(current)
        }
    }

    /// Light-verifies the chain after a seal and at least every `verify_every_millis`, and checks
    /// the key backup. A damaged chain stops the shipper.
    async fn check_chain(&mut self, now: i64, sealed: bool) -> Result<(), ShipFault> {
        let due = sealed
            || self
                .last_verify
                .is_none_or(|at| now.saturating_sub(at) >= self.cfg.verify_every_millis);
        if !due {
            return Ok(());
        }
        self.last_verify = Some(now);
        let generation = self.relay.as_ref().map_or(0, |r| r.generation());
        if let Some(checker) = self.checker.clone()
            && generation > 0
        {
            match checker.check(generation).await? {
                ChainState::Clean => {
                    if self.gap_raised {
                        self.gap_raised = false;
                        let subject = self.cfg.source.clone();
                        self.alarms.clear(&subject, AlarmKind::JournalGap).await;
                    }
                }
                ChainState::Damaged { code } => {
                    return Err(ShipFault::ChainDamaged { code });
                }
            }
        }
        if let Some(backup) = self.backup.clone() {
            let subject = self.cfg.source.clone();
            match backup.backed_up().await {
                Some(false) if !self.backup_raised => {
                    self.backup_raised = true;
                    self.alarms
                        .raise(
                            &subject,
                            Alarm::KeyBackupMissing {
                                key: "journal".to_owned(),
                            },
                        )
                        .await;
                }
                Some(true) if self.backup_raised => {
                    self.backup_raised = false;
                    self.alarms
                        .clear(&subject, AlarmKind::KeyBackupMissing)
                        .await;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn fork_record(generation: u64, fork: Option<&ForkMark>) -> Option<Fork> {
    if generation <= 1 {
        return None;
    }
    let (forked_from, at_millis) = fork.map_or((0, 0), |f| (f.forked_from, f.at_millis));
    Some(Fork::new(generation, forked_from, FORK_REASON, at_millis))
}

fn millis_u64(millis: i64) -> u64 {
    u64::try_from(millis).unwrap_or(0)
}
