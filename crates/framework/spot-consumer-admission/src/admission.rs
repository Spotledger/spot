//! [`ConsumerAdmission`]: the single entry point both consumer front doors call before work.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use spot_coordinator::{BoxFuture, Coordinator, RateLimit};
use spot_throttle_coordinator::{
    Attempt, CoordinatorThrottle, Kind, MAX_TENANT_BYTES, SourceAddr, ThrottlePolicy, Verdict,
};

use crate::account::fold_account;
use crate::destination::{CanonicalDestination, Destination, canonicalise};
use crate::error::AdmissionError;
use crate::policy::{AdmissionPolicy, Purpose};

/// The capacity of the filler buckets of the resend-cooldown throttle: high enough that a source
/// or tenant ceiling there can never bind (one million units a second), so the only bucket that
/// says anything is the one-unit-per-destination cooldown.
const COOLDOWN_FILLER_CAPACITY: u64 = 1_000_000;

/// The prefix of the synthetic tenant the resend cooldown is counted under.
///
/// The throttle binds every digest to the tenant, so a distinct tenant gives a distinct key: the
/// one-unit cooldown bucket can never overwrite the caller's own per-destination bucket (which
/// has a real capacity and window), and the cooldown's filler source and tenant buckets cannot
/// touch the real ones either.
const COOLDOWN_TENANT_PREFIX: &str = "cd~";

/// One attempt, as the front door received it.
///
/// `account` is passed **as typed whether or not the account exists**: this crate never looks at
/// a user table, so a known and an unknown name take the same path at the same cost and produce
/// the same answer.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    /// What is being attempted.
    pub purpose: Purpose,
    /// The tenant; part of every bucket key.
    pub tenant: &'a str,
    /// The account name as typed, known or not.
    pub account: Option<&'a str>,
    /// The e-mail or phone a code would go to, as typed.
    pub destination: Option<Destination<'a>>,
    /// Where the attempt came from.
    pub source: SourceAddr<'a>,
}

impl fmt::Debug for Request<'_> {
    /// Names the fields and redacts every value a person supplied.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("purpose", &self.purpose)
            .field("tenant", &self.tenant)
            .field("account", &self.account.map(|_| "<redacted>"))
            .field("destination", &self.destination)
            .field("source", &"<redacted>")
            .finish()
    }
}

/// The answer: the attempt may go ahead, and this is where the code goes.
///
/// It carries the **folded** account and the **canonical** destination and nothing else: the raw forms never leave
/// [`ConsumerAdmission::admit`], and `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Admission {
    account: Option<String>,
    destination: Option<CanonicalDestination>,
}

impl Admission {
    /// The folded account ([`fold_account`]): the **only** spelling to look the user up by, so
    /// the attempt that was counted and the account that is checked are the same name.
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    /// The canonical destination the caller should send to or look the account up by.
    pub fn destination(&self) -> Option<&CanonicalDestination> {
        self.destination.as_ref()
    }
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("account", &self.account.as_ref().map(|_| "<redacted>"))
            .field("destination", &self.destination)
            .finish()
    }
}

/// The admission layer over one shared throttle.
///
/// Build it once per process and hand the same value to every handler: it holds the throttle
/// (and therefore the salt ring) and the policy, and no state of its own.
pub struct ConsumerAdmission {
    throttle: Arc<CoordinatorThrottle>,
    cooldown: Option<CoordinatorThrottle>,
    policy: AdmissionPolicy,
}

impl fmt::Debug for ConsumerAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsumerAdmission")
            .field("policy", &self.policy)
            .field("throttle", &self.throttle)
            .field("resend_cooldown_armed", &self.cooldown.is_some())
            .finish()
    }
}

impl ConsumerAdmission {
    /// An admission layer over `throttle` with **no** resend cooldown.
    ///
    /// A policy that arms a cooldown needs the coordinator to build its one-unit bucket, so it is
    /// refused here with `bad_policy`: use [`ConsumerAdmission::with_coordinator`] instead. The
    /// refusal is loud on purpose — a cooldown that is silently off is a code-pumping hole.
    pub fn new(
        throttle: Arc<CoordinatorThrottle>,
        policy: AdmissionPolicy,
    ) -> Result<Self, AdmissionError> {
        if policy.resend_cooldown().is_some() {
            return Err(AdmissionError::BadPolicy {
                reason: "a resend cooldown needs the coordinator: build with with_coordinator",
            });
        }
        Ok(Self {
            throttle,
            cooldown: None,
            policy,
        })
    }

    /// An admission layer over `throttle` whose resend cooldown is counted on `coordinator`.
    ///
    /// `coordinator` must be the same store the throttle counts on (and the throttle's ring, so
    /// the cooldown survives a restart and is shared by every node). The cooldown is one bucket
    /// per canonical destination holding one unit, refilled over `resend_cooldown`: the first
    /// code takes the unit, the next one inside the window is refused with the time left.
    pub fn with_coordinator(
        throttle: Arc<CoordinatorThrottle>,
        coordinator: Arc<dyn Coordinator>,
        policy: AdmissionPolicy,
    ) -> Result<Self, AdmissionError> {
        let cooldown = match policy.resend_cooldown() {
            None => None,
            Some(window) => {
                let base = throttle.policy();
                let filler = RateLimit {
                    capacity: COOLDOWN_FILLER_CAPACITY,
                    per: Duration::from_secs(1),
                };
                let one_per_window = RateLimit {
                    capacity: 1,
                    per: window,
                };
                let limits =
                    ThrottlePolicy::new(base.per_account(), filler, one_per_window, filler)
                        .map_err(|_| AdmissionError::BadPolicy {
                            reason: "the resend cooldown could not be given a throttle policy",
                        })?;
                Some(CoordinatorThrottle::with_ring(
                    coordinator,
                    throttle.ring().clone(),
                    limits,
                ))
            }
        };
        Ok(Self {
            throttle,
            cooldown,
            policy,
        })
    }

    /// The policy this layer applies.
    pub fn policy(&self) -> &AdmissionPolicy {
        &self.policy
    }

    /// Canonicalises the destination, checks the resend cooldown, prices the attempt and counts
    /// it on the throttle; all of it before the caller touches a password, a code or a user
    /// table.
    ///
    /// `Err` always means **refuse**: a coordinator that could not count
    /// ([`AdmissionError::Unavailable`]) is never a quiet allow.
    pub fn admit<'a>(
        &'a self,
        request: &'a Request<'a>,
    ) -> BoxFuture<'a, Result<Admission, AdmissionError>> {
        Box::pin(async move {
            let tenant = request.tenant;
            if !plain_tenant(tenant) || tenant.starts_with(COOLDOWN_TENANT_PREFIX) {
                // The prefix is reserved: a real tenant named `cd~x` would share the cooldown
                // key of tenant `x` and could spend or read its resend cooldown.
                return Err(AdmissionError::BadRequest {
                    reason: "the tenant must be 1 to 64 printable ASCII bytes without a colon and must not start with cd~",
                });
            }

            // The account is counted, never inspected: a blank one is a caller bug, not a
            // destination to guess at.
            let account = match request.account {
                Some(value) => Some(fold_account(value)?),
                None => None,
            };

            // Canonicalise first: nothing is counted under a spelling the bucket does not share.
            let canonical = match request.destination {
                Some(destination) => {
                    Some(canonicalise(destination, self.policy.strip_plus_tags())?)
                }
                None => None,
            };

            // What this purpose needs. A missing one is a caller bug, refused before any count.
            match (request.purpose, &canonical, account.as_deref()) {
                (Purpose::SendCode, None, _) => {
                    return Err(AdmissionError::BadRequest {
                        reason: "a code request must name the destination it sends to",
                    });
                }
                (Purpose::Register, None, _) => {
                    return Err(AdmissionError::BadRequest {
                        reason: "a registration must name the destination it registers",
                    });
                }
                (Purpose::SignIn, _, None) => {
                    return Err(AdmissionError::BadRequest {
                        reason: "a sign-in must name the account it checks",
                    });
                }
                (Purpose::Recover, None, None) => {
                    return Err(AdmissionError::BadRequest {
                        reason: "a recovery must name an account or a destination",
                    });
                }
                _ => {}
            }

            // The resend cooldown, before any rate unit is spent: a refused resend must not
            // pre-spend the allowance of the destination it targets.
            if request.purpose == Purpose::SendCode
                && let Some(destination) = &canonical
                && let Some(cooldown) = &self.cooldown
            {
                let cooldown_tenant = cooldown_tenant(tenant)?;
                let attempt = Attempt {
                    kind: Kind::CodeRequest,
                    account: None,
                    source: request.source,
                    destination: Some(destination.as_str()),
                    tenant: &cooldown_tenant,
                };
                match cooldown
                    .admit(&attempt)
                    .await
                    .map_err(AdmissionError::from_throttle)?
                {
                    Verdict::Allow => {}
                    Verdict::Deny { retry_after } => {
                        return Err(AdmissionError::RateLimited { retry_after });
                    }
                }
            }

            let attempt = Attempt {
                kind: request.purpose.kind(),
                // A registration counts its destination, not an account name: a flood of
                // registrations must not deny the password check of a name it names.
                account: if request.purpose.charges_account() {
                    account.as_deref()
                } else {
                    None
                },
                source: request.source,
                destination: canonical.as_ref().map(CanonicalDestination::as_str),
                tenant,
            };

            // A code costs `code_cost` units of the destination bucket, so it is counted that many
            // times; every other purpose costs one unit of each bucket it names.
            let takes = match request.purpose {
                Purpose::SendCode => self.policy.code_cost(),
                _ => 1,
            };
            for _ in 0..takes {
                match self
                    .throttle
                    .admit(&attempt)
                    .await
                    .map_err(AdmissionError::from_throttle)?
                {
                    Verdict::Allow => {}
                    Verdict::Deny { retry_after } => {
                        return Err(AdmissionError::RateLimited { retry_after });
                    }
                }
            }

            Ok(Admission {
                account,
                destination: canonical,
            })
        })
    }
}

/// The tenant rule the throttle applies, checked here so a caller gets `bad_request` rather than
/// a throttle `bad_attempt`.
fn plain_tenant(tenant: &str) -> bool {
    !tenant.is_empty()
        && tenant.len() <= MAX_TENANT_BYTES
        && tenant.bytes().all(|b| b.is_ascii_graphic() && b != b':')
}

/// The synthetic tenant the resend cooldown is counted under: `cd~{tenant}`.
fn cooldown_tenant(tenant: &str) -> Result<String, AdmissionError> {
    let prefixed = format!("{COOLDOWN_TENANT_PREFIX}{tenant}");
    if !plain_tenant(&prefixed) {
        return Err(AdmissionError::BadRequest {
            reason: "the tenant is too long to carry a resend cooldown",
        });
    }
    Ok(prefixed)
}
