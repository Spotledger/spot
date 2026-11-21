//! Cancellation under load (WP-AI-05.T1): 10,000 random cancellations leak no permit.
//!
//! Every admitted call is either cancelled (its statement cancelled through a
//! [`StatementCanceller`]) or completed; whichever way, the permit is released. After each of
//! the 10,000 iterations the gate must report no call in flight.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_ai_isolation::Outcome;
use spot_ai_isolation::admission::{AiAdmission, AiRequest};
use spot_ai_isolation::cancel::RecordingCanceller;
use spot_ai_isolation::config::{AiLimits, AiPoolSettings};
use spot_ai_isolation::identity::{AgentId, TenantId};
use spot_ai_isolation::load::StaticLoad;

/// A deterministic random source, so the 10,000-cancellation run reproduces exactly.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

#[test]
fn ten_thousand_random_cancellations_leak_no_permit() {
    let limits = AiLimits {
        per_agent_rate: spot_coordinator::RateLimit {
            capacity: 1_000_000,
            per: Duration::from_secs(60),
        },
        per_tenant_rate: spot_coordinator::RateLimit {
            capacity: 1_000_000,
            per: Duration::from_secs(60),
        },
        ..AiLimits::default()
    };
    let pool = AiPoolSettings {
        pool_size: 8,
        ..AiPoolSettings::default()
    };
    let gate = Arc::new(
        AiAdmission::new(limits, pool, Arc::new(StaticLoad::calm()))
            .expect("a valid configuration"),
    );
    let canceller = RecordingCanceller::new();
    let tenant = TenantId::new("acme").expect("a valid tenant");
    let mut rng = Lcg(0x1234_5678_9abc_def0);
    let mut cancelled: u32 = 0;
    let mut completed: u32 = 0;

    for _ in 0..10_000 {
        let agent = AgentId::new(format!("agent-{}", rng.next() % 4)).expect("a valid agent");
        let request = AiRequest::new(agent, tenant.clone());
        if let Ok(permit) = gate.admit(&request, Duration::ZERO) {
            if rng.next().is_multiple_of(2) {
                let error = permit.cancel(&canceller);
                assert_eq!(error.code, spot_ai_isolation::error::CANCELLED);
                cancelled += 1;
            } else {
                permit.complete(Duration::ZERO, Outcome::Success);
                completed += 1;
            }
        }
        assert_eq!(gate.in_flight(), 0, "a permit leaked");
    }

    assert_eq!(cancelled + completed, 10_000, "every call was resolved");
    assert_eq!(canceller.count(), cancelled as usize);
    assert_eq!(gate.in_flight(), 0);
    assert!(cancelled > 0 && completed > 0);
}
