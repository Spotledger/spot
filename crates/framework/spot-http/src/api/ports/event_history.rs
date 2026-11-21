//! The history seam of the event stream routes (contract 0.16.0, WP-JRN-09.T3): the consumer
//! selectors `classes` and `entities` of `event_consumer_put`, and `served_from`.
//!
//! `spot-http` does not depend on the event-stream engine or the journal. This file holds what
//! the routes decide on their own and the one question they ask the host:
//!
//! - [`parse_selectors`]: `classes` (`state`, `domain`, `audit`; absent means `domain` and
//!   `audit`; telemetry is never a class) and `entities` (DocType names; only meaningful when
//!   `classes` contains `state`). An unknown class, telemetry, a duplicate, too many entries or
//!   `entities` without `state` is `422 eventstream.spec_invalid`.
//! - [`ServedFrom`]: where an answer came from, as the contract's `served_from` member, the
//!   `X-Served-From` header of the stream and the `served_from` SSE frame.
//! - [`EventHistoryPort`]: whether the tenant has a journal that can serve history, so a route
//!   knows whether an older checkpoint is `journal` or `410 eventstream.resync_required`.
//!
//! **Not wired by this task.** `routes/events.rs` and `ports/events.rs` (WP-INT-02.T4) own the
//! consumer operations and are not part of this task's files: they call these functions once the
//! lead widens them (see the task report).

use std::collections::BTreeSet;

use serde_json::Value as Json;

use super::PortKey;
use crate::api::registry::Code;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`EventHistoryPort`].
pub const KEY: PortKey = PortKey::of::<dyn EventHistoryPort>();

/// The most DocType names `entities` may hold (the contract's `maxItems`).
pub const MAX_ENTITIES: usize = 64;
/// The longest DocType name (the contract's `maxLength`).
pub const MAX_ENTITY_LEN: usize = 200;

/// An event class a consumer can ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClassIn {
    /// Full document images (`doc_state`); needs the reserved-class stream right.
    State,
    /// Business facts.
    Domain,
    /// The audit-grade `sys_` and `identity_` families; needs the reserved-class stream right.
    Audit,
}

impl ClassIn {
    /// The contract's text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Domain => "domain",
            Self::Audit => "audit",
        }
    }

    /// Whether naming this class needs the reserved-class stream right (`403 permission.denied`
    /// without it: the port decides, the route only knows which classes were named).
    #[must_use]
    pub fn is_reserved(self) -> bool {
        matches!(self, Self::State | Self::Audit)
    }
}

/// What the request body's selectors came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorsIn {
    /// The classes, in contract order; `domain` and `audit` when the body named none.
    pub classes: Vec<ClassIn>,
    /// The DocType names narrowing `state` events; empty means every entity.
    pub entities: Vec<String>,
    /// Whether the body named `classes` at all (a consumer registered before 0.16.0 did not).
    pub classes_named: bool,
}

fn spec_invalid(message: &str) -> ApiError {
    ApiError::new(Code::EventstreamSpecInvalid, message)
}

/// Reads `classes` and `entities` out of a consumer spec body.
///
/// # Errors
/// `422 eventstream.spec_invalid` for an unknown class or telemetry, a duplicate class or
/// entity, a member of the wrong type, more than [`MAX_ENTITIES`] entities, a name longer than
/// [`MAX_ENTITY_LEN`] or empty, and `entities` named without `state` in the classes.
pub fn parse_selectors(body: &Json) -> Result<SelectorsIn, ApiError> {
    let classes_named = body.get("classes").is_some_and(|v| !v.is_null());
    let classes = match body.get("classes") {
        None | Some(Json::Null) => vec![ClassIn::Domain, ClassIn::Audit],
        Some(Json::Array(items)) => {
            let mut out = BTreeSet::new();
            for item in items {
                let class = match item.as_str() {
                    Some("state") => ClassIn::State,
                    Some("domain") => ClassIn::Domain,
                    Some("audit") => ClassIn::Audit,
                    Some("telemetry") => {
                        return Err(spec_invalid("telemetry is never in the stream"));
                    }
                    _ => return Err(spec_invalid("`classes` holds an unknown class")),
                };
                if !out.insert(class) {
                    return Err(spec_invalid("`classes` names a class twice"));
                }
            }
            if out.is_empty() {
                return Err(spec_invalid("`classes` is empty"));
            }
            out.into_iter().collect()
        }
        Some(_) => return Err(spec_invalid("`classes` must be a list")),
    };
    let entities = match body.get("entities") {
        None | Some(Json::Null) => Vec::new(),
        Some(Json::Array(items)) => {
            if items.len() > MAX_ENTITIES {
                return Err(spec_invalid("`entities` holds too many names"));
            }
            let mut out: Vec<String> = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .filter(|n| !n.is_empty() && n.len() <= MAX_ENTITY_LEN)
                    .ok_or_else(|| {
                        spec_invalid("`entities` holds a name that is empty or too long")
                    })?;
                if out.iter().any(|seen| seen == name) {
                    return Err(spec_invalid("`entities` names an entity twice"));
                }
                out.push(name.to_owned());
            }
            out
        }
        Some(_) => return Err(spec_invalid("`entities` must be a list")),
    };
    if !entities.is_empty() && !classes.contains(&ClassIn::State) {
        return Err(spec_invalid(
            "`entities` narrows `state` events, so `classes` must contain `state`",
        ));
    }
    Ok(SelectorsIn {
        classes,
        entities,
        classes_named,
    })
}

/// Where an answer came from (the contract's `served_from`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServedFrom {
    /// The live outbox.
    Outbox,
    /// The tenant's journal answered because the checkpoint predates the outbox horizon.
    Journal,
}

impl ServedFrom {
    /// The contract's text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outbox => "outbox",
            Self::Journal => "journal",
        }
    }

    /// The `X-Served-From` header value of an event stream.
    #[must_use]
    pub fn header_value(self) -> &'static str {
        self.as_str()
    }

    /// The `served_from` SSE frame sent when a stream passes from history to the live feed:
    /// `event: served_from`, `data: {"served_from": "outbox"}`, nothing else.
    #[must_use]
    pub fn frame(self) -> Vec<u8> {
        format!(
            "event: served_from\ndata: {{\"served_from\":\"{}\"}}\n\n",
            self.as_str()
        )
        .into_bytes()
    }

    /// `served_from` as the answer member of a pull.
    #[must_use]
    pub fn member(self) -> Json {
        Json::String(self.as_str().to_owned())
    }
}

/// Whether the tenant of a request has a journal that can serve history.
pub trait EventHistoryPort: Send + Sync + 'static {
    /// `true` when an older checkpoint is served from the journal, `false` when it is a `410`.
    fn has_history<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, bool>;
}
