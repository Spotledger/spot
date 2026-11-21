//! Subscription: the `ModelHandle` agents hold, and the epoch stamp every action carries.
//!
//! An agent does not own the deployment model; it holds a cheap clone of a [`ModelHandle`].
//! The handle publishes through a `tokio::sync::watch` channel, so:
//!
//! - [`ModelHandle::current`] is a lock-free read of the newest model;
//! - [`ModelHandle::subscribe`] gives a receiver that wakes on every new epoch, and the agent
//!   decides with the epoch it read rather than with an old copy;
//! - every clone shares the same channel and the same pending-event buffer.
//!
//! # Gaps and refresh
//!
//! An event that skips epochs is buffered, not applied: the model must not claim the deployment
//! changed when it cannot know what happened in between. The handle reports the gap, the caller
//! refreshes from its [`FactSource`]s, and anything the refresh already covers is dropped. A
//! periodic [`ModelHandle::self_check`] verifies the model against the sources and refreshes on
//! the first mismatch, so drift is repaired without waiting for an event.
//!
//! # Auditing the epoch a decision relied on
//!
//! [`EpochStamp`] ties an action to the exact model version and facts it saw.
//! [`EpochStamp::attach`] writes it into an audit record's payload under
//! [`EPOCH_PAYLOAD_KEY`] and [`DIGEST_PAYLOAD_KEY`], so a later reviewer can prove which
//! deployment the agent believed it was acting on.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_audit_chain::{AuditRecord, PayloadValue};
use tokio::sync::watch;

use crate::DeploymentError;
use crate::event::{ApplyOutcome, DeploymentEvent, apply};
use crate::facts::{FactSource, Mismatch, SelfCheck, build, read, verify};
use crate::model::{DeploymentModel, Digest};

/// The most events that wait in the buffer for an earlier epoch to arrive.
const PENDING_LIMIT: usize = 64;

/// The audit payload key holding the epoch a decision relied on.
pub const EPOCH_PAYLOAD_KEY: &str = "deployment_epoch";

/// The audit payload key holding the digest of the facts the decision relied on.
pub const DIGEST_PAYLOAD_KEY: &str = "deployment_digest";

/// The epoch and facts an agent action relied on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpochStamp {
    /// The model epoch.
    pub epoch: u64,
    /// The digest of the facts at that epoch.
    pub digest: Digest,
}

impl EpochStamp {
    /// Stamps the model's current version.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Serialise`] when the model's facts cannot be rendered.
    pub fn of(model: &DeploymentModel) -> Result<Self, DeploymentError> {
        Ok(Self {
            epoch: model.epoch,
            digest: model.digest()?,
        })
    }

    /// Writes the stamp into an audit record's payload.
    ///
    /// The epoch is an integer and the digest text; neither is a secret, and an action's
    /// arguments are never touched.
    pub fn attach(&self, record: &mut AuditRecord) {
        let epoch = i64::try_from(self.epoch).unwrap_or(i64::MAX);
        record
            .payload
            .insert(EPOCH_PAYLOAD_KEY.to_owned(), PayloadValue::Int(epoch));
        record.payload.insert(
            DIGEST_PAYLOAD_KEY.to_owned(),
            PayloadValue::Text(self.digest.to_string()),
        );
    }
}

/// What [`ModelHandle::apply`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandleOutcome {
    /// The event (and any buffered events behind it) applied.
    Applied {
        /// The model's new epoch.
        epoch: u64,
    },
    /// The event skips epochs and waits in the buffer; refresh from the fact sources.
    Queued {
        /// The epoch the event names.
        epoch: u64,
        /// The epoch the model expected.
        expected: u64,
    },
    /// The event id was already applied.
    Duplicate {
        /// The model's epoch.
        epoch: u64,
    },
    /// The event is older than the model's next epoch.
    Stale {
        /// The model's epoch.
        model_epoch: u64,
        /// The ignored event's epoch.
        event_epoch: u64,
    },
    /// Another event already produced this epoch.
    Conflict {
        /// The epoch both events claim.
        epoch: u64,
        /// The id of the event that produced it.
        applied: String,
    },
}

impl HandleOutcome {
    /// Whether a refresh is needed before the model can move forward.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        matches!(self, Self::Queued { .. })
    }
}

/// A shareable handle to the deployment model.
///
/// Cloning is cheap: every clone shares the watch channel and the pending-event buffer, so an
/// agent can keep one in its context, hand another to a tool, and both see the same epochs.
#[derive(Clone, Debug)]
pub struct ModelHandle {
    sender: watch::Sender<Arc<DeploymentModel>>,
    pending: Arc<Mutex<std::collections::BTreeMap<u64, DeploymentEvent>>>,
    gate: Arc<Mutex<()>>,
}

impl ModelHandle {
    /// A handle publishing an initial model.
    #[must_use]
    pub fn new(model: DeploymentModel) -> Self {
        let (sender, _receiver) = watch::channel(Arc::new(model));
        Self {
            sender,
            pending: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            gate: Arc::new(Mutex::new(())),
        }
    }

    /// The newest model.
    #[must_use]
    pub fn current(&self) -> Arc<DeploymentModel> {
        self.sender.borrow().clone()
    }

    /// The newest model's epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.sender.borrow().epoch
    }

    /// A receiver that wakes whenever the model moves to a new epoch.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<DeploymentModel>> {
        self.sender.subscribe()
    }

    /// Applies an event, draining any buffered events it unblocks.
    ///
    /// # Errors
    ///
    /// Any error [`apply`] raises, or [`DeploymentError::EventInvalid`] when a buffered event
    /// cannot apply once its predecessors arrived.
    pub fn apply(&self, event: DeploymentEvent) -> Result<HandleOutcome, DeploymentError> {
        let _gate = lock(&self.gate);
        let mut pending = lock(&self.pending);
        if pending.values().any(|queued| queued.id == event.id) {
            return Ok(HandleOutcome::Duplicate {
                epoch: self.epoch(),
            });
        }
        let current = self.current();
        match apply(&current, &event)? {
            ApplyOutcome::Applied { model, .. } => {
                let epoch = self.drain(*model, &mut pending)?;
                Ok(HandleOutcome::Applied { epoch })
            }
            ApplyOutcome::Gap { expected, .. } => {
                if event.epoch > expected {
                    pending.retain(|epoch, _| *epoch >= expected);
                    pending.insert(event.epoch, event.clone());
                    // Bounded: a source that runs far ahead cannot grow the buffer without end.
                    // The farthest events go first; the caller has been told to refresh anyway.
                    while pending.len() > PENDING_LIMIT {
                        let _dropped = pending.pop_last();
                    }
                    Ok(HandleOutcome::Queued {
                        epoch: event.epoch,
                        expected,
                    })
                } else {
                    Ok(HandleOutcome::Stale {
                        model_epoch: current.epoch,
                        event_epoch: event.epoch,
                    })
                }
            }
            ApplyOutcome::Duplicate { epoch } => Ok(HandleOutcome::Duplicate { epoch }),
            ApplyOutcome::Stale {
                model_epoch,
                event_epoch,
            } => Ok(HandleOutcome::Stale {
                model_epoch,
                event_epoch,
            }),
            ApplyOutcome::Conflict { epoch, applied } => {
                Ok(HandleOutcome::Conflict { epoch, applied })
            }
        }
    }

    /// Applies every event in order, returning the last outcome.
    ///
    /// # Errors
    ///
    /// Any error [`ModelHandle::apply`] raises.
    pub fn apply_all(
        &self,
        events: impl IntoIterator<Item = DeploymentEvent>,
    ) -> Result<Option<HandleOutcome>, DeploymentError> {
        let mut last = None;
        for event in events {
            last = Some(self.apply(event)?);
        }
        Ok(last)
    }

    /// Rebuilds the model from the sources and publishes it.
    ///
    /// # Errors
    ///
    /// Any error [`crate::facts::refresh`] raises.
    pub fn refresh(&self, sources: &[&dyn FactSource], at: i64) -> Result<u64, DeploymentError> {
        let _gate = lock(&self.gate);
        let current = self.current();
        let model = crate::facts::refresh(Some(&current), sources, at)?;
        let epoch = model.epoch;
        self.publish(model);
        let mut pending = lock(&self.pending);
        pending.retain(|event_epoch, _| *event_epoch > epoch);
        Ok(epoch)
    }

    /// Compares the model with the sources, fact by fact.
    ///
    /// # Errors
    ///
    /// Any error [`read`] raises.
    pub fn verify(&self, sources: &[&dyn FactSource]) -> Result<Vec<Mismatch>, DeploymentError> {
        let current = self.current();
        let (facts, _) = read(sources)?;
        Ok(verify(&current, &facts))
    }

    /// Verifies the model and refreshes it when it disagrees with the sources.
    ///
    /// # Errors
    ///
    /// Any error [`read`] or [`build`] raises.
    pub fn self_check(
        &self,
        sources: &[&dyn FactSource],
        at: i64,
    ) -> Result<SelfCheck, DeploymentError> {
        let _gate = lock(&self.gate);
        let current = self.current();
        let (facts, versions) = read(sources)?;
        let mismatches = verify(&current, &facts);
        if mismatches.is_empty() {
            return Ok(SelfCheck {
                mismatches,
                refreshed_to: None,
            });
        }
        let evidence = sources.iter().find_map(|source| source.evidence());
        let model = build(Some(&current), &facts, versions, evidence, at)?;
        let epoch = model.epoch;
        self.publish(model);
        let mut pending = lock(&self.pending);
        pending.retain(|event_epoch, _| *event_epoch > epoch);
        Ok(SelfCheck {
            mismatches,
            refreshed_to: Some(epoch),
        })
    }

    /// Stamps the model's current version.
    ///
    /// # Errors
    ///
    /// [`DeploymentError::Serialise`] when the model's facts cannot be rendered.
    pub fn stamp(&self) -> Result<EpochStamp, DeploymentError> {
        EpochStamp::of(&self.current())
    }

    /// Applies the next buffered event, if any.
    fn drain(
        &self,
        model: DeploymentModel,
        pending: &mut std::collections::BTreeMap<u64, DeploymentEvent>,
    ) -> Result<u64, DeploymentError> {
        let mut latest = model;
        loop {
            let next_epoch = latest.epoch.saturating_add(1);
            let Some(next) = pending.remove(&next_epoch) else {
                break;
            };
            match apply(&latest, &next)? {
                ApplyOutcome::Applied { model, .. } => latest = *model,
                _ => break,
            }
        }
        let epoch = latest.epoch;
        // An epoch that is already behind the model can never apply; drop it rather than let
        // it sit in the buffer forever.
        pending.retain(|event_epoch, _| *event_epoch > epoch);
        self.publish(latest);
        Ok(epoch)
    }

    fn publish(&self, model: DeploymentModel) {
        let _previous = self.sender.send_replace(Arc::new(model));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
