//! The message-arrived trigger (ADR-080 section 5.3).
//!
//! `spot-bpm-triggers` has an open registry of trigger kinds, so this crate adds `message_arrived`
//! the way the five built-in kinds are added: a [`TriggerMatcher`] and a [`TriggerStarter`]
//! registered with [`register`]. A process version's trigger spec for the kind is a
//! [`CustomSpec`] built by [`message_arrived_spec`] and added to the index with
//! `TriggerIndex::add_spec` (the model's trigger node cannot express the kind yet).
//!
//! The source event's id is the link message's id, so the intake's start key
//! `(process, trigger node, message id)` is the same on every redelivery and after every restart:
//! a message starts one request, however often its notice is read. [`EngineCaseStarter`] is the
//! `CaseStarter` that starts that request in the engine atomically with its variables and binds
//! the conversation to it.
//!
//! # The Case id is a cryptographic function of the start key
//!
//! The peer chooses the message id that is part of the start key, so the Case id must not be
//! findable by a collision search: a collision would make [`StartOutcome::Existing`] hand this
//! conversation someone else's Case, and that peer's `cancel` then runs the victim's
//! compensation. [`EngineCaseStarter::case_id`] therefore derives the id from a truncated
//! SHA-256, and the `Existing` path additionally verifies that the stored Case was started by
//! *this* message (`bpm.entry.message_id`) before binding anything; a mismatch is refused with
//! `bpm_conversations.case_collision`.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, CaseRecord, CaseStore, Engine, TimerPort};
use spot_bpm_triggers::{
    CaseStarter, CustomEvent, CustomSpec, Error as TriggerError, Source, SourceEvent, StartInput,
    StartKey, StartOutcome, TriggerMatch, TriggerMatcher, TriggerRegistry, TriggerSpec,
    TriggerStarter,
};
use spot_dag::Published;
use spot_dag_exec::start as dag_start;
use spot_link_exchange::{Conversation, MessageId};
use spot_tenant_link::LinkId;

use crate::error::codes;
use crate::record::{ConversationStore, update};

/// The kind token of the message-arrived trigger.
pub const KIND_MESSAGE_ARRIVED: &str = "message_arrived";

/// The spec of a trigger node that starts a request when `message` of `contract` arrives and the
/// contract names it as an entry message.
#[must_use]
pub fn message_arrived_spec(contract: &str, message: &str) -> TriggerSpec {
    TriggerSpec::Custom(CustomSpec {
        kind: KIND_MESSAGE_ARRIVED.to_owned(),
        config: json!({ "contract": contract, "message": message }),
    })
}

/// A message that arrived and may start a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Arrival {
    /// The link message's id.
    pub message_id: MessageId,
    /// The conversation it belongs to.
    pub conversation: Conversation,
    /// The link it came on.
    pub link: LinkId,
    /// The contract.
    pub contract: String,
    /// The contract version the conversation runs on.
    pub version: u32,
    /// The contract message's name.
    pub message: String,
    /// The contract fields, as plain JSON.
    pub payload: Value,
    /// Unix milliseconds the inbox took it.
    pub received_at_ms: u64,
}

impl Arrival {
    /// The source event the intake matches against the published trigger specs.
    #[must_use]
    pub fn source_event(&self) -> SourceEvent {
        SourceEvent::system(Source::Custom(CustomEvent {
            kind: KIND_MESSAGE_ARRIVED.to_owned(),
            source_event_id: self.message_id.to_string(),
            payload: json!({
                "contract": self.contract,
                "version": self.version,
                "message": self.message,
                "conversation": self.conversation.to_string(),
                "link": self.link.to_string(),
                "message_id": self.message_id.to_string(),
                "received_at_ms": self.received_at_ms,
                "payload": self.payload,
            }),
        }))
    }
}

/// The matcher and starter of the `message_arrived` kind.
#[derive(Debug, Clone, Copy, Default)]
pub struct MessageArrivedKind;

impl TriggerMatcher for MessageArrivedKind {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool, TriggerError> {
        let (TriggerSpec::Custom(spec), Source::Custom(arrived)) = (&matched.spec, &event.source)
        else {
            return Ok(false);
        };
        if spec.kind != KIND_MESSAGE_ARRIVED || arrived.kind != KIND_MESSAGE_ARRIVED {
            return Ok(false);
        }
        let same = |key: &str| {
            spec.config.get(key).and_then(Value::as_str).is_some()
                && spec.config.get(key) == arrived.payload.get(key)
        };
        Ok(same("contract") && same("message"))
    }
}

impl TriggerStarter for MessageArrivedKind {
    fn start_input(
        &self,
        matched: &TriggerMatch,
        event: &SourceEvent,
    ) -> Result<StartInput, TriggerError> {
        let Source::Custom(arrived) = &event.source else {
            return Err(TriggerError::StartFailed {
                process: matched.process.clone(),
                node: matched.node.to_string(),
                reason: "a message-arrived start needs an arrived message".to_owned(),
            });
        };
        let text = |key: &str| arrived.payload.get(key).and_then(Value::as_str);
        let (Some(contract), Some(message), Some(conversation), Some(link)) = (
            text("contract"),
            text("message"),
            text("conversation"),
            text("link"),
        ) else {
            return Err(TriggerError::StartFailed {
                process: matched.process.clone(),
                node: matched.node.to_string(),
                reason: "the arrived message is incomplete".to_owned(),
            });
        };
        let payload = arrived
            .payload
            .get("payload")
            .cloned()
            .unwrap_or(Value::Null);
        let received_at = arrived
            .payload
            .get("received_at_ms")
            .cloned()
            .unwrap_or(Value::Null);
        let message_id = arrived
            .payload
            .get("message_id")
            .cloned()
            .unwrap_or(Value::Null);
        Ok(StartInput {
            version: matched.version,
            variables: json!({
                "bpm": {
                    "conv": { contract: { "id": conversation, "link": link } },
                    "entry": {
                        "contract": contract,
                        "message": message,
                        "message_id": message_id,
                        "received_at_ms": received_at,
                    },
                },
                "received": { message: payload },
            }),
            initiator: None,
        })
    }
}

/// Registers the `message_arrived` kind with a trigger registry.
pub fn register(registry: &mut TriggerRegistry) {
    registry.register_kind(
        KIND_MESSAGE_ARRIVED,
        Arc::new(MessageArrivedKind),
        Arc::new(MessageArrivedKind),
    );
}

/// Starts the request a message starts, in the engine, once per start key.
///
/// The Case id is a pure function of the start key, so two starts of one key address one Case; the
/// Case is created with its variables in a single store call, so a crash leaves either no Case or
/// a complete one. The conversation record is bound to the Case afterwards, on a fresh start and
/// on an existing one alike, so a crash between the two is repaired by the redelivery.
pub struct EngineCaseStarter<S: CaseStore + 'static, T: TimerPort + 'static> {
    engine: Arc<Engine<S, T>>,
    conversations: Arc<dyn ConversationStore>,
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> EngineCaseStarter<S, T> {
    /// A starter over `engine` that binds started Cases in `conversations`.
    #[must_use]
    pub fn new(engine: Arc<Engine<S, T>>, conversations: Arc<dyn ConversationStore>) -> Self {
        Self {
            engine,
            conversations,
        }
    }

    fn failed(key: &StartKey, reason: &str) -> TriggerError {
        TriggerError::StartFailed {
            process: key.process.clone(),
            node: key.node.to_string(),
            reason: reason.to_owned(),
        }
    }

    /// The Case id for a start key: a truncated SHA-256 over the three parts.
    ///
    /// The message id in `key.source_event` is chosen by the peer, so a hash with a feasible
    /// collision search would let a peer steer its message onto another conversation's Case.
    /// 128 bits of SHA-256 leaves no such search; the leading `m` keeps the id inside the
    /// engine's `[a-z][a-z0-9_]*` grammar.
    ///
    /// # Errors
    /// [`TriggerError::StartFailed`] when the derived id is not a valid Case id.
    pub fn case_id(key: &StartKey) -> Result<CaseId, TriggerError> {
        let mut digest = Sha256::new();
        for part in [
            key.process.as_str(),
            key.node.as_str(),
            key.source_event.as_str(),
        ] {
            digest.update(part.as_bytes());
            // A separator the three parts cannot contain, so `(a, bc)` and `(ab, c)` differ.
            digest.update([0u8]);
        }
        let mut text = String::with_capacity(33);
        text.push('m');
        for byte in &digest.finalize()[..16] {
            // Writing into a `String` cannot fail.
            let _ = write!(text, "{byte:02x}");
        }
        CaseId::new(&text).map_err(|_| Self::failed(key, "the case id is invalid"))
    }

    /// Refuses a start whose Case id is already taken by a Case another message started.
    ///
    /// The stored Case's `bpm.entry.message_id` is the identity of the message that started it;
    /// an `Existing` answer is only honest when that is the message being started now.
    fn verify_started_by(
        key: &StartKey,
        case: &CaseRecord,
        input: &StartInput,
    ) -> Result<(), TriggerError> {
        let wanted = input
            .variables
            .pointer("/bpm/entry/message_id")
            .and_then(Value::as_str);
        let stored = case
            .variables
            .pointer("/bpm/entry/message_id")
            .and_then(Value::as_str);
        if wanted.is_some() && wanted == stored {
            return Ok(());
        }
        Err(TriggerError::StartFailed {
            process: key.process.clone(),
            node: key.node.to_string(),
            reason: format!(
                "{}: the case id is already used by another message",
                codes::CASE_COLLISION
            ),
        })
    }

    fn bind(&self, key: &StartKey, case: &str, variables: &Value) -> Result<(), TriggerError> {
        let entry = variables.pointer("/bpm/entry");
        let text = |value: Option<&Value>, key: &str| {
            value
                .and_then(|v| v.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let Some(contract) = text(entry, "contract") else {
            return Ok(());
        };
        let conv = variables.pointer(&format!("/bpm/conv/{contract}"));
        let (Some(conversation), Some(link), Some(message_id)) = (
            text(conv, "id").and_then(|id| Conversation::new(id).ok()),
            text(conv, "link").and_then(|link| link.parse::<LinkId>().ok()),
            text(entry, "message_id").and_then(|id| id.parse::<MessageId>().ok()),
        ) else {
            return Ok(());
        };
        // A failure is reported, so the notice is read again and the binding retried.
        update(&*self.conversations, &link, &conversation, |record| {
            if record.case.is_none() {
                record.case = Some(case.to_owned());
            }
            record.consumed.insert(message_id);
            Ok(())
        })
        .map(|_| ())
        .map_err(|_| Self::failed(key, "the conversation could not be bound to the case"))
    }
}

#[async_trait]
impl<S: CaseStore + 'static, T: TimerPort + 'static> CaseStarter for EngineCaseStarter<S, T> {
    async fn start(&self, key: StartKey, input: StartInput) -> Result<StartOutcome, TriggerError> {
        let id = Self::case_id(&key)?;
        let case = id.to_string();
        let existing = self
            .engine
            .store()
            .load(&id)
            .await
            .map_err(|_| Self::failed(&key, "the case store failed"))?;
        if let Some(existing) = existing {
            Self::verify_started_by(&key, &existing, &input)?;
            self.bind(&key, &case, &input.variables)?;
            return Ok(StartOutcome::Existing { case });
        }
        let definition = self
            .engine
            .processes()
            .get(&key.process, input.version)
            .ok_or_else(|| Self::failed(&key, "the process version is not published"))?;
        let at_ms = input
            .variables
            .pointer("/bpm/entry/received_at_ms")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let now = OffsetDateTime::from_unix_timestamp(at_ms.div_euclid(1_000))
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let pin = Published {
            version: definition.compiled.dag.version(),
            hash: definition.compiled.dag.hash().clone(),
        };
        let plan = dag_start(
            &definition.compiled.dag,
            &pin,
            id.clone(),
            case.clone(),
            now,
        )
        .map_err(|_| Self::failed(&key, "the case could not be planned"))?;
        let mut record = CaseRecord::new(plan.run, input.initiator.clone());
        record.variables = input.variables.clone();
        let created = self.engine.store().create(record).await;
        if created.is_err() {
            // Another start of the same key may have won the race; its Case must be this
            // message's, or the collision is refused rather than bound.
            let won = self
                .engine
                .store()
                .load(&id)
                .await
                .map_err(|_| Self::failed(&key, "the case store failed"))?;
            let Some(won) = won else {
                return Err(Self::failed(&key, "the case could not be stored"));
            };
            Self::verify_started_by(&key, &won, &input)?;
            self.bind(&key, &case, &input.variables)?;
            return Ok(StartOutcome::Existing { case });
        }
        self.bind(&key, &case, &input.variables)?;
        Ok(StartOutcome::Started { case })
    }
}
