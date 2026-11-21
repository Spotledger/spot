//! The effect hold, engaged in `Held` mode for the whole replay.
//!
//! A replay re-executes real operations, so it may reach a point where the old release would have
//! sent an email, called a bank API or delivered a webhook. **Nothing may leave the clone.** Every
//! such call passes through [`EffectHold::guard`], which delegates to the platform's
//! [`Gate`](spot_effect_hold::Gate) (WP-MIG-08.T1) while the site is held: a held-class effect is
//! recorded in the durable ledger and deferred, never dispatched.
//!
//! [`EffectHold::engage`] refuses to build a hold unless the site is genuinely held, so an unheld
//! replay cannot start. [`EffectHold::guard`] turns the one case the gate would let through — a
//! never-held class such as a fiscal transmission (ADR-43) — into an error
//! ([`Error::EffectEscaped`](crate::Error::EffectEscaped)): during a replay there is no issued
//! document to transmit, so even a fiscal effect must not leave.

use std::sync::Arc;

use spot_core::SiteId;
use spot_effect_hold::{Admission, EffectClass, EffectRef, Gate, HoldSnapshot};

use crate::error::{Error, Result};

/// The side-effect hold for one replay, in `Held` mode.
pub struct EffectHold {
    gate: Arc<Gate>,
    site: SiteId,
}

impl EffectHold {
    /// Engages the hold for `site` over `gate`, stamping the snapshot with `now` (Unix
    /// microseconds, supplied by the caller).
    ///
    /// # Errors
    /// [`Error::NotHeld`] when the site is open or its state is unknown: a replay must never run
    /// while an effect could leave.
    pub async fn engage(gate: Arc<Gate>, site: SiteId, now: i64) -> Result<EffectHold> {
        gate.load(now).await;
        match gate.snapshot() {
            HoldSnapshot::Held { .. } => Ok(EffectHold { gate, site }),
            HoldSnapshot::Open => Err(Error::NotHeld {
                site: site.to_string(),
                state: "open".to_owned(),
            }),
            HoldSnapshot::Unknown => Err(Error::NotHeld {
                site: site.to_string(),
                state: "unknown".to_owned(),
            }),
        }
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The gate, for a caller that wants the underlying hold.
    #[must_use]
    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// An effect reference for this site, with the class' key as its ordering source.
    #[must_use]
    pub fn effect(
        &self,
        class: EffectClass,
        kind: impl Into<String>,
        key: impl Into<String>,
        created_at: i64,
    ) -> EffectRef {
        EffectRef::new(self.site.clone(), class, kind, key, created_at)
    }

    /// Guards one external effect immediately before the irrevocable act.
    ///
    /// A held-class effect is recorded and returned as [`Admission::Held`]; a never-held class —
    /// the one case the gate would admit — is refused with
    /// [`Error::EffectEscaped`](crate::Error::EffectEscaped), so nothing leaves during a replay.
    ///
    /// # Errors
    /// [`Error::EffectEscaped`] for a never-held class.
    pub async fn guard(&self, effect: &EffectRef, now: i64) -> Result<Admission> {
        match self.gate.admit(effect, now).await {
            Admission::Held(ticket) => Ok(Admission::Held(ticket)),
            Admission::Proceed => Err(Error::EffectEscaped {
                key: effect.key.clone(),
                class: effect.class.key(),
            }),
        }
    }
}

impl std::fmt::Debug for EffectHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectHold")
            .field("site", &self.site)
            .field("state", &self.gate.snapshot())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_effect_hold::{HoldState, InMemoryLedger, LedgerStore, MigrationId, PolicyTable};

    async fn held_gate() -> (Arc<Gate>, SiteId) {
        let site = SiteId::new("acme").expect("site");
        let ledger = Arc::new(InMemoryLedger::new());
        ledger
            .compare_and_set_hold(
                &site,
                &HoldState::Open,
                HoldState::Held {
                    migration_id: MigrationId::new("mig-1").expect("migration"),
                    reason: "rehearsal".to_owned(),
                    since: 0,
                    expected_by: None,
                },
            )
            .await
            .expect("hold");
        let gate = Arc::new(Gate::new(
            site.clone(),
            ledger as Arc<dyn LedgerStore>,
            Arc::new(PolicyTable::default()),
        ));
        (gate, site)
    }

    #[tokio::test]
    async fn engage_refuses_an_open_site() {
        let site = SiteId::new("acme").expect("site");
        let ledger = Arc::new(InMemoryLedger::new());
        let gate = Arc::new(Gate::new(
            site.clone(),
            ledger as Arc<dyn LedgerStore>,
            Arc::new(PolicyTable::default()),
        ));
        let err = EffectHold::engage(gate, site, 0).await.unwrap_err();
        assert_eq!(err.code(), crate::codes::NOT_HELD);
    }

    #[tokio::test]
    async fn a_held_class_is_held_and_a_fiscal_effect_is_refused() {
        let (gate, site) = held_gate().await;
        let hold = EffectHold::engage(gate, site, 0).await.expect("held");
        let mail = hold.effect(EffectClass::Mail, "invoice mail", "k1", 0);
        assert!(matches!(
            hold.guard(&mail, 0).await.expect("held"),
            Admission::Held(_)
        ));
        let fiscal = hold.effect(EffectClass::FiscalTransmission, "fiscal", "k2", 0);
        let err = hold.guard(&fiscal, 0).await.unwrap_err();
        assert_eq!(err.code(), crate::codes::EFFECT_ESCAPED);
    }
}
