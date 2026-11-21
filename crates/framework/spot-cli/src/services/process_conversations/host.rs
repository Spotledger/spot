//! The conversation core of one site: the receiving path (rate and size bounds, then the link
//! exchange's inbox behind the contract gate), the trigger index of `message_arrived` triggers,
//! and the driver that wakes waits, starts requests and takes timeouts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use spot_bpm_conversations::handlers::unix_ms;
use spot_bpm_conversations::{
    CompensationHook, ContractBook, ConversationError, ConversationStore, Conversations,
    EngineCaseStarter, GateAudit, LinkAudit, LinkLimits, PeerDirectory, ReceiveError, Refusal,
    Runtime, SharedSigner, WaitFor, trigger,
};
use spot_bpm_engine::{CaseStore, Engine, TimerPort};
use spot_bpm_triggers::{AllowAll, TriggerIndex, TriggerIntake, TriggerRegistry};
use spot_core::Clock;
use spot_link_exchange::{
    Inbox, Message, ReceiveOutcome, ReceiveStatus, RejectCode, TransportParty,
};
use spot_tenant_link::{Link, LinkId};
use time::OffsetDateTime;

use super::Settings;
use super::seams::{LinkMessageSink, LinkResources};

/// The service principal the module's own work (a request started by a message, a wait woken by
/// one, a timeout) is recorded under in the audit log. A caller never selects it: it is not a
/// parameter of anything the peer or an operator can send.
pub const SERVICE_PRINCIPAL: &str = "service:process_conversations";

/// The audit codes of a message this module refuses before the link exchange sees it.
pub mod refusals {
    /// The link delivered more messages this minute than the site allows.
    pub const RATE_LIMITED: &str = "process_conversations.rate_limited";
    /// The payload is larger than the site allows.
    pub const TOO_LARGE: &str = "process_conversations.payload_too_large";
}

/// How many links the rate table tracks before it forgets the minutes that are over.
const MAX_TRACKED_LINKS: usize = 4_096;

/// Everything the core is built from.
pub struct CoreParts {
    /// The site name, for logs.
    pub site: String,
    /// The site's clock: every instant the module uses comes from it.
    pub clock: Arc<dyn Clock>,
    /// The settings, validated.
    pub settings: Settings,
    /// The contracts this site holds and the role it plays in each.
    pub book: ContractBook,
    /// The conversation records (durable in production).
    pub store: Arc<dyn ConversationStore>,
    /// The routes to the peers.
    pub peers: Arc<dyn PeerDirectory>,
    /// What the link host offers.
    pub links: Arc<dyn LinkResources>,
    /// The site's identity key.
    pub signer: SharedSigner,
    /// Where the receiving gate writes its refusals; the link's own audit chain when `None`.
    pub gate_audit: Option<Arc<dyn GateAudit>>,
}

/// Counters, for the health check and tests. Never a label, never a value from a message.
#[derive(Default)]
struct Counters {
    received: AtomicU64,
    applied: AtomicU64,
    refused: AtomicU64,
    not_taken: AtomicU64,
    rate_limited: AtomicU64,
    too_large: AtomicU64,
    errors: AtomicU64,
    cycles: AtomicU64,
    started: AtomicU64,
    woken: AtomicU64,
    timed_out: AtomicU64,
}

/// A snapshot of [`ConversationCore::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Messages the carrier delivered.
    pub received: u64,
    /// Messages stored in order.
    pub applied: u64,
    /// Messages refused (the sender has a signed receipt with the reason).
    pub refused: u64,
    /// Messages not taken (a limit, an unknown link): the sender keeps and resends.
    pub not_taken: u64,
    /// Messages over the per-link rate.
    pub rate_limited: u64,
    /// Messages over the size bound.
    pub too_large: u64,
    /// Failed receive and driver passes.
    pub errors: u64,
    /// Driver passes run.
    pub cycles: u64,
    /// Requests started by an arrival.
    pub started: u64,
    /// Waits woken by an arrival.
    pub woken: u64,
    /// Waits that ran out of time.
    pub timed_out: u64,
}

/// The receiving side and the shared state of one site's conversations.
pub struct ConversationCore {
    site: String,
    clock: Arc<dyn Clock>,
    settings: Settings,
    runtime: Arc<Runtime>,
    links: Arc<dyn LinkResources>,
    signer: SharedSigner,
    index: Mutex<Arc<TriggerIndex>>,
    rates: Mutex<BTreeMap<LinkId, (u64, u32)>>,
    counters: Counters,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ConversationCore {
    /// The core of a site.
    #[must_use]
    pub fn new(parts: CoreParts) -> Arc<Self> {
        let gate_audit: Arc<dyn GateAudit> = parts
            .gate_audit
            .unwrap_or_else(|| Arc::new(LinkAudit::new(parts.links.audit())));
        let limits = Arc::new(LinkLimits::with(
            parts.settings.max_open_conversations,
            parts.settings.max_held_inputs,
        ));
        let runtime = Arc::new(
            Runtime::new(parts.book, parts.store, parts.peers, gate_audit)
                .with_limits(limits)
                .with_default_wait_for(parts.settings.wait_for().unwrap_or(WaitFor::Queued)),
        );
        Arc::new(Self {
            site: parts.site,
            clock: parts.clock,
            settings: parts.settings,
            runtime,
            links: parts.links,
            signer: parts.signer,
            index: Mutex::new(Arc::new(TriggerIndex::new())),
            rates: Mutex::new(BTreeMap::new()),
            counters: Counters::default(),
        })
    }

    /// The shared runtime the node handlers and the driver use.
    #[must_use]
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.runtime
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The site name.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The current instant, from the site's clock.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        self.clock.now_utc()
    }

    /// Replaces the `message_arrived` triggers of the published processes.
    pub fn set_index(&self, index: TriggerIndex) {
        *lock(&self.index) = Arc::new(index);
    }

    /// The trigger index in force.
    #[must_use]
    pub fn index(&self) -> Arc<TriggerIndex> {
        Arc::clone(&lock(&self.index))
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let c = &self.counters;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Stats {
            received: get(&c.received),
            applied: get(&c.applied),
            refused: get(&c.refused),
            not_taken: get(&c.not_taken),
            rate_limited: get(&c.rate_limited),
            too_large: get(&c.too_large),
            errors: get(&c.errors),
            cycles: get(&c.cycles),
            started: get(&c.started),
            woken: get(&c.woken),
            timed_out: get(&c.timed_out),
        }
    }

    /// What the health check says: empty when nothing is wrong.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let stats = self.stats();
        let mut warnings = Vec::new();
        if stats.rate_limited > 0 {
            warnings.push(format!(
                "{} messages were not taken because a link went over its rate",
                stats.rate_limited
            ));
        }
        if stats.too_large > 0 {
            warnings.push(format!(
                "{} messages were refused for their size",
                stats.too_large
            ));
        }
        warnings
    }

    fn bump(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Writes the one audit row a refusal of this module leaves: ids and a code, never a body.
    fn refuse(
        &self,
        link: &Link,
        message: &Message,
        code: &'static str,
        now_ms: u64,
    ) -> Result<(), ReceiveError> {
        self.runtime
            .audit
            .refused(&Refusal {
                link: link.id,
                conversation: message.conversation.clone(),
                message_id: message.id,
                code,
                at_ms: now_ms,
            })
            .map_err(ReceiveError::from)?;
        tracing::warn!(
            target: "spot::audit::process_conversations",
            site = %self.site, who = SERVICE_PRINCIPAL, rule = code,
            "a conversation message was not taken"
        );
        Ok(())
    }

    /// The rate and size bounds, before anything is parsed or stored.
    fn admit(
        &self,
        link: &Link,
        message: &Message,
        now_ms: u64,
    ) -> Result<Option<ReceiveOutcome>, ReceiveError> {
        if message.payload.len() > self.settings.max_payload_bytes {
            self.bump(&self.counters.too_large);
            self.refuse(link, message, refusals::TOO_LARGE, now_ms)?;
            return Ok(Some(ReceiveOutcome {
                status: ReceiveStatus::Rejected(RejectCode::CapacityExceeded),
                receipts: Vec::new(),
            }));
        }
        let minute = now_ms / 60_000;
        let over = {
            let mut rates = lock(&self.rates);
            if rates.len() >= MAX_TRACKED_LINKS && !rates.contains_key(&link.id) {
                rates.retain(|_, (when, _)| *when == minute);
            }
            let entry = rates.entry(link.id).or_insert((minute, 0));
            if entry.0 != minute {
                *entry = (minute, 0);
            }
            entry.1 = entry.1.saturating_add(1);
            entry.1 > self.settings.max_messages_per_minute
        };
        if over {
            self.bump(&self.counters.rate_limited);
            self.refuse(link, message, refusals::RATE_LIMITED, now_ms)?;
            return Ok(Some(ReceiveOutcome {
                status: ReceiveStatus::Busy,
                receipts: Vec::new(),
            }));
        }
        Ok(None)
    }

    /// Takes one message from a carrier. See [`LinkMessageSink::receive`].
    ///
    /// The link is read from the host **per message**, so a link revoked, expired or re-granted a
    /// moment ago is judged as it is now: grants per topic and direction, expiry, the peer-region
    /// requirement and revocation are checked by the inbox (`Link::allows_with_region`) before
    /// any Case is touched, and the apply path re-reads the link from the store.
    ///
    /// # Errors
    /// [`ReceiveError`] when the exchange, the audit trail or the conversation store fails.
    pub fn receive_message(
        &self,
        link_id: &LinkId,
        transport: &TransportParty,
        message: &Message,
    ) -> Result<ReceiveOutcome, ReceiveError> {
        self.bump(&self.counters.received);
        let now_ms = unix_ms(self.now());
        let Some(shared) = self.links.link(link_id) else {
            // Nothing of a link this site does not know is stored or judged, and no answer
            // confirms or denies that the id exists.
            self.bump(&self.counters.not_taken);
            tracing::warn!(
                target: "spot::audit::process_conversations",
                site = %self.site, who = SERVICE_PRINCIPAL, rule = "unknown_link",
                "a conversation message arrived on a link this site does not hold"
            );
            return Ok(ReceiveOutcome {
                status: ReceiveStatus::Busy,
                receipts: Vec::new(),
            });
        };
        let link = shared
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(outcome) = self.admit(&link, message, now_ms).inspect_err(|_| {
            self.bump(&self.counters.errors);
        })? {
            return Ok(outcome);
        }
        let links = self.links.links();
        let inbox_store = self.links.inbox();
        let quarantine = self.links.quarantine();
        let audit = self.links.audit();
        let feed = self.links.feed();
        let region = self.links.region();
        let mut inbox =
            Inbox::new(&*inbox_store, &*audit, &*quarantine, &*self.signer).with_links(&*links);
        if let Some(oracle) = &region {
            inbox = inbox.with_region_oracle(&**oracle);
        }
        let gate = spot_bpm_conversations::ConformantInbox::new(
            &inbox,
            &*inbox_store,
            &*quarantine,
            &*self.signer,
            &self.runtime,
        )
        .with_ledger(&*feed);
        let outcome = gate
            .receive_from(&link, transport, message, now_ms)
            .inspect_err(|_| self.bump(&self.counters.errors))?;
        match outcome.status {
            ReceiveStatus::Applied | ReceiveStatus::PendingApply => {
                self.bump(&self.counters.applied);
            }
            ReceiveStatus::Rejected(_) => self.bump(&self.counters.refused),
            ReceiveStatus::Busy => self.bump(&self.counters.not_taken),
            ReceiveStatus::Parked | ReceiveStatus::Duplicate => {}
        }
        Ok(outcome)
    }

    /// The trigger intake over the index in force, for an engine.
    #[must_use]
    pub fn intake<S: CaseStore + 'static, T: TimerPort + 'static>(
        &self,
        engine: &Arc<Engine<S, T>>,
    ) -> Arc<TriggerIntake> {
        let mut registry = TriggerRegistry::with_builtin_kinds();
        trigger::register(&mut registry);
        let starter = Arc::new(EngineCaseStarter::new(
            Arc::clone(engine),
            Arc::clone(&self.runtime.store),
        ));
        // A start by a message is the platform's: it runs as the service principal, never as a
        // caller, and every start is audited with the rule that decided it.
        Arc::new(TriggerIntake::new(
            self.index(),
            Arc::new(registry),
            starter,
            Arc::new(AllowAll),
        ))
    }
}

impl LinkMessageSink for ConversationCore {
    fn receive(
        &self,
        link: &LinkId,
        transport: &TransportParty,
        message: &Message,
    ) -> Result<ReceiveOutcome, ReceiveError> {
        self.receive_message(link, transport, message)
    }
}

/// What one driver pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CycleReport {
    /// Notices handled.
    pub handled: usize,
    /// Requests started by an arrival.
    pub started: usize,
    /// Waits woken by an arrival.
    pub woken: usize,
    /// Conversations a peer cancelled.
    pub cancelled: usize,
    /// Waits that ran out of time.
    pub timed_out: usize,
    /// Sends whose delivery wait ended.
    pub delivered: usize,
}

/// The driver of one site's conversations over an engine: arrivals first (they start requests and
/// wake waits), then the time-driven turns (timeouts, delivery waits). Time is the site clock's.
pub struct Driver<S: CaseStore + 'static, T: TimerPort + 'static> {
    core: Arc<ConversationCore>,
    engine: Arc<Engine<S, T>>,
    hook: Option<Arc<dyn CompensationHook>>,
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> Driver<S, T> {
    /// A driver over `engine`.
    #[must_use]
    pub fn new(core: Arc<ConversationCore>, engine: Arc<Engine<S, T>>) -> Self {
        Self {
            core,
            engine,
            hook: None,
        }
    }

    /// The same driver with a compensation hook (a peer cancelled the conversation).
    #[must_use]
    pub fn with_compensation(mut self, hook: Arc<dyn CompensationHook>) -> Self {
        self.hook = Some(hook);
        self
    }

    /// The library driver for this pass: the intake is rebuilt over the index in force, so a
    /// publish needs no restart.
    fn conversations(&self) -> Conversations<S, T> {
        let conversations = Conversations::new(
            Arc::clone(&self.core.runtime),
            Arc::clone(&self.engine),
            self.core.intake(&self.engine),
        );
        match &self.hook {
            Some(hook) => conversations.with_compensation(Arc::clone(hook)),
            None => conversations,
        }
    }

    /// One pass at `now`.
    ///
    /// # Errors
    /// The library's failure (a store, trigger or engine failure). Nothing past the failing
    /// notice is acknowledged, so the next pass resumes there.
    pub async fn cycle_at(&self, now: OffsetDateTime) -> Result<CycleReport, ConversationError> {
        let core = &self.core;
        core.bump(&core.counters.cycles);
        let conversations = self.conversations();
        let feed = core.links.feed();
        let inbox = core.links.inbox();
        let arrivals = conversations
            .process_arrivals(&*feed, &*inbox, now)
            .await
            .inspect_err(|_| core.bump(&core.counters.errors))?;
        let ticked = conversations
            .tick(now)
            .await
            .inspect_err(|_| core.bump(&core.counters.errors))?;
        let add = |counter: &AtomicU64, n: usize| {
            counter.fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        };
        add(&core.counters.started, arrivals.started);
        add(&core.counters.woken, arrivals.woken);
        add(&core.counters.timed_out, ticked.timed_out);
        let report = CycleReport {
            handled: arrivals.handled,
            started: arrivals.started,
            woken: arrivals.woken,
            cancelled: arrivals.cancelled,
            timed_out: ticked.timed_out,
            delivered: ticked.delivered,
        };
        if report.started + report.woken + report.cancelled + report.timed_out > 0 {
            tracing::info!(
                target: "spot::audit::process_conversations",
                site = %core.site, who = SERVICE_PRINCIPAL,
                started = report.started, woken = report.woken, cancelled = report.cancelled,
                timed_out = report.timed_out,
                "conversations advanced"
            );
        }
        Ok(report)
    }

    /// One pass now, at the site clock's instant.
    ///
    /// # Errors
    /// As [`Driver::cycle_at`].
    pub async fn cycle(&self) -> Result<CycleReport, ConversationError> {
        self.cycle_at(self.core.now()).await
    }
}
