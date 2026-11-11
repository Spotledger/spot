//! The hub side of the exchange.

use spot_sync_policy::preset::AdoptMode;

use crate::{AdoptionError, RecordState, Version};

/// What the hub sees, in stream order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubInput {
    /// A version of the record written by the creating node arrived (re-delivery is harmless).
    RecordArrived {
        /// The version that arrived.
        version: Version,
    },
    /// The `adopt` setting of the DocType (or its default) is set or changed.
    AdoptMode(AdoptMode),
    /// A business-process step "adopt this record" fired (ADR-063 amendment 1).
    ProcessStepFired,
    /// An administrator asked for the adoption by hand (the `manual` mode's other trigger).
    AdminAdopt,
    /// The acknowledgement of the creating node arrived, carrying its final version.
    AckReceived {
        /// The last version the node wrote before it stopped owning.
        final_version: Version,
    },
    /// The creating node is offline.
    NodeOffline,
}

/// What the hub does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubOutput {
    /// Write the adoption request naming the version the hub has seen.
    WriteRequest {
        /// The highest version of the record the hub has seen.
        seen_version: Version,
    },
    /// The hub is now the owner.
    TakeOwnership,
    /// Write the adopted version, the first hub-owned one (the node's final version plus one).
    WriteAdopted {
        /// The adopted version.
        version: Version,
    },
    /// Raise the `AdoptionPending` condition (amber; data only, never blocks an edit).
    RaiseCondition,
    /// Clear the `AdoptionPending` condition.
    ClearCondition,
    /// The input was refused; the state is unchanged.
    Rejected(AdoptionError),
}

/// The hub's view of one record's adoption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HubSide {
    state: RecordState,
    mode: AdoptMode,
    seen: Option<Version>,
    adopted_final: Option<Version>,
    condition_raised: bool,
    node_offline: bool,
}

impl HubSide {
    /// A hub that has not seen the record, with the DocType's `adopt` setting.
    #[must_use]
    pub fn new(mode: AdoptMode) -> Self {
        Self {
            state: RecordState::Provisional,
            mode,
            seen: None,
            adopted_final: None,
            condition_raised: false,
            node_offline: false,
        }
    }

    /// The record's adoption state.
    #[must_use]
    pub fn state(&self) -> RecordState {
        self.state
    }

    /// The current `adopt` setting.
    #[must_use]
    pub fn mode(&self) -> AdoptMode {
        self.mode
    }

    /// The highest version of the node's writes the hub has seen.
    #[must_use]
    pub fn seen_version(&self) -> Option<Version> {
        self.seen
    }

    /// Whether the hub owns the record.
    #[must_use]
    pub fn owns(&self) -> bool {
        self.state == RecordState::Adopted
    }

    /// Whether the `AdoptionPending` condition is raised.
    #[must_use]
    pub fn condition_raised(&self) -> bool {
        self.condition_raised
    }

    /// Takes one input; returns the next state and the outputs. A refused input returns the
    /// state unchanged with one [`HubOutput::Rejected`].
    #[must_use]
    pub fn step(self, input: HubInput) -> (Self, Vec<HubOutput>) {
        let mut next = self;
        let mut out = Vec::new();
        match input {
            HubInput::RecordArrived { version } => {
                if !next.owns() {
                    next.seen = Some(next.seen.map_or(version, |s| s.max(version)));
                    next.on_trigger_point(next.mode == AdoptMode::Auto, &mut out);
                }
            }
            HubInput::AdoptMode(mode) => {
                next.mode = mode;
                next.on_trigger_point(mode == AdoptMode::Auto, &mut out);
            }
            HubInput::ProcessStepFired | HubInput::AdminAdopt => {
                if next.state == RecordState::Provisional && next.mode != AdoptMode::Never {
                    if next.seen.is_none() {
                        out.push(HubOutput::Rejected(AdoptionError::RecordNotArrived));
                    } else {
                        next.on_trigger_point(true, &mut out);
                    }
                }
            }
            HubInput::NodeOffline => {
                next.node_offline = true;
                next.raise_if_pending(&mut out);
            }
            HubInput::AckReceived { final_version } => next.on_ack(final_version, &mut out),
        }
        (next, out)
    }

    /// `request` says whether the trigger that brought us here asks for the request now.
    /// `never` settles the record as never adopted; nothing happens before the record arrived.
    fn on_trigger_point(&mut self, request: bool, out: &mut Vec<HubOutput>) {
        if self.state != RecordState::Provisional {
            return;
        }
        let Some(seen) = self.seen else { return };
        if self.mode == AdoptMode::Never {
            self.state = RecordState::NeverAdopted;
        } else if request {
            self.state = RecordState::AdoptionPending {
                requested_version: seen,
            };
            out.push(HubOutput::WriteRequest { seen_version: seen });
            self.raise_if_pending(out);
        }
    }

    fn raise_if_pending(&mut self, out: &mut Vec<HubOutput>) {
        let pending = matches!(self.state, RecordState::AdoptionPending { .. });
        if pending && self.node_offline && !self.condition_raised {
            self.condition_raised = true;
            out.push(HubOutput::RaiseCondition);
        }
    }

    fn on_ack(&mut self, final_version: Version, out: &mut Vec<HubOutput>) {
        match self.state {
            RecordState::Adopted => {
                if self.adopted_final != Some(final_version) {
                    out.push(HubOutput::Rejected(AdoptionError::AckMismatch));
                }
            }
            RecordState::Provisional | RecordState::NeverAdopted => {
                out.push(HubOutput::Rejected(AdoptionError::NoRequest));
            }
            RecordState::AdoptionPending { .. } => {
                let seen = self.seen.unwrap_or(0);
                if final_version < seen {
                    out.push(HubOutput::Rejected(AdoptionError::StaleAck {
                        final_version,
                        seen_version: seen,
                    }));
                } else if final_version > seen {
                    out.push(HubOutput::Rejected(AdoptionError::MissingVersions {
                        final_version,
                        seen_version: seen,
                    }));
                } else if let Some(adopted) = final_version.checked_add(1) {
                    self.state = RecordState::Adopted;
                    self.adopted_final = Some(final_version);
                    out.push(HubOutput::TakeOwnership);
                    out.push(HubOutput::WriteAdopted { version: adopted });
                    if self.condition_raised {
                        self.condition_raised = false;
                        out.push(HubOutput::ClearCondition);
                    }
                } else {
                    out.push(HubOutput::Rejected(AdoptionError::VersionExhausted));
                }
            }
        }
    }
}
