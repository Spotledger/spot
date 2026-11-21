//! The two node handlers of a conversation: send a contract message, wait for one.
//!
//! Both are registered with `spot-bpm-engine` through [`register`]. They are synchronous and keep
//! the engine's rule that a handler's side effect is keyed by the attempt's idempotency key:
//! the send decides its message id from `(case, node, attempt)` and records the decision before
//! queueing, so a replay after a crash queues the same message once.
//!
//! # What a node sees of a conversation
//!
//! The Case's variables carry `bpm.conv.<contract>.{id, link}` once the conversation exists: the
//! first send creates it, a message that started the Case sets it. A received message that a wait
//! consumed is written to `bpm.received.<message>`. A send node's payload is the object at the
//! variable path its `mapping` names (default `bpm.out`).
//!
//! # Branches
//!
//! A node records how it ended in the Case variables, and the process routes on that with the
//! ordinary edge conditions of the shared expression language, exactly as after a branch node:
//!
//! - a wait writes `bpm.wait.<node>.result` (`received` or `timeout`), and a received message's
//!   fields to `bpm.received.<message>`;
//! - a send writes `bpm.send.<node>.result` (`sent`, or `rejected` when the receiver refused it
//!   and the node continues) and, for a refusal, `bpm.send.<node>.reason`.
//!
//! So the edges leaving a wait carry conditions such as
//! `doc.bpm.wait.wait_approval.result == "received"` and `... == "timeout"`.

use std::sync::Arc;

use serde_json::{Value, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{HandlerRegistry, NodeCtx, NodeHandler, NodeKind, NodeOutcome, WaitSpec};
use spot_bpm_model::{RejectAction, TimeoutAction};
use spot_link_exchange::{Conversation, MessageState};
use spot_tenant_link::{LinkId, Topic};

use crate::error::{ConversationError, Result, Violation, codes};
use crate::ledger::{ensure_record, record_sent};
use crate::order::check_message;
use crate::record::{ConversationRecord, Intent, Wait, WaitFor, WaitKind, update};
use crate::resolve::{Resolution, route};
use crate::runtime::Runtime;
use crate::sender::{MessageSender, SendRequest, Sent, message_id};
use crate::wire::{Header, encode};

/// The variable path a send node's payload comes from when its `mapping` names none.
// See `wire::MESSAGE_FIELD` for why this is built with `concat!`.
pub const DEFAULT_PAYLOAD_PATH: &str = concat!("bpm", ".out");

/// Registers the conversation handlers with the engine.
///
/// The engine maps the `send_message` and `wait_message` node kinds to its `unknown` kind until it
/// names them, so one dispatcher is registered for that key and tells the two apart by their
/// config; the typed handlers are registered under their own tokens for the day the engine does.
pub fn register(registry: &mut HandlerRegistry, runtime: &Arc<Runtime>) {
    registry.register(ConversationHandlers::new(runtime.clone()));
    registry.register(SendHandler::new(runtime.clone()));
    registry.register(WaitHandler::new(runtime.clone()));
}

/// Instants as Unix milliseconds (0 before the epoch).
#[must_use]
pub fn unix_ms(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp_nanos().div_euclid(1_000_000)).unwrap_or(0)
}

fn config_text<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

fn fail(error: &ConversationError) -> NodeOutcome {
    NodeOutcome::fail(error.code(), error.to_string())
}

/// The conversation a Case holds for a contract, from its variables.
fn conversation_of(ctx: &NodeCtx<'_>, contract: &str) -> Option<(Conversation, LinkId)> {
    let id = ctx
        .variable(&format!("bpm.conv.{contract}.id"))?
        .as_str()?
        .to_owned();
    let link = ctx
        .variable(&format!("bpm.conv.{contract}.link"))?
        .as_str()?
        .to_owned();
    Some((Conversation::new(id).ok()?, link.parse().ok()?))
}

fn remember_conversation(ctx: &mut NodeCtx<'_>, contract: &str, record: &ConversationRecord) {
    ctx.set_variable(
        &format!("bpm.conv.{contract}.id"),
        Value::String(record.conversation.to_string()),
    );
    ctx.set_variable(
        &format!("bpm.conv.{contract}.link"),
        Value::String(record.link.to_string()),
    );
}

/// The conversation name for a request's conversation on a contract and link: a pure function,
/// so a replay that lost the variables derives the same name.
fn derive_conversation(link: &LinkId, case: &str, contract: &str) -> Result<Conversation> {
    let text = format!("{link}|{case}|{contract}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Conversation::new(format!("c-{hash:016x}")).map_err(|_| ConversationError::Config {
        detail: "the conversation name could not be derived",
    })
}

/// How far a delivery state has got relative to what a send step waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Not far enough yet.
    Pending,
    /// Far enough.
    Reached,
    /// The receiver refused it, or it expired; the text is the stable reason.
    Refused(&'static str),
}

pub(crate) fn classify(state: MessageState, wait_for: WaitFor) -> Delivery {
    match state {
        MessageState::Rejected(code) => Delivery::Refused(code.code()),
        MessageState::Expired => Delivery::Refused("expired"),
        MessageState::Applied => Delivery::Reached,
        MessageState::Acknowledged if wait_for != WaitFor::Applied => Delivery::Reached,
        _ if wait_for == WaitFor::Queued => Delivery::Reached,
        _ => Delivery::Pending,
    }
}

/// The resolution of a delivery wait that ended in a refusal.
pub(crate) fn refusal_resolution(
    on_reject: RejectAction,
    node: &str,
    reason: &'static str,
) -> Resolution {
    match on_reject {
        RejectAction::ErrorBranch => Resolution::Failed {
            code: if reason == "expired" {
                codes::DELIVERY_EXPIRED
            } else {
                codes::REJECTED
            },
            reason: format!("the receiver did not take the message ({reason})"),
        },
        _ => Resolution::Done {
            output: Some(json!({ "rejected": reason })),
            set: vec![
                (format!("bpm.send.{node}.result"), json!("rejected")),
                (format!("bpm.send.{node}.reason"), json!(reason)),
            ],
        },
    }
}

/// The resolution of a send whose delivery reached what the node waits for.
pub(crate) fn sent_resolution(node: &str, output: Value) -> Resolution {
    Resolution::Done {
        output: Some(output),
        set: vec![(format!("bpm.send.{node}.result"), json!("sent"))],
    }
}

/// Turns a resolution into the outcome a handler returns: variables are written to the context
/// and the branches are the outgoing conditions that hold.
fn outcome_of(ctx: &mut NodeCtx<'_>, resolution: Resolution) -> NodeOutcome {
    match resolution {
        Resolution::Failed { code, reason } => NodeOutcome::fail(code, reason),
        Resolution::Done { output, set } => {
            for (path, value) in set {
                ctx.set_variable(&path, value);
            }
            let branches = route(&ctx.outgoing_conditions(), ctx.variables, ctx.now);
            NodeOutcome::Done { output, branches }
        }
    }
}

fn parse_wait_for(config: &Value) -> Result<Option<WaitFor>> {
    match config_text(config, "wait_for") {
        None => Ok(None),
        Some(token) => WaitFor::parse(token)
            .map(Some)
            .ok_or(ConversationError::Config {
                detail: "wait_for is not queued, acknowledged or applied",
            }),
    }
}

/// The resolution of a wait that a message satisfied.
pub(crate) fn received_resolution(node: &str, message: &str, payload: Value) -> Resolution {
    Resolution::Done {
        output: Some(payload.clone()),
        set: vec![
            (format!("bpm.received.{message}"), payload),
            (format!("bpm.wait.{node}.result"), json!("received")),
        ],
    }
}

/// The resolution of a wait whose time limit passed and takes the timeout path.
pub(crate) fn timeout_resolution(node: &str) -> Resolution {
    Resolution::Done {
        output: Some(json!({ "timed_out": true })),
        set: vec![(format!("bpm.wait.{node}.result"), json!("timeout"))],
    }
}

/// Sends one contract message (`send_message` node).
pub struct SendHandler {
    runtime: Arc<Runtime>,
}

impl SendHandler {
    /// A handler over the shared runtime.
    #[must_use]
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    fn run_send(&self, ctx: &mut NodeCtx<'_>) -> Result<NodeOutcome> {
        let rt = &self.runtime;
        let contract_id = config_text(ctx.config, "contract")
            .ok_or(ConversationError::Config {
                detail: "a send node names a contract",
            })?
            .to_owned();
        let name = config_text(ctx.config, "message")
            .ok_or(ConversationError::Config {
                detail: "a send node names a message",
            })?
            .to_owned();
        let mapping = config_text(ctx.config, "mapping")
            .unwrap_or(DEFAULT_PAYLOAD_PATH)
            .to_owned();
        let on_reject = match config_text(ctx.config, "on_reject") {
            Some("continue") => RejectAction::Continue,
            _ => RejectAction::ErrorBranch,
        };
        let wait_for = parse_wait_for(ctx.config)?.unwrap_or(rt.default_wait_for);
        let key = ctx.key.to_string();
        let now_ms = unix_ms(ctx.now);
        let case = ctx.case.to_string();

        let record = self.open(ctx, &contract_id, &name)?;
        let contract = rt
            .book
            .get(&record.contract, record.contract_version)
            .ok_or(Violation::ContractUnknown)?
            .clone();
        let def = contract
            .message(&name)
            .ok_or(Violation::MessageUnknown)?
            .clone();
        remember_conversation(ctx, &contract_id, &record);
        let sender = rt
            .peers
            .sender(&record.link)
            .ok_or(ConversationError::NoPeer)?;

        let payload = ctx
            .variable(&mapping)
            .filter(|value| value.is_object())
            .cloned()
            .ok_or(ConversationError::PayloadMissing)?;

        // The decision (the id) is kept before the send so a replay sends the same message, but
        // nothing that says "sent" is recorded until the sender accepted it: a refused send must
        // leave the conversation as it was.
        let id = match record.intents.get(&key) {
            Some(intent) => intent.id,
            None => {
                let id = message_id(now_ms, &record.link, &key);
                update(&*rt.store, &record.link, &record.conversation, |r| {
                    if r.intents.contains_key(&key) {
                        return Ok(());
                    }
                    check_message(
                        &contract,
                        &r.role,
                        &def.to,
                        &name,
                        def.schema_version,
                        &payload,
                        &r.order_state(),
                    )?;
                    r.intents.insert(
                        key.clone(),
                        Intent {
                            id,
                            message: name.clone(),
                        },
                    );
                    Ok(())
                })?;
                id
            }
        };
        // A crash after the sender took the message and before this record was written leaves the
        // outbox holding a message the record does not know: record it from the outbox before
        // anything judges the order, or the replay's own check (and the peer's reply) fails.
        record_sent(rt, &record.link, &record.conversation, now_ms)?;
        // Whatever the first attempt decided, the id is the recorded one: re-read it so a
        // concurrent decision for the same key wins consistently.
        let current = rt.store.load(&record.link, &record.conversation)?;
        let id = current
            .as_ref()
            .and_then(|r| r.intents.get(&key).map(|intent| intent.id))
            .unwrap_or(id);
        if let Some(current) = &current
            && !current.has_event(&id)
        {
            // A replay after a refused send checks the order again against the record as it is.
            check_message(
                &contract,
                &current.role,
                &def.to,
                &name,
                def.schema_version,
                &payload,
                &current.order_state(),
            )?;
        }

        let header = Header::new(contract.id.clone(), contract.version, name.clone());
        let wire = encode(&header, &payload, &def)?;
        let topic = Topic::new(def.topic.clone()).map_err(|_| ConversationError::Config {
            detail: "the contract topic is not a link topic",
        })?;
        let actor = ctx.principal.map_or_else(
            || "system".to_owned(),
            |principal| format!("user:{principal}"),
        );
        let sent = sender.send(&SendRequest {
            conversation: &record.conversation,
            topic: &topic,
            schema_version: def.schema_version,
            id,
            payload: &wire,
            now_ms,
            actor: &actor,
        });
        let sent = match sent {
            Ok(sent) => sent,
            Err(error) => {
                // Forget the decision when the sender never took the message, so a later retry
                // starts clean; when it did take it, the decision stays and the id with it.
                // An error from `state` keeps the intent: the sender may hold the message.
                if matches!(sender.state(&id), Ok(None)) {
                    let _ = update(&*rt.store, &record.link, &record.conversation, |r| {
                        if r.intents.get(&key).is_some_and(|i| i.id == id) && !r.has_event(&id) {
                            r.intents.remove(&key);
                        }
                        Ok(())
                    });
                }
                return Err(error);
            }
        };
        update(&*rt.store, &record.link, &record.conversation, |r| {
            r.append_sent(&contract, &name, id, now_ms)?;
            if r.case.is_none() {
                r.case = Some(case.clone());
            }
            Ok(())
        })?;
        self.after_send(ctx, &record, &*sender, sent, wait_for, on_reject)
    }

    /// Finds or opens the conversation this send belongs to.
    fn open(&self, ctx: &NodeCtx<'_>, contract_id: &str, name: &str) -> Result<ConversationRecord> {
        let rt = &self.runtime;
        if let Some((conversation, link)) = conversation_of(ctx, contract_id) {
            return rt
                .store
                .load(&link, &conversation)?
                .ok_or(ConversationError::NoConversation);
        }
        let contract = rt
            .book
            .latest(contract_id)
            .ok_or(Violation::ContractUnknown)?;
        let role = rt
            .book
            .role(contract_id)
            .ok_or(Violation::ContractUnknown)?;
        let def = contract.message(name).ok_or(Violation::MessageUnknown)?;
        if def.from != role {
            return Err(Violation::WrongDirection.into());
        }
        let link = rt
            .peers
            .route(contract_id, &def.to)
            .ok_or(ConversationError::NoPeer)?;
        let conversation = derive_conversation(&link, &ctx.case.to_string(), contract_id)?;
        ensure_record(rt, &link, &conversation, contract, role, &def.to)
    }

    fn after_send(
        &self,
        ctx: &mut NodeCtx<'_>,
        record: &ConversationRecord,
        sender: &dyn MessageSender,
        sent: Sent,
        wait_for: WaitFor,
        on_reject: RejectAction,
    ) -> Result<NodeOutcome> {
        let state = sender.state(&sent.id)?;
        let delivery = match state {
            Some(state) => classify(state, wait_for),
            None => Delivery::Pending,
        };
        let output = json!({
            "message_id": sent.id.to_string(),
            "conversation": record.conversation.to_string(),
        });
        let node = ctx.node.to_string();
        match delivery {
            Delivery::Reached => Ok(outcome_of(ctx, sent_resolution(&node, output))),
            Delivery::Refused(reason) => Ok(outcome_of(
                ctx,
                refusal_resolution(on_reject, &node, reason),
            )),
            Delivery::Pending => {
                let attempt = ctx.attempt;
                let id = sent.id;
                update(
                    &*self.runtime.store,
                    &record.link,
                    &record.conversation,
                    |r| {
                        r.register_wait(Wait {
                            node: node.clone(),
                            attempt,
                            kind: WaitKind::Delivery {
                                id,
                                wait_for,
                                on_reject,
                            },
                            due_ms: None,
                        })
                    },
                )?;
                Ok(NodeOutcome::Wait(WaitSpec::ForEvent {
                    key: format!("delivery:{id}"),
                    due: None,
                }))
            }
        }
    }
}

impl NodeHandler for SendHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Unknown("send_message".to_owned())
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        self.run_send(ctx).unwrap_or_else(|error| fail(&error))
    }
}

/// Waits for one contract message (`wait_message` node).
pub struct WaitHandler {
    runtime: Arc<Runtime>,
}

/// What a wait attempt found in the record.
enum Found {
    Consumed(Value),
    Registered,
}

impl WaitHandler {
    /// A handler over the shared runtime.
    #[must_use]
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    fn run_wait(&self, ctx: &mut NodeCtx<'_>) -> Result<NodeOutcome> {
        let rt = &self.runtime;
        let contract_id = config_text(ctx.config, "contract")
            .ok_or(ConversationError::Config {
                detail: "a wait node names a contract",
            })?
            .to_owned();
        let name = config_text(ctx.config, "message")
            .ok_or(ConversationError::Config {
                detail: "a wait node names a message",
            })?
            .to_owned();
        let on_timeout = match config_text(ctx.config, "on_timeout") {
            Some("timeout_edge") => TimeoutAction::TimeoutEdge,
            _ => TimeoutAction::ErrorBranch,
        };
        let (conversation, link) =
            conversation_of(ctx, &contract_id).ok_or(ConversationError::NoConversation)?;
        let record = rt
            .store
            .load(&link, &conversation)?
            .ok_or(ConversationError::NoConversation)?;
        let contract = rt
            .book
            .get(&record.contract, record.contract_version)
            .ok_or(Violation::ContractUnknown)?
            .clone();
        let def = contract.message(&name).ok_or(Violation::MessageUnknown)?;
        if def.to != record.role {
            return Err(Violation::WrongDirection.into());
        }
        let now_ms = unix_ms(ctx.now);
        let limit_ms = wait_limit_ms(
            ctx,
            contract.timeout(&name, &record.role).map(|t| t.seconds),
        );
        let due_ms = limit_ms.map(|limit| now_ms.saturating_add(limit));
        let node = ctx.node.to_string();
        let attempt = ctx.attempt;
        let case = ctx.case.to_string();

        let (found, _) = update(&*rt.store, &link, &conversation, |r| {
            if let Some(id) = r.consumed_by.get(&(node.clone(), attempt)).copied() {
                // A replay of an attempt that already took its message: the same one again.
                let payload = r
                    .events
                    .iter()
                    .find(|event| event.id == id)
                    .and_then(|event| event.payload.clone())
                    .unwrap_or(Value::Null);
                r.waits.retain(|w| w.node != node);
                return Ok(Found::Consumed(payload));
            }
            if let Some(event) = r.unconsumed(&name) {
                let (id, payload) = (event.id, event.payload.clone().unwrap_or(Value::Null));
                r.consumed.insert(id);
                r.consumed_by.insert((node.clone(), attempt), id);
                r.waits.retain(|w| w.node != node);
                return Ok(Found::Consumed(payload));
            }
            if r.ending.is_some() {
                return Err(Violation::ConversationEnded.into());
            }
            let Some(due_ms) = due_ms else {
                return Err(ConversationError::Config {
                    detail: "a wait for a message needs a time limit",
                });
            };
            r.register_wait(Wait {
                node: node.clone(),
                attempt,
                kind: WaitKind::Message {
                    message: name.clone(),
                    on_timeout,
                },
                due_ms: Some(due_ms),
            })?;
            if r.case.is_none() {
                r.case = Some(case.clone());
            }
            Ok(Found::Registered)
        })?;
        match found {
            Found::Consumed(payload) => {
                Ok(outcome_of(ctx, received_resolution(&node, &name, payload)))
            }
            Found::Registered => Ok(NodeOutcome::Wait(WaitSpec::ForEvent {
                key: format!("msg:{conversation}:{name}"),
                due: None,
            })),
        }
    }
}

/// The wait's limit in milliseconds: the contract's limit for this wait, or the time left to the
/// node's own deadline, whichever is shorter.
fn wait_limit_ms(ctx: &NodeCtx<'_>, contract_seconds: Option<u64>) -> Option<u64> {
    let contract = contract_seconds.map(|seconds| seconds.saturating_mul(1_000));
    let node = ctx
        .deadline
        .map(|deadline| u64::try_from((deadline - ctx.now).whole_milliseconds()).unwrap_or(0));
    match (contract, node) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

impl NodeHandler for WaitHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Unknown("wait_message".to_owned())
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        self.run_wait(ctx).unwrap_or_else(|error| fail(&error))
    }
}

/// The dispatcher registered for the engine's `unknown` kind, which is what the engine calls a
/// `send_message` or `wait_message` node until it names them.
///
/// TODO(WP-BPM-12 follow-up, tracked by the WP-BPM-12 task issue for this crate): the engine's
/// `NodeKind` has no `SendMessage`/`WaitMessage`, so both steps arrive as
/// `NodeKind::Unknown("unknown")` and are told apart by which optional config field they carry —
/// a node with neither is neither step. Delete this dispatcher and register the two handlers
/// under their own kinds as soon as `spot-bpm-engine` names them; until then a node that names
/// both fields silently runs as a send.
pub struct ConversationHandlers {
    send: SendHandler,
    wait: WaitHandler,
}

impl ConversationHandlers {
    /// The dispatcher over the shared runtime.
    #[must_use]
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self {
            send: SendHandler::new(runtime.clone()),
            wait: WaitHandler::new(runtime),
        }
    }
}

impl NodeHandler for ConversationHandlers {
    fn kind(&self) -> NodeKind {
        NodeKind::Unknown("unknown".to_owned())
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        // The two configs are told apart by the field only each has.
        if ctx.config.get("on_reject").is_some() {
            self.send.run(ctx)
        } else if ctx.config.get("on_timeout").is_some() {
            self.wait.run(ctx)
        } else {
            NodeOutcome::fail(codes::CONFIG_INVALID, "the node is not a conversation node")
        }
    }
}
