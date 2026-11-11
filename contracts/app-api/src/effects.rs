//! Effect declarations and operation receipts (ADR-080 section 6; contract 0.5.0).
//!
//! A pipeline step that changes the world says so in advance with [`EffectDecl`]s: what kind of
//! effect it has, its class, what it writes, and how it is undone ([`Reversal`]). While it runs it
//! reports what it actually did with [`HookContext::report_effect`](crate::HookContext), and the
//! engine gathers those reports into an [`OperationReceipt`]. The process engine reads only
//! classes, reversibility and opaque references from them; it never reads a balance or movement
//! store (ADR-080 6.4, 6.9).
//!
//! # Rules
//!
//! - **Internal** effects (writes to append-only balance and movement stores) run in phase 3
//!   (`after_*`), inside the operation's transaction, and are reported there. An internal effect
//!   must be [`Reversal::ReversibleBy`] the operation that reverses the step's own operation
//!   ([`reversing_operation`]: `Post` is undone by `Cancel`; `Correct` is cancel plus a new
//!   draft, so it needs no entry of its own).
//! - **External** effects (mail, connector calls, webhooks, messages to other organisations) are
//!   announced in phase 4 (`*_committed`) through the outbox and are reported when announced.
//!   They may be reversible, compensable or irreversible; an irreversible one is a point of no
//!   return.
//! - A step may report only an effect whose class and kind it declared
//!   ([`EffectReceipt::matches_declaration`]). A report that does not match is a conformance
//!   failure in the test kit and a logged event in production.
//! - A step with no declaration has "no declared effects"; an operation whose steps report
//!   nothing has an empty receipt. Both are valid, so a 0.4.0 controller keeps working.
//! - [`OpaqueRef`]s identify rows in stores the engine does not read. Consumers must not
//!   interpret, parse, order or compare them for anything but equality.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_storage_contract::{Name, Version};
use time::OffsetDateTime;

use crate::controller::{StepDecl, StepName};
use crate::error::{AppError, AppResult, ErrorClass};
use crate::lifecycle::{HookPoint, Operation, Phase, Target, op_name};

fn invalid(code: &str, message: String) -> AppError {
    AppError::new(ErrorClass::Internal, code, message)
}

macro_rules! storage_style_name {
    ($(#[$m:meta])* $ty:ident, $code:literal, $what:literal) => {
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $ty(String);

        impl $ty {
            /// Validates the name: lowercase letters, digits and underscores, starting with a
            /// letter, at most 63 bytes (the storage name rule).
            pub fn new(s: &str) -> AppResult<Self> {
                Name::new(s)
                    .map(|n| Self(n.as_str().to_owned()))
                    .map_err(|_| invalid($code, format!(concat!($what, " must be a storage-style name, got {:?}"), s)))
            }

            /// The name as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $ty {
            type Error = AppError;
            fn try_from(s: String) -> AppResult<Self> {
                Self::new(&s)
            }
        }

        impl From<$ty> for String {
            fn from(v: $ty) -> String {
                v.0
            }
        }
    };
}

storage_style_name!(
    /// An effect class: what sort of effect this is, for example `balance_movement`,
    /// `mail` or `link_message`. The engine understands classes, never what they write.
    EffectClassName,
    "effects.invalid_class",
    "an effect class"
);

storage_style_name!(
    /// A store or channel an effect writes to, named like a storage entity.
    StoreName,
    "effects.invalid_store",
    "a store name"
);

/// A named compensating action: `<app>:<action>`, both storage-style names, for example
/// `erp_accounts:send_correction`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActionRef(String);

impl ActionRef {
    /// Validates an action reference.
    pub fn new(s: &str) -> AppResult<Self> {
        let ok = s
            .split_once(':')
            .is_some_and(|(a, n)| Name::new(a).is_ok() && Name::new(n).is_ok());
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(invalid(
                "effects.invalid_action",
                format!("an action reference is <app>:<action>, got {s:?}"),
            ))
        }
    }

    /// The reference as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ActionRef {
    type Error = AppError;
    fn try_from(s: String) -> AppResult<Self> {
        Self::new(&s)
    }
}

impl From<ActionRef> for String {
    fn from(v: ActionRef) -> String {
        v.0
    }
}

/// An identifier of a row in a store the engine does not read. **Consumers must not interpret
/// it**: the only supported operations are storing it and comparing it for equality.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OpaqueRef(String);

impl OpaqueRef {
    /// Longest reference in bytes.
    pub const MAX_LEN: usize = 128;

    /// Wraps a reference: not empty, at most [`MAX_LEN`](Self::MAX_LEN) bytes, no control
    /// characters.
    pub fn new(s: &str) -> AppResult<Self> {
        if s.is_empty() || s.len() > Self::MAX_LEN || s.chars().any(char::is_control) {
            return Err(invalid(
                "effects.invalid_ref",
                format!(
                    "an opaque reference is 1 to {} bytes with no control characters",
                    Self::MAX_LEN
                ),
            ));
        }
        Ok(Self(s.to_owned()))
    }

    /// The reference as text, for storing and equality only.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OpaqueRef {
    type Error = AppError;
    fn try_from(s: String) -> AppResult<Self> {
        Self::new(&s)
    }
}

impl From<OpaqueRef> for String {
    fn from(v: OpaqueRef) -> String {
        v.0
    }
}

/// A 32-byte digest as 64 lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hex32(String);

impl Hex32 {
    /// Validates a digest in hex.
    pub fn new(s: &str) -> AppResult<Self> {
        if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_owned()))
        } else {
            Err(invalid(
                "effects.invalid_digest",
                "a digest is 64 lowercase hex characters".to_owned(),
            ))
        }
    }

    /// The digest as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Hex32 {
    type Error = AppError;
    fn try_from(s: String) -> AppResult<Self> {
        Self::new(&s)
    }
}

impl From<Hex32> for String {
    fn from(v: Hex32) -> String {
        v.0
    }
}

/// Whether an effect stays inside the platform's transaction or leaves the building.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EffectKind {
    /// Writes to append-only stores inside the operation's transaction (phase 3).
    Internal,
    /// Mail, connector calls, webhooks, messages to other organisations; announced in phase 4.
    External,
}

/// How an effect is undone (ADR-080 6.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reversal {
    /// Undone by a lifecycle operation of the same record (post by cancel).
    ReversibleBy(Operation),
    /// Cannot be undone, but a named compensating action exists.
    CompensableBy(ActionRef),
    /// Nothing can undo it: a point of no return.
    Irreversible {
        /// Why, for the designer and the audit trail.
        reason: String,
    },
}

/// The summary of a [`Reversal`], what the process engine reasons with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reversibility {
    /// Undone by a lifecycle operation.
    Reversible,
    /// Compensated by a named action.
    Compensable,
    /// A point of no return.
    Irreversible,
}

impl Reversal {
    /// The summary.
    pub fn reversibility(&self) -> Reversibility {
        match self {
            Reversal::ReversibleBy(_) => Reversibility::Reversible,
            Reversal::CompensableBy(_) => Reversibility::Compensable,
            Reversal::Irreversible { .. } => Reversibility::Irreversible,
        }
    }
}

/// The operation that reverses `op`, if `op` has a pair: `Post` is reversed by `Cancel`.
/// `Correct` is cancel plus a new draft, so it is covered by `Cancel`.
pub fn reversing_operation(op: Operation) -> Option<Operation> {
    match op {
        Operation::Post => Some(Operation::Cancel),
        _ => None,
    }
}

/// One effect a step declares (ADR-080 6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectDecl {
    /// Internal or external.
    pub kind: EffectKind,
    /// The class.
    pub class: EffectClassName,
    /// Stores or channels it writes.
    pub writes: Vec<StoreName>,
    /// How it is undone.
    pub reversal: Reversal,
}

impl EffectDecl {
    /// Builds a declaration. Call [`validate`](Self::validate) before registering it.
    pub fn new(
        kind: EffectKind,
        class: EffectClassName,
        writes: Vec<StoreName>,
        reversal: Reversal,
    ) -> Self {
        Self {
            kind,
            class,
            writes,
            reversal,
        }
    }

    /// Checks the declaration on its own: an internal effect must be [`Reversal::ReversibleBy`]
    /// and must write at least one store; an irreversible effect must give a reason.
    pub fn validate(&self) -> AppResult<()> {
        if self.kind == EffectKind::Internal && !matches!(self.reversal, Reversal::ReversibleBy(_))
        {
            return Err(invalid(
                "effects.internal_not_reversible",
                format!(
                    "internal effect {} must be reversible by a lifecycle operation, not {:?}",
                    self.class,
                    self.reversal.reversibility()
                ),
            ));
        }
        if self.kind == EffectKind::Internal && self.writes.is_empty() {
            return Err(invalid(
                "effects.internal_writes_nothing",
                format!("internal effect {} names no store it writes", self.class),
            ));
        }
        if let Reversal::Irreversible { reason } = &self.reversal
            && reason.trim().is_empty()
        {
            return Err(invalid(
                "effects.irreversible_reason",
                format!("irreversible effect {} needs a reason", self.class),
            ));
        }
        Ok(())
    }

    /// [`validate`](Self::validate), plus the rules that depend on where the step runs: an
    /// internal effect runs in phase 3 and, on an operation that has a pair, is reversed by that
    /// pair; an external effect runs in phase 4.
    pub fn validate_at(&self, point: &HookPoint) -> AppResult<()> {
        self.validate()?;
        let phase = point.phase();
        match self.kind {
            EffectKind::Internal => {
                if phase != Phase::After {
                    return Err(invalid(
                        "effects.wrong_phase",
                        format!(
                            "internal effect {} must run in the after phase, not at {}",
                            self.class,
                            point.name()
                        ),
                    ));
                }
                if let HookPoint::Lifecycle {
                    target: Target::Op(op),
                    ..
                } = point
                    && let Some(pair) = reversing_operation(*op)
                    && self.reversal != Reversal::ReversibleBy(pair)
                {
                    return Err(invalid(
                        "effects.wrong_reversal",
                        format!(
                            "internal effect {} at {} must be reversible by {}",
                            self.class,
                            point.name(),
                            op_name(pair)
                        ),
                    ));
                }
            }
            EffectKind::External => {
                if phase != Phase::Committed {
                    return Err(invalid(
                        "effects.wrong_phase",
                        format!(
                            "external effect {} must be announced in the committed phase, not at {}",
                            self.class,
                            point.name()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// The effects one step declares. Carried beside [`StepDecl`] by
/// [`Controller::step_effects`](crate::Controller::step_effects) so that `StepDecl` itself keeps
/// the 0.4.0 shape (see the 0.5.0 changelog).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepEffects {
    /// The step.
    pub step: StepName,
    /// Its effects; empty means "no declared effects".
    pub effects: Vec<EffectDecl>,
}

impl StepEffects {
    /// A step's declarations.
    pub fn new(step: StepName, effects: Vec<EffectDecl>) -> Self {
        Self { step, effects }
    }
}

/// Validates every declaration of a controller against its steps: each [`StepEffects`] names a
/// step the controller declares, at most once, and each effect passes
/// [`EffectDecl::validate_at`] for that step's hook point.
pub fn validate_declarations(steps: &[StepDecl], declared: &[StepEffects]) -> AppResult<()> {
    for (i, d) in declared.iter().enumerate() {
        if declared[..i].iter().any(|e| e.step == d.step) {
            return Err(invalid(
                "effects.duplicate_step",
                format!("effects of step {} are declared twice", d.step),
            ));
        }
        let Some(step) = steps.iter().find(|s| s.name == d.step) else {
            return Err(invalid(
                "effects.unknown_step",
                format!("effects are declared for {}, which is not a step", d.step),
            ));
        };
        for e in &d.effects {
            e.validate_at(&step.point)?;
        }
    }
    Ok(())
}

/// What happened to an effect (ADR-080 6.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReceiptState {
    /// Written (internal) or handed to the outbox.
    Applied,
    /// Waiting in a hold scope for a commit point.
    Held,
    /// Released from the hold.
    Released,
    /// Delivered to its destination.
    Delivered,
    /// Failed, with the error code.
    Failed {
        /// The error code.
        code: String,
    },
}

/// What one effect actually did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectReceipt {
    /// The step that produced it.
    pub step: StepName,
    /// The declared class.
    pub class: EffectClassName,
    /// Internal or external.
    pub kind: EffectKind,
    /// Opaque references to the rows or messages produced.
    pub store_refs: Vec<OpaqueRef>,
    /// How many rows or messages.
    pub count: u32,
    /// Digest of the content, for the audit trail.
    pub digest: Hex32,
    /// Where it stands.
    pub state: ReceiptState,
}

impl EffectReceipt {
    /// Whether the step declared an effect of this class and kind. A report that does not match
    /// is a conformance failure in the test kit and a logged event in production.
    pub fn matches_declaration(&self, declared: &[StepEffects]) -> bool {
        declared
            .iter()
            .filter(|d| d.step == self.step)
            .flat_map(|d| d.effects.iter())
            .any(|e| e.class == self.class && e.kind == self.kind)
    }
}

/// The record of what a lifecycle operation caused. Empty `effects` is valid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationReceipt {
    /// The operation.
    #[serde(with = "operation_wire")]
    pub operation: Operation,
    /// The record's DocType.
    pub doctype: String,
    /// The record's name.
    pub name: String,
    /// The record's version after the operation.
    #[serde(with = "version_wire")]
    pub version: Version,
    /// One entry per reported effect, in the order reported.
    pub effects: Vec<EffectReceipt>,
    /// When the operation committed (a value the host read once, never the wall clock here).
    #[serde(with = "micros_wire")]
    pub committed_at: OffsetDateTime,
}

mod operation_wire {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::{Operation, op_name};

    const ALL: [Operation; 7] = [
        Operation::Create,
        Operation::Update,
        Operation::Post,
        Operation::Cancel,
        Operation::Delete,
        Operation::Rename,
        Operation::UpdatePosted,
    ];

    pub fn serialize<S: Serializer>(op: &Operation, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(op_name(*op))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Operation, D::Error> {
        let text = String::deserialize(d)?;
        ALL.into_iter()
            .find(|op| op_name(*op) == text)
            .ok_or_else(|| D::Error::custom(format!("unknown operation {text:?}")))
    }
}

mod version_wire {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::Version;

    pub fn serialize<S: Serializer>(v: &Version, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(v.0)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
        u64::deserialize(d).map(Version)
    }
}

/// Microseconds since the Unix epoch.
mod micros_wire {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::OffsetDateTime;

    pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        let micros = i64::try_from(t.unix_timestamp_nanos() / 1_000)
            .map_err(|_| serde::ser::Error::custom("timestamp out of range"))?;
        s.serialize_i64(micros)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let micros = i64::deserialize(d)?;
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000)
            .map_err(|e| D::Error::custom(e.to_string()))
    }
}
