//! The process contract model (ADR-080 section 5.2, WP-BPM-12).
//!
//! A process always runs inside one tenant; when several organisations take
//! part, each runs its own half and the halves agree on a **process contract**
//! (ADR-080 section 5.1). The contract is a versioned document every
//! participant holds:
//!
//! - the [`participants`](ProcessContract::participants) by role, and whether
//!   each is a platform tenant or an external system;
//! - the [`messages`](ProcessContract::messages): a name, a tenant-link topic,
//!   a schema version, the roles that send and receive it, and the fields that
//!   may cross (nothing else crosses, ADR-058 rule 6);
//! - the [`order`](ProcessContract::order): which message may follow which and
//!   by which role, how often a step may repeat, and which outcomes end a
//!   conversation;
//! - the [`timeouts`](ProcessContract::timeouts): the time limit of each wait;
//! - the [`signatures`](ProcessContract::signatures) the participants put on
//!   this version (ADR-080 decision 4 leaves the signing policy open, so a
//!   contract stores and verifies any signature present and requires none).
//!
//! The types here are plain data: no I/O, no clock, no randomness, no floats
//! (ADR-12). Time is a number of seconds the caller supplies, never read from
//! a clock.

/// The node config a send step names: the id of the contract it sends on.
pub const CONTRACT_KIND: &str = "process_contract";

/// The name a `spot-custom-bundle` section carrying process contracts uses.
pub const SECTION_KIND: &str = "process_contracts";

/// The schema version of the contract text form a bundle section carries.
pub const SECTION_SCHEMA_VERSION: u32 = 1;

/// Whether a participant is a platform tenant or an external system
/// (ADR-080 section 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParticipantKind {
    /// A tenant on the platform, reached over a tenant link (ADR-058).
    #[default]
    Tenant,
    /// A system that is not a tenant, reached through a connector or a signed
    /// webhook (ADR-058 rule 12).
    External,
}

impl ParticipantKind {
    /// The text token used by the text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::External => "external",
        }
    }
}

/// One role of the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    /// The role's name; `[a-z][a-z0-9_]{0,63}`.
    pub role: String,
    /// Whether the role is a tenant or an external system.
    pub kind: ParticipantKind,
}

impl Participant {
    /// Builds a participant.
    #[must_use]
    pub fn new(role: impl Into<String>, kind: ParticipantKind) -> Self {
        Self {
            role: role.into(),
            kind,
        }
    }
}

/// The kind of value a field may carry.
///
/// Money and quantities travel as **integers in minor units** (ADR-12): there
/// is deliberately no floating-point kind, and a payload containing a float is
/// rejected by [`Schema::validate`](crate::Schema::validate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Text.
    String,
    /// A signed integer: money and quantities as minor units.
    Integer,
    /// A boolean.
    Boolean,
}

impl FieldKind {
    /// The text token used by the text form and by JSON Schema.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
        }
    }
}

/// One field of a message: the only shape in which a value may cross
/// (ADR-058 rule 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The field's name: dotted lower-case segments, at most 128 characters.
    pub name: String,
    /// The kind of value it carries.
    pub kind: FieldKind,
    /// Whether the payload must carry it.
    pub required: bool,
    /// The longest text, for a [`FieldKind::String`]; absent for the other
    /// kinds.
    pub max_len: Option<u64>,
}

impl Field {
    /// A required field of the given kind.
    #[must_use]
    pub fn required(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: true,
            max_len: None,
        }
    }

    /// An optional field of the given kind.
    #[must_use]
    pub fn optional(name: impl Into<String>, kind: FieldKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            max_len: None,
        }
    }

    /// The same field with a longest-text bound.
    #[must_use]
    pub fn with_max_len(mut self, max_len: u64) -> Self {
        self.max_len = Some(max_len);
        self
    }
}

/// One message the participants exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDef {
    /// The message's name; `[a-z][a-z0-9_]{0,63}`. It is what a send or wait
    /// step names.
    pub name: String,
    /// The tenant-link topic the message travels on (ADR-058 rule 7): two or
    /// more dotted lower-case segments.
    pub topic: String,
    /// The schema version of the payload; starts at 1.
    pub schema_version: u32,
    /// The role that sends the message.
    pub from: String,
    /// The role that receives it.
    pub to: String,
    /// The fields that may cross, and nothing else.
    pub fields: Vec<Field>,
}

impl MessageDef {
    /// Builds a message.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        topic: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        fields: Vec<Field>,
    ) -> Self {
        Self {
            name: name.into(),
            topic: topic.into(),
            schema_version: 1,
            from: from.into(),
            to: to.into(),
            fields,
        }
    }

    /// The same message at another schema version.
    #[must_use]
    pub fn at_schema_version(mut self, schema_version: u32) -> Self {
        self.schema_version = schema_version;
        self
    }

    /// The field with this name, when the message carries it.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.name == name)
    }
}

/// What a transition leads to: the next message, or an outcome that ends the
/// conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The name of the message that may follow.
    Message(String),
    /// The name of the outcome the conversation ends in.
    Outcome(String),
}

/// One order rule: after a message, one role may send the next message — or
/// the conversation ends in an outcome (ADR-080 section 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    /// The role that may act: the sender of [`Next::Message`], or the role that
    /// declares the outcome.
    pub by: String,
    /// The message just exchanged; absent for a message that starts a
    /// conversation.
    pub after: Option<String>,
    /// What may follow.
    pub next: Next,
    /// The most times this transition may be taken in one conversation. It is
    /// what bounds a cycle: a cycle with at least one bounded transition is
    /// bounded, a cycle with none is refused by the checker. Absent means once,
    /// which is also what a linear order needs.
    pub bound: Option<u32>,
}

impl Transition {
    /// A transition from `after` to the message `next`, sent by `by`.
    #[must_use]
    pub fn message(by: impl Into<String>, after: Option<&str>, next: impl Into<String>) -> Self {
        Self {
            by: by.into(),
            after: after.map(str::to_owned),
            next: Next::Message(next.into()),
            bound: None,
        }
    }

    /// A transition from `after` to the outcome `outcome`.
    #[must_use]
    pub fn outcome(by: impl Into<String>, after: Option<&str>, outcome: impl Into<String>) -> Self {
        Self {
            by: by.into(),
            after: after.map(str::to_owned),
            next: Next::Outcome(outcome.into()),
            bound: None,
        }
    }

    /// The same transition bounded to `bound` repetitions.
    #[must_use]
    pub fn with_bound(mut self, bound: u32) -> Self {
        self.bound = Some(bound);
        self
    }

    /// The message this transition leads to, when it leads to one.
    #[must_use]
    pub fn next_message(&self) -> Option<&str> {
        match &self.next {
            Next::Message(name) => Some(name),
            Next::Outcome(_) => None,
        }
    }

    /// The outcome this transition leads to, when it leads to one.
    #[must_use]
    pub fn next_outcome(&self) -> Option<&str> {
        match &self.next {
            Next::Message(_) => None,
            Next::Outcome(name) => Some(name),
        }
    }
}

/// How a conversation ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutcomeKind {
    /// The participants achieved what the conversation was for.
    #[default]
    Completed,
    /// A participant cancelled, and the contract's cancellation message was
    /// exchanged (ADR-080 section 5.9).
    Cancelled,
    /// The conversation failed as the contract declares.
    Failed,
    /// The conversation cannot continue and a person must decide
    /// (ADR-080 section 5.6).
    NeedsAttention,
}

impl OutcomeKind {
    /// The text token used by the text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

/// One outcome state of the whole conversation (ADR-080 section 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The outcome's name; `[a-z][a-z0-9_]{0,63}`.
    pub name: String,
    /// What kind of ending it is.
    pub kind: OutcomeKind,
}

impl Outcome {
    /// Builds an outcome.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: OutcomeKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// The time limit of one wait (ADR-080 section 5.2, "the time limit of each
/// wait").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeoutRule {
    /// The message the role waits for.
    pub message: String,
    /// The role that waits — the message's receiver.
    pub by: String,
    /// The time limit in whole seconds (ADR-12: never a float).
    pub seconds: u64,
    /// The outcome the waiting side takes when the limit passes; absent leaves
    /// the choice to the waiting side's own timeout branch, which the contract
    /// still bounds.
    pub outcome: Option<String>,
}

impl TimeoutRule {
    /// A time limit for `by` waiting for `message`.
    #[must_use]
    pub fn new(message: impl Into<String>, by: impl Into<String>, seconds: u64) -> Self {
        Self {
            message: message.into(),
            by: by.into(),
            seconds,
            outcome: None,
        }
    }

    /// The same rule taking an outcome when the limit passes.
    #[must_use]
    pub fn ending_in(mut self, outcome: impl Into<String>) -> Self {
        self.outcome = Some(outcome.into());
        self
    }
}

/// One participant's signature on this contract version (ADR-080 decision 4).
///
/// The structure is stable; the algorithm is a token the verifier understands,
/// so no cryptography is baked into the model. `signed_hash` is the content
/// hash the signature covers, which is what makes a signature over a *version*
/// meaningful: re-signing a changed contract is a new signature over a new
/// hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// The role that signed.
    pub role: String,
    /// The algorithm token, for example `ed25519`.
    pub algorithm: String,
    /// The identifier of the public key in the signer's vault.
    pub key_id: String,
    /// The content hash the signature covers, 64 lowercase hex characters.
    pub signed_hash: String,
    /// The signature itself, opaque to this crate.
    pub signature: String,
}

impl Signature {
    /// Builds a signature.
    #[must_use]
    pub fn new(
        role: impl Into<String>,
        algorithm: impl Into<String>,
        key_id: impl Into<String>,
        signed_hash: impl Into<String>,
        signature: impl Into<String>,
    ) -> Self {
        Self {
            role: role.into(),
            algorithm: algorithm.into(),
            key_id: key_id.into(),
            signed_hash: signed_hash.into(),
            signature: signature.into(),
        }
    }
}

/// A versioned process contract: the agreed document both halves of a
/// conversation hold (ADR-080 section 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessContract {
    /// The contract's stable id; `[a-z][a-z0-9_]{0,63}`.
    pub id: String,
    /// The version number; starts at 1. A change is always a new version, and
    /// a running conversation stays on the version it started with.
    pub version: u32,
    /// An optional human-readable title.
    pub title: Option<String>,
    /// The roles taking part.
    pub participants: Vec<Participant>,
    /// The messages that may cross.
    pub messages: Vec<MessageDef>,
    /// The order rules.
    pub order: Vec<Transition>,
    /// The outcome states a conversation may end in.
    pub outcomes: Vec<Outcome>,
    /// The time limit of each wait.
    pub timeouts: Vec<TimeoutRule>,
    /// The signatures participants put on this version; none is required.
    pub signatures: Vec<Signature>,
}

impl ProcessContract {
    /// Builds a contract version from its parts, with no title and no
    /// signatures.
    #[must_use]
    pub fn new(id: impl Into<String>, version: u32) -> Self {
        Self {
            id: id.into(),
            version,
            title: None,
            participants: Vec::new(),
            messages: Vec::new(),
            order: Vec::new(),
            outcomes: Vec::new(),
            timeouts: Vec::new(),
            signatures: Vec::new(),
        }
    }

    /// The same contract with a title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The same contract with these participants.
    #[must_use]
    pub fn with_participants(mut self, participants: Vec<Participant>) -> Self {
        self.participants = participants;
        self
    }

    /// The same contract with these messages.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<MessageDef>) -> Self {
        self.messages = messages;
        self
    }

    /// The same contract with these order rules.
    #[must_use]
    pub fn with_order(mut self, order: Vec<Transition>) -> Self {
        self.order = order;
        self
    }

    /// The same contract with these outcomes.
    #[must_use]
    pub fn with_outcomes(mut self, outcomes: Vec<Outcome>) -> Self {
        self.outcomes = outcomes;
        self
    }

    /// The same contract with these wait limits.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Vec<TimeoutRule>) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// The same contract with these signatures.
    #[must_use]
    pub fn with_signatures(mut self, signatures: Vec<Signature>) -> Self {
        self.signatures = signatures;
        self
    }

    /// The participant with this role, when the contract names it.
    #[must_use]
    pub fn participant(&self, role: &str) -> Option<&Participant> {
        self.participants.iter().find(|p| p.role == role)
    }

    /// The message with this name, when the contract defines it.
    #[must_use]
    pub fn message(&self, name: &str) -> Option<&MessageDef> {
        self.messages.iter().find(|m| m.name == name)
    }

    /// The outcome with this name, when the contract declares it.
    #[must_use]
    pub fn outcome(&self, name: &str) -> Option<&Outcome> {
        self.outcomes.iter().find(|o| o.name == name)
    }

    /// The transitions that may follow `message`, in definition order.
    #[must_use]
    pub fn outgoing(&self, message: &str) -> Vec<&Transition> {
        self.order
            .iter()
            .filter(|t| t.after.as_deref() == Some(message))
            .collect()
    }

    /// The transitions that start a conversation, in definition order.
    #[must_use]
    pub fn starts(&self) -> Vec<&Transition> {
        self.order.iter().filter(|t| t.after.is_none()).collect()
    }

    /// The time limit of `by` waiting for `message`, when the contract sets
    /// one.
    #[must_use]
    pub fn timeout(&self, message: &str, by: &str) -> Option<&TimeoutRule> {
        self.timeouts
            .iter()
            .find(|t| t.message == message && t.by == by)
    }

    /// Whether the role sends or receives this message.
    #[must_use]
    pub fn involves(&self, role: &str, message: &str) -> bool {
        self.message(message)
            .is_some_and(|m| m.from == role || m.to == role)
    }

    /// Every message reachable from a conversation's start, following the
    /// order rules.
    #[must_use]
    pub fn reachable_messages(&self) -> std::collections::BTreeSet<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut queue: Vec<String> = self
            .starts()
            .iter()
            .filter_map(|t| t.next_message().map(str::to_owned))
            .collect();
        while let Some(name) = queue.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            for transition in self.outgoing(&name) {
                if let Some(next) = transition.next_message() {
                    queue.push(next.to_owned());
                }
            }
        }
        seen
    }
}
