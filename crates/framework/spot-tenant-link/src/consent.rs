//! Consent: one party administrator's signed approval of a link, with a step-up proof.
//!
//! ADR-058 rule 3 requires both tenant administrators to consent. A one-click approve is a UI
//! matter: the two [`Consent`] records still exist, each naming the party, the actor, the instant
//! and the strong sign-in it rests on. The proof is checked through [`StepUpGate`], whose
//! shipping adapter [`PolicyStepUp`] applies the tenant's own `spot-iam-access` step-up policy —
//! this crate invents neither a second policy nor a second notion of "recent".
//!
//! A consent that reaches a peer is verified for shape only: the peer has the sender's signature
//! on it, and cannot see the sender's IAM. The step-up check is therefore always the local
//! signer's, performed before the message is signed.
//!
//! Creating a consent is not the only moment it is checked. [`Consent::check_use`] runs whenever
//! a consent is **used** — in `propose`, in `from_proposal`, when a handshake accepts and when an
//! acceptance arrives — and re-checks its shape, refuses a consent older than the caller's
//! [`ConsentPolicy`] and confirms the proof again through the [`StepUpGate`]. A consent that was
//! strong when it was recorded does not stay usable forever.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};

use crate::error::{Error, Result};
use crate::model::Party;
use crate::util::{put_field, put_i64, put_u64};

/// The step-up operation name a link consent is recorded under, in the
/// `spot-iam-access` snake-case vocabulary.
pub const CONSENT_OPERATION: &str = "link_consent";

/// How old a consent may be when a handshake uses it.
///
/// A proposal may sit in a mailbox, and an acceptance may be delayed: the consent that approved
/// it must still be recent when it is used, not only when it was recorded. [`Default`] is
/// fifteen minutes, the ceiling ADR-058's step-up wording assumes for an administrator's approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsentPolicy {
    max_age_ms: u64,
}

impl ConsentPolicy {
    /// The default maximum age: fifteen minutes.
    pub const DEFAULT_MAX_AGE_MS: u64 = 15 * 60 * 1_000;

    /// A policy with the given maximum age.
    #[must_use]
    pub fn new(max_age_ms: u64) -> Self {
        Self { max_age_ms }
    }

    /// The maximum age in Unix milliseconds.
    #[must_use]
    pub fn max_age_ms(self) -> u64 {
        self.max_age_ms
    }
}

impl Default for ConsentPolicy {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_AGE_MS)
    }
}

/// What the consent rests on: a strong sign-in that is recent enough for the tenant's policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepUpProof {
    /// The operation that asked for step-up (`link_consent`).
    pub operation: OperationName,
    /// How the session was established.
    pub method: AuthMethod,
    /// When the strong sign-in happened, in seconds since the Unix epoch.
    pub authenticated_at: i64,
}

/// Checks a consent's step-up proof.
///
/// The adapter over the tenant's IAM implements this; [`PolicyStepUp`] is the shipped one.
pub trait StepUpGate {
    /// Confirms the proof at `at` (Unix milliseconds).
    ///
    /// # Errors
    /// [`Error::StepUpRequired`] when the sign-in is absent, stale or too weak;
    /// [`Error::ConsentInvalid`] when the proof names another operation.
    fn confirm(&self, actor: &str, proof: &StepUpProof, at: u64) -> Result<()>;
}

/// The shipping gate: the tenant's `spot-iam-access` policy, with a method floor.
///
/// The policy's recent-sign-in age is applied even when the tenant has not listed
/// `link_consent`: consent always needs a fresh strong sign-in, and adding the operation to the
/// policy can only make the check stricter in the tenant's own vocabulary.
pub struct PolicyStepUp {
    policy: StepUpPolicy,
    minimum: AuthMethod,
}

impl PolicyStepUp {
    /// A gate over `policy`, requiring at least a password plus a time-based code.
    #[must_use]
    pub fn new(policy: StepUpPolicy) -> Self {
        Self {
            policy,
            minimum: AuthMethod::PasswordTotp,
        }
    }

    /// The same gate with another method floor.
    #[must_use]
    pub fn with_minimum(mut self, minimum: AuthMethod) -> Self {
        self.minimum = minimum;
        self
    }
}

impl fmt::Debug for PolicyStepUp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyStepUp")
            .field("minimum", &self.minimum)
            .field("max_age_seconds", &self.policy.max_age_seconds())
            .finish()
    }
}

impl StepUpGate for PolicyStepUp {
    fn confirm(&self, actor: &str, proof: &StepUpProof, at: u64) -> Result<()> {
        if proof.operation.as_str() != CONSENT_OPERATION {
            return Err(Error::ConsentInvalid {
                reason: "the step-up proof names the consent operation",
            });
        }
        let now = i64::try_from(at / 1_000).map_err(|_| Error::ConsentInvalid {
            reason: "the consent instant is out of range",
        })?;
        let last = proof.authenticated_at;
        let stale = last > now
            || now.saturating_sub(last) > self.policy.max_age_seconds()
            || proof.method.is_weaker_than(self.minimum);
        if stale {
            return Err(Error::StepUpRequired {
                actor: actor.to_owned(),
            });
        }
        // The tenant policy may add the operation; where it does, its own answer is final.
        if self
            .policy
            .operations()
            .contains(&OperationName::new(CONSENT_OPERATION))
        {
            self.policy
                .require(&proof.operation, last, now)
                .map_err(|_| Error::StepUpRequired {
                    actor: actor.to_owned(),
                })?;
        }
        Ok(())
    }
}

/// One party administrator's consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    /// The party consenting.
    pub party: Party,
    /// The administrator (`user:<id>` or `agent:<id>`).
    pub actor: String,
    /// Unix milliseconds.
    pub at: u64,
    /// The step-up proof the local side checked before signing.
    pub proof: StepUpProof,
}

impl Consent {
    /// Records a consent after checking its proof through `gate`.
    ///
    /// # Errors
    /// [`Error::ConsentInvalid`] for a malformed actor, operation or instant, and
    /// [`Error::StepUpRequired`] when the gate refuses the proof.
    pub fn new(
        party: Party,
        actor: impl Into<String>,
        at: u64,
        proof: StepUpProof,
        gate: &dyn StepUpGate,
    ) -> Result<Self> {
        let consent = Self {
            party,
            actor: actor.into(),
            at,
            proof,
        };
        consent.check_shape(at)?;
        gate.confirm(&consent.actor, &consent.proof, at)?;
        Ok(consent)
    }

    /// The shape a peer can check without the sender's IAM.
    ///
    /// # Errors
    /// [`Error::ConsentInvalid`] when the actor, operation or instants are wrong or in the
    /// future of `at`; [`Error::StepUpRequired`] when the proof is password-only.
    pub fn check_shape(&self, at: u64) -> Result<()> {
        if !actor_ok(&self.actor) {
            return Err(Error::ConsentInvalid {
                reason: "a consent is recorded by an administrator, never by the system",
            });
        }
        if self.proof.operation.as_str() != CONSENT_OPERATION {
            return Err(Error::ConsentInvalid {
                reason: "the step-up proof names the consent operation",
            });
        }
        if self.proof.method == AuthMethod::Password {
            return Err(Error::StepUpRequired {
                actor: self.actor.clone(),
            });
        }
        let now = i64::try_from(at / 1_000).map_err(|_| Error::ConsentInvalid {
            reason: "the consent instant is out of range",
        })?;
        if self.at > at || self.proof.authenticated_at > now {
            return Err(Error::ConsentInvalid {
                reason: "a consent is not dated in the future",
            });
        }
        Ok(())
    }

    /// Checks a consent at the moment a handshake **uses** it, not only when it was recorded.
    ///
    /// Runs [`Consent::check_shape`], refuses an age above `policy`'s maximum with
    /// [`Error::ConsentStale`], and confirms the proof again through `gate` — so a consent whose
    /// strong sign-in has since lapsed is refused even though it verified when it was created.
    ///
    /// # Errors
    /// [`Error::ConsentInvalid`], [`Error::StepUpRequired`] or [`Error::ConsentStale`].
    pub fn check_use(&self, at: u64, policy: ConsentPolicy, gate: &dyn StepUpGate) -> Result<()> {
        self.check_shape(at)?;
        if at.saturating_sub(self.at) > policy.max_age_ms {
            return Err(Error::ConsentStale {
                actor: self.actor.clone(),
                max_age_ms: policy.max_age_ms,
            });
        }
        gate.confirm(&self.actor, &self.proof, at)
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        self.party.encode(out);
        put_field(out, self.actor.as_bytes());
        put_u64(out, self.at);
        put_field(out, self.proof.operation.as_str().as_bytes());
        out.push(method_tag(self.proof.method));
        put_i64(out, self.proof.authenticated_at);
    }
}

fn method_tag(method: AuthMethod) -> u8 {
    match method {
        AuthMethod::Password => 0,
        AuthMethod::PasswordTotp => 1,
        AuthMethod::Sso => 2,
        AuthMethod::Hardware => 3,
    }
}

/// Whether `actor` is an administrator. The system is never an actor of a consent: a machine
/// cannot stand in for a tenant administrator's approval.
fn actor_ok(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= 256
        && actor
            .strip_prefix("user:")
            .or_else(|| actor.strip_prefix("agent:"))
            .is_some_and(|id| !id.is_empty() && !id.contains(char::is_whitespace))
}

/// The two consents of a link must come from different parties and different actors.
///
/// # Errors
/// [`Error::ConsentDuplicate`] when the same actor would stand for both sides.
pub fn require_distinct(first: &Consent, second: &Consent) -> Result<()> {
    if first.party == second.party || first.actor == second.actor {
        return Err(Error::ConsentDuplicate {
            actor: if first.actor == second.actor {
                first.actor.clone()
            } else {
                second.actor.clone()
            },
        });
    }
    Ok(())
}
