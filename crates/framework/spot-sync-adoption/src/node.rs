//! The creating node's side of the exchange.

use crate::{AdoptionError, Version};

/// What the node sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeInput {
    /// The adoption request reached the node's stream and is applied.
    RequestApplied {
        /// The version the hub says it has seen.
        seen_version: Version,
    },
    /// A user edits the record at the node.
    LocalEdit,
    /// The acknowledgement is durably in the node's stream.
    AckDurable,
    /// A clock edge supplied by the host: re-offers an acknowledgement not yet durable.
    Tick,
}

/// What the node does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeOutput {
    /// The edit became this version (it flows up in the node's stream).
    EditWritten {
        /// The new version.
        version: Version,
    },
    /// The node stops owning: its copy is a read-only mirror, exactly now.
    StopOwning,
    /// Write the adoption acknowledgement carrying the node's final version.
    WriteAck {
        /// The last version the node wrote while it owned the record.
        final_version: Version,
    },
    /// The input was refused; the state is unchanged.
    Rejected(AdoptionError),
}

/// Whether the node holds the record as owner or as a mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ownership {
    /// The node owns the record and may edit it.
    Owner,
    /// The node's copy is a read-only mirror of the hub-owned record.
    Mirror,
}

/// The node's view of one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeSide {
    ownership: Ownership,
    version: Version,
    ack_durable: bool,
}

impl NodeSide {
    /// A node that owns the record at `version` (the version it created).
    #[must_use]
    pub fn new(version: Version) -> Self {
        Self {
            ownership: Ownership::Owner,
            version,
            ack_durable: false,
        }
    }

    /// Owner or mirror.
    #[must_use]
    pub fn ownership(&self) -> Ownership {
        self.ownership
    }

    /// Whether the node owns the record.
    #[must_use]
    pub fn owns(&self) -> bool {
        self.ownership == Ownership::Owner
    }

    /// The node's latest version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Takes one input; returns the next state and the outputs. A refused input returns the
    /// state unchanged with one [`NodeOutput::Rejected`].
    #[must_use]
    pub fn step(self, input: NodeInput) -> (Self, Vec<NodeOutput>) {
        let mut next = self;
        let mut out = Vec::new();
        match (input, self.ownership) {
            (NodeInput::LocalEdit, Ownership::Owner) => match self.version.checked_add(1) {
                Some(version) => {
                    next.version = version;
                    out.push(NodeOutput::EditWritten { version });
                }
                None => out.push(NodeOutput::Rejected(AdoptionError::VersionExhausted)),
            },
            (NodeInput::LocalEdit, Ownership::Mirror) => {
                out.push(NodeOutput::Rejected(AdoptionError::NotOwner));
            }
            (NodeInput::RequestApplied { seen_version }, Ownership::Owner) => {
                if seen_version > self.version {
                    out.push(NodeOutput::Rejected(AdoptionError::RequestAhead {
                        seen_version,
                        node_version: self.version,
                    }));
                } else {
                    next.ownership = Ownership::Mirror;
                    out.push(NodeOutput::StopOwning);
                    out.push(NodeOutput::WriteAck {
                        final_version: self.version,
                    });
                }
            }
            // A request applied twice changes nothing: the first application already stopped
            // the ownership and wrote the acknowledgement.
            (NodeInput::RequestApplied { .. }, Ownership::Mirror) => {}
            (NodeInput::AckDurable, Ownership::Mirror) => next.ack_durable = true,
            (NodeInput::AckDurable, Ownership::Owner) => {}
            (NodeInput::Tick, Ownership::Mirror) if !self.ack_durable => {
                out.push(NodeOutput::WriteAck {
                    final_version: self.version,
                });
            }
            (NodeInput::Tick, _) => {}
        }
        (next, out)
    }
}
