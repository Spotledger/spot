//! One poll step per mailbox account: [`poll_once`].
//!
//! The step is safe to repeat, to crash in the middle of and to run on two nodes:
//!
//! - **One node polls.** The step takes the coordinator lease `mail_intake:<site>:<account>`
//!   first; without it nothing else happens ([`PollOutcome::NotLeader`]). The lease's fencing
//!   token travels with the cursor write, and the store refuses a token older than one it has
//!   seen, so a node that lost its lease in the middle of a poll cannot move the cursor.
//! - **The cursor moves only past handled mail.** A message is handled when the sink accepted
//!   it (or already had it), when the engine dropped it on purpose, or when the sink rejected
//!   it as a poison message. A retryable sink error stops the batch at that message and the
//!   next poll starts there.
//! - **A crash between the sink and the cursor does not duplicate.** Each accepted message is
//!   recorded in the seen store (by a digest of its Message-ID) before the cursor is written,
//!   and the next poll skips what the seen store knows. The one window left is a crash between
//!   the sink's `accept` and the seen record; a sink that keeps the [`DedupeKey`] with what it
//!   creates answers `Duplicate` there.
//! - **Time is an argument.** Nothing here reads a clock.

use std::fmt::Write as _;
use std::time::Duration;

use sha2::{Digest, Sha256};
use spot_coordinator::{Coordinator, FencingToken, Lease};
use spot_mail::{
    FetchLimits, FetchOutcome, IncomingMessage, MailError, MailboxCursor, PermanentFailure,
};
use time::OffsetDateTime;

use crate::account::Account;
use crate::marker::OwnMailKey;
use crate::ports::{
    Accepted, CredentialError, CredentialSource, CursorStore, DedupeKey, IntakeAudit, IntakeSink,
    MailFetcher, SeenStore, SinkError, StoreError,
};
use crate::report::{
    AuditEvent, DROP_AUTO_REPLY, DROP_DAILY_CAP, DROP_OWN_MAIL, PollOutcome, PollReport,
};

/// The header the outgoing queue adds to every message it sends, so a message that comes back
/// to a polled mailbox is recognised as our own. Its value is a keyed MAC over the message's
/// Message-ID ([`crate::OwnMailKey`]); [`poll_once`] drops a message whose marker verifies
/// (counted and audited as `own_mail`), and still drops by the `From` address.
pub const OUTGOING_MARKER_HEADER: &str = crate::marker::HEADER;

/// The longest delay after a transient failure: 15 minutes.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// The first delay after a transient failure.
pub const BASE_BACKOFF: Duration = Duration::from_secs(30);
/// The delay before the next poll when more mail waits than one poll took.
pub const MORE_DELAY: Duration = Duration::from_secs(1);

const SECONDS_PER_DAY: i64 = 86_400;

/// What one poll needs from the host.
pub struct PollDeps<'a> {
    /// The coordinator that hands out the account's lease.
    pub coordinator: &'a dyn Coordinator,
    /// Who holds the lease (this node's id).
    pub holder: &'a str,
    /// The site the account belongs to; part of the lease name.
    pub site: &'a str,
    /// How long the lease lasts. Longer than the slowest possible poll (the IMAP timeout times
    /// the number of protocol phases), for example five minutes.
    pub lease_ttl: Duration,
    /// The cursor store.
    pub cursors: &'a dyn CursorStore,
    /// The seen store.
    pub seen: &'a dyn SeenStore,
    /// The sink that takes messages in.
    pub sink: &'a dyn IntakeSink,
    /// The audit trail.
    pub audit: &'a dyn IntakeAudit,
    /// Where the password comes from.
    pub credentials: &'a dyn CredentialSource,
    /// What fetches mail.
    pub fetcher: &'a dyn MailFetcher,
}

/// What the host keeps for one account between polls: the failure streak, the daily counter and
/// an operator halt. It lives in memory; a restart begins a new day-count and a fresh streak.
#[derive(Debug, Default, Clone)]
pub struct PollState {
    failures: u32,
    halt: Option<(PollOutcome, &'static str)>,
    day: Option<i64>,
    day_count: u32,
    cap_audited_day: Option<i64>,
}

impl PollState {
    /// A fresh state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets polling resume after an operator fixed the credentials or the configuration.
    pub fn clear_halt(&mut self) {
        self.halt = None;
        self.failures = 0;
    }

    /// Whether polling is stopped until an operator acts, and why.
    pub fn halted(&self) -> Option<&'static str> {
        self.halt.map(|(_, reason)| reason)
    }

    /// Consecutive transient failures.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Messages the sink took in on the current UTC day (as of the last poll).
    pub fn accepted_today(&self) -> u32 {
        self.day_count
    }
}

/// The delay after the `failures`-th transient failure in a row: 30 s, 60 s, 2 min, ... capped
/// at [`MAX_BACKOFF`].
pub fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(10);
    BASE_BACKOFF.saturating_mul(1u32 << shift).min(MAX_BACKOFF)
}

pub(crate) fn normalise_message_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_ascii_lowercase()
}

/// The deduplication key of `message` read under mailbox generation `uid_validity`: a SHA-256
/// digest, in hex, of the normalised Message-ID. A message without one is keyed by its mailbox
/// position instead, which makes it unique per generation but not across a mailbox reset.
pub fn dedupe_key(message: &IncomingMessage, uid_validity: u32) -> DedupeKey {
    let basis = match message
        .message_id
        .as_deref()
        .map(normalise_message_id)
        .filter(|id| !id.is_empty())
    {
        Some(id) => format!("id:{id}"),
        None => format!("pos:{uid_validity}:{}", message.uid),
    };
    let digest = Sha256::digest(basis.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        let _ = write!(hex, "{byte:02x}");
    }
    DedupeKey(hex)
}

/// Whether the message came from the account's own address.
fn is_own_mail(account: &Account, message: &IncomingMessage) -> bool {
    message
        .from
        .as_ref()
        .is_some_and(|from| from.address.eq_ignore_ascii_case(account.address.trim()))
}

/// Whether the message carries a valid `X-Spot-Outgoing` marker of this site: it has a
/// Message-ID, the key is known and the keyed MAC over the id matches. A missing key, a missing
/// header, a forged value and a message without a Message-ID are all "no".
fn has_own_marker(key: Option<&OwnMailKey>, message: &IncomingMessage) -> bool {
    let (Some(key), Some(id), Some(value)) = (
        key,
        message.message_id.as_deref(),
        message.header(OUTGOING_MARKER_HEADER),
    ) else {
        return false;
    };
    key.verifies(id, value)
}

/// What a poll saw of marked own mail, for the one audit entry it writes.
#[derive(Default)]
struct MarkedOwn {
    count: u32,
    first_uid: Option<u32>,
}

enum Step {
    Handled,
    Stop(&'static str),
}

struct Run<'a> {
    account: &'a Account,
    now: OffsetDateTime,
    deps: &'a PollDeps<'a>,
    fence: FencingToken,
    day: i64,
}

/// Polls `account` once.
///
/// See the module documentation for the guarantees. The report and the audit trail hold counts,
/// UID positions and reason codes only.
pub async fn poll_once(
    account: &Account,
    now: OffsetDateTime,
    state: &mut PollState,
    deps: &PollDeps<'_>,
) -> PollReport {
    if !account.enabled {
        return PollReport::empty(PollOutcome::Disabled);
    }
    if let Some((outcome, reason)) = state.halt {
        let mut report = PollReport::empty(outcome);
        report.reason = Some(reason);
        return report;
    }
    if account.validate().is_err() {
        return halt(
            state,
            deps,
            account,
            PollOutcome::ConfigRejected,
            "account_invalid",
        );
    }

    let lease_name = format!("mail_intake:{}:{}", deps.site, account.name);
    let lease = match deps
        .coordinator
        .acquire(&lease_name, deps.holder, deps.lease_ttl)
        .await
    {
        Ok(lease) => lease,
        Err(error) if error.code == spot_coordinator::error::LEASE_HELD => {
            let mut report = PollReport::empty(PollOutcome::NotLeader);
            report.next_delay = Some(account.poll_interval);
            return report;
        }
        Err(_) => return failed(state, "coordinator_unavailable"),
    };

    let day = now.unix_timestamp().div_euclid(SECONDS_PER_DAY);
    if state.day != Some(day) {
        state.day = Some(day);
        state.day_count = 0;
    }
    let run = Run {
        account,
        now,
        deps,
        fence: lease.token,
        day,
    };
    let report = run.execute(state).await;
    release(deps.coordinator, &lease).await;
    report
}

async fn release(coordinator: &dyn Coordinator, lease: &Lease) {
    // A stale handle means another node already took over; nothing to give back.
    if coordinator.release(lease).await.is_err() {
        tracing::debug!("mail intake lease already gone at release");
    }
}

fn halt(
    state: &mut PollState,
    deps: &PollDeps<'_>,
    account: &Account,
    outcome: PollOutcome,
    reason: &'static str,
) -> PollReport {
    state.halt = Some((outcome, reason));
    if outcome == PollOutcome::AuthFailed {
        deps.audit.record(&account.name, AuditEvent::AuthFailed);
    }
    let mut report = PollReport::empty(outcome);
    report.reason = Some(reason);
    report
}

fn failed(state: &mut PollState, reason: &'static str) -> PollReport {
    state.failures = state.failures.saturating_add(1);
    let mut report = PollReport::empty(PollOutcome::Failed);
    report.reason = Some(reason);
    report.next_delay = Some(backoff(state.failures));
    report
}

impl Run<'_> {
    async fn execute(&self, state: &mut PollState) -> PollReport {
        let account = self.account;
        let deps = self.deps;

        let secret = match deps.credentials.resolve(&account.credential_ref).await {
            Ok(secret) => secret,
            Err(CredentialError::NotFound) => {
                return halt(
                    state,
                    deps,
                    account,
                    PollOutcome::AuthFailed,
                    "credential_missing",
                );
            }
            Err(CredentialError::Unavailable) => {
                return failed(state, "credential_unavailable");
            }
        };
        let loaded = match deps.cursors.load(&account.name).await {
            Ok(cursor) => cursor,
            Err(_) => return failed(state, "store_unavailable"),
        };
        let limits = FetchLimits {
            max_messages: account.limits.max_messages_per_poll,
            max_message_bytes: account.limits.max_message_bytes,
        };
        let batch = match deps.fetcher.fetch(account, secret, loaded, limits).await {
            Ok(batch) => batch,
            Err(MailError::Permanent(PermanentFailure::Authentication { .. })) => {
                return halt(state, deps, account, PollOutcome::AuthFailed, "auth_failed");
            }
            Err(MailError::Permanent(_)) => {
                return halt(
                    state,
                    deps,
                    account,
                    PollOutcome::ConfigRejected,
                    "server_rejected",
                );
            }
            Err(MailError::Transient(_)) => return failed(state, "unreachable"),
        };

        if batch.outcome == FetchOutcome::MailboxReset {
            return self
                .reset(state, loaded, &batch.cursor, batch.messages.len())
                .await;
        }

        let validity = batch.cursor.uid_validity;
        let own_key = deps.credentials.own_mail_key().await;
        let mut marked = MarkedOwn::default();
        let mut report = PollReport::empty(PollOutcome::Polled);
        report.fetched = u32::try_from(batch.messages.len()).unwrap_or(u32::MAX);
        report.uid_range = match (batch.messages.first(), batch.messages.last()) {
            (Some(first), Some(last)) => Some((first.uid, last.uid)),
            _ => None,
        };
        report.more = batch.more;

        let prior_uid = loaded
            .filter(|cursor| cursor.uid_validity == validity)
            .map_or(0, |cursor| cursor.last_uid);
        let mut progressed = prior_uid;
        let mut stopped: Option<&'static str> = None;
        for message in &batch.messages {
            let step = self
                .handle(
                    state,
                    &mut report,
                    message,
                    validity,
                    own_key.as_ref(),
                    &mut marked,
                )
                .await;
            match step {
                Step::Handled => progressed = progressed.max(message.uid),
                Step::Stop(reason) => {
                    stopped = Some(reason);
                    break;
                }
            }
        }

        if let Some(first_uid) = marked.first_uid {
            deps.audit.record(
                &account.name,
                AuditEvent::OwnMail {
                    count: marked.count,
                    first_uid,
                },
            );
        }

        let target = if stopped.is_some() {
            MailboxCursor {
                uid_validity: validity,
                last_uid: progressed,
            }
        } else {
            batch.cursor
        };
        if loaded != Some(target) {
            match deps.cursors.store(&account.name, target, self.fence).await {
                Ok(()) => {}
                Err(StoreError::StaleFence) => {
                    report.outcome = PollOutcome::NotLeader;
                    report.reason = Some("stale_fence");
                    report.next_delay = Some(account.poll_interval);
                    return report;
                }
                Err(StoreError::Unavailable) => {
                    state.failures = state.failures.saturating_add(1);
                    report.outcome = PollOutcome::Failed;
                    report.reason = Some("store_unavailable");
                    report.next_delay = Some(backoff(state.failures));
                    return report;
                }
            }
        }

        match stopped {
            Some(reason) => {
                state.failures = state.failures.saturating_add(1);
                report.outcome = PollOutcome::Failed;
                report.reason = Some(reason);
                report.next_delay = Some(backoff(state.failures));
            }
            None => {
                state.failures = 0;
                report.next_delay = Some(if batch.more {
                    MORE_DELAY
                } else {
                    account.poll_interval
                });
            }
        }
        report
    }

    /// A changed `UIDVALIDITY`: store the new generation with a fresh position, import nothing.
    /// The next poll reads the mailbox from the start and the seen store skips what the site
    /// already has.
    async fn reset(
        &self,
        state: &mut PollState,
        loaded: Option<MailboxCursor>,
        new: &MailboxCursor,
        returned: usize,
    ) -> PollReport {
        let account = self.account;
        self.deps.audit.record(
            &account.name,
            AuditEvent::MailboxReset {
                old_validity: loaded.map(|cursor| cursor.uid_validity),
                new_validity: new.uid_validity,
            },
        );
        let fresh = MailboxCursor {
            uid_validity: new.uid_validity,
            last_uid: 0,
        };
        match self
            .deps
            .cursors
            .store(&account.name, fresh, self.fence)
            .await
        {
            Ok(()) => {}
            Err(StoreError::StaleFence) => {
                let mut report = PollReport::empty(PollOutcome::NotLeader);
                report.reason = Some("stale_fence");
                report.next_delay = Some(account.poll_interval);
                return report;
            }
            Err(StoreError::Unavailable) => {
                return failed(state, "store_unavailable");
            }
        }
        state.failures = 0;
        let mut report = PollReport::empty(PollOutcome::MailboxReset);
        report.fetched = u32::try_from(returned).unwrap_or(u32::MAX);
        report.reason = Some("mailbox_reset");
        report.next_delay = Some(MORE_DELAY);
        report
    }

    async fn handle(
        &self,
        state: &mut PollState,
        report: &mut PollReport,
        message: &IncomingMessage,
        validity: u32,
        own_key: Option<&OwnMailKey>,
        marked: &mut MarkedOwn,
    ) -> Step {
        let account = self.account;
        let deps = self.deps;

        if has_own_marker(own_key, message) {
            *report.dropped.entry(DROP_OWN_MAIL).or_insert(0) += 1;
            marked.count = marked.count.saturating_add(1);
            marked.first_uid.get_or_insert(message.uid);
            return Step::Handled;
        }
        if is_own_mail(account, message) {
            *report.dropped.entry(DROP_OWN_MAIL).or_insert(0) += 1;
            return Step::Handled;
        }
        if message.auto_submitted {
            *report.dropped.entry(DROP_AUTO_REPLY).or_insert(0) += 1;
            return Step::Handled;
        }

        let key = dedupe_key(message, validity);
        match deps.seen.contains(&account.name, key.as_str()).await {
            Ok(true) => {
                report.duplicates += 1;
                return Step::Handled;
            }
            Ok(false) => {}
            Err(_) => return Step::Stop("store_unavailable"),
        }

        if state.day_count >= account.limits.max_messages_per_day {
            *report.dropped.entry(DROP_DAILY_CAP).or_insert(0) += 1;
            if state.cap_audited_day != Some(self.day) {
                state.cap_audited_day = Some(self.day);
                deps.audit.record(
                    &account.name,
                    AuditEvent::DailyCapReached {
                        day: self.day,
                        cap: account.limits.max_messages_per_day,
                    },
                );
            }
            return Step::Handled;
        }

        if message.truncated {
            report.truncated += 1;
        }
        match deps.sink.accept(account, message, &key).await {
            Ok(Accepted::Created(_)) => {
                report.accepted += 1;
                state.day_count = state.day_count.saturating_add(1);
            }
            Ok(Accepted::Duplicate) => report.duplicates += 1,
            Ok(Accepted::Dropped(reason)) => {
                *report.dropped.entry(reason).or_insert(0) += 1;
            }
            Err(SinkError::Retryable) => return Step::Stop("sink_retryable"),
            Err(SinkError::Rejected(reason)) => {
                report.rejected += 1;
                deps.audit.record(
                    &account.name,
                    AuditEvent::SinkRejected {
                        uid: message.uid,
                        reason,
                    },
                );
            }
        }
        match deps
            .seen
            .first_time(&account.name, key.as_str(), self.now)
            .await
        {
            Ok(_) => Step::Handled,
            Err(_) => Step::Stop("store_unavailable"),
        }
    }
}
