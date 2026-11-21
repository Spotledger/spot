//! Actual audit-chain integration, tamper detection and fail-closed logging.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_audit_chain::{
    AppendOutcome, AuditChain, AuditStore, ChainEntry, Checkpoint, CheckpointPolicy, InMemoryStore,
    KeyRing, LocalSigner, StoreFuture,
};
use spot_deep_access::*;
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll, Waker},
};

struct Ready;
impl AuditExecutor for Ready {
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, DeepError> {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(v) => Ok(v),
            Poll::Pending => Err(DeepError::AuditUnavailable),
        }
    }
}
#[derive(Clone, Default)]
struct TamperStore {
    inner: Arc<InMemoryStore>,
    tamper: Arc<AtomicU8>,
}
impl AuditStore for TamperStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.inner.head(site)
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.inner.append_if_head(site, seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let mut entries = self.inner.range(site, from, to).await?;
            match self.tamper.load(Ordering::SeqCst) {
                1 => {
                    if let Some(e) = entries.first_mut() {
                        e.record.actor = "user:attacker".into();
                    }
                }
                2 => {
                    if !entries.is_empty() {
                        entries.remove(0);
                    }
                }
                3 => {
                    entries.pop();
                }
                _ => {}
            }
            Ok(entries)
        })
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

#[test]
fn report_verifies_actual_chain_and_rejects_edit_removal_and_tail_truncation() {
    let mut f = Fixture::new();
    let signer = Arc::new(LocalSigner::from_seed([9; 32]));
    let mut keys = KeyRing::new();
    keys.add(&signer.public_key()).unwrap();
    let store = TamperStore::default();
    let observable = store.clone();
    let chain = AuditChain::new(store, "tenant-a", signer, CheckpointPolicy { every: 1 });
    let mut log = ChainLog::new(chain, Ready);
    let c = issue(
        &f.request,
        Some(&f.receipt),
        f.operator.verifying_key().to_bytes(),
        Some(&f.attestation),
        &f.policy,
        100,
        [42; 48],
        &mut IssueContext {
            trust: &f.trust,
            attestation: &f.verifier,
            nonces: &mut f.nonces,
            log: &mut log,
        },
    )
    .unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    let d = authorise(
        &c,
        &a,
        &p,
        100,
        &mut f.state,
        &mut UseContext {
            agent: &f.agent,
            authoriser: &f.authoriser,
            hard_floor: &spot_agent_tiers::Context::change(),
            policy: &f.policy,
            revocations: &mut f.revocations,
            log: &mut log,
        },
    );
    assert!(d.allowed);
    let cp = log.checkpoint(100).unwrap();
    assert_eq!(cp.seq, 4);
    let session: String = c.session_id().iter().map(|b| format!("{b:02x}")).collect();
    let report = log.customer_view(&session, &keys, &cp).unwrap();
    assert!(report.verified);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("SUP-42") && l.contains("window"))
    );
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("read") && l.contains("deep_access.allow"))
    );
    for tamper in 1..=3 {
        observable.tamper.store(tamper, Ordering::SeqCst);
        let report = log.customer_view(&session, &keys, &cp).unwrap();
        assert!(
            !report.verified,
            "tamper mode {tamper} must fail verification"
        );
        assert!(report.lines.last().unwrap().contains("verified: no"));
    }
}
struct FailedLog;
impl SessionLog for FailedLog {
    fn tenant(&self) -> &str {
        "tenant-a"
    }
    fn append(&mut self, _: DeepAccessEvent) -> Result<(), DeepError> {
        Err(DeepError::AuditUnavailable)
    }
}
#[test]
fn audit_outage_never_issues_or_authorises_and_revokes_existing_access() {
    let mut f = Fixture::new();
    let mut bad = FailedLog;
    let result = issue(
        &f.request,
        Some(&f.receipt),
        f.operator.verifying_key().to_bytes(),
        Some(&f.attestation),
        &f.policy,
        100,
        [42; 48],
        &mut IssueContext {
            trust: &f.trust,
            attestation: &f.verifier,
            nonces: &mut f.nonces,
            log: &mut bad,
        },
    );
    assert_eq!(result.unwrap_err(), DeepError::AuditUnavailable);
    let c = f.issue().unwrap();
    let a = f.action();
    let p = f.proof(&c, &a, 1);
    let result = authorise(
        &c,
        &a,
        &p,
        100,
        &mut f.state,
        &mut UseContext {
            agent: &f.agent,
            authoriser: &f.authoriser,
            hard_floor: &spot_agent_tiers::Context::change(),
            policy: &f.policy,
            revocations: &mut f.revocations,
            log: &mut bad,
        },
    );
    assert!(!result.allowed);
    assert_eq!(result.refusal, Some(DeepError::AuditUnavailable));
    assert!(
        f.revocations
            .lookup(c.tenant(), c.session_id())
            .unwrap()
            .is_some()
    );
}
#[test]
fn every_failure_stage_writes_one_decision_event() {
    for stage in 0..9 {
        let mut f = Fixture::new();
        let c = f.issue().unwrap();
        let mut a = f.action();
        let mut p = f.proof(&c, &a, 1);
        let mut now = 100;
        match stage {
            0 => {
                a.action = "unknown".into();
            }
            1 => {
                a.tenant = "another".into();
            }
            2 => {
                a.doctype = Some("Payroll".into());
            }
            3 => {
                f.agent = f.agent.with_status(spot_agent_tiers::Status::Suspended);
            }
            4 => {
                f.policy.protected_actions.insert("read".into());
            }
            5 => {
                p.signature.clear();
            }
            6 => {
                now = c.not_after();
            }
            7 => {
                now = c.not_before() - 1;
            }
            8 => {
                revoke(
                    &c,
                    "customer-owner",
                    "stop",
                    now,
                    &mut f.revocations,
                    &mut f.log,
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = f.log.events.len();
        let d = f.use_at(&c, &a, &p, now);
        assert!(!d.allowed);
        assert_eq!(f.log.events.len(), before + 1);
        assert_eq!(f.log.events.last().unwrap().code, d.code());
    }
}
