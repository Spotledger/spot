//! The connector seam of native API 0.25.0 (`connector_list`, `connector_get`,
//! `connector_invoke`, `connector_runs` and the unauthenticated `connector_inbound`,
//! WP-INT-05.T4).
//!
//! `spot-http` does not depend on the connector service. It defines [`ConnectorsPort`], the one
//! seam the route group needs; the adapter over the connector service of `spot-cli`
//! (`services/connectors.rs`, WP-INT-05.T3) is a host task. [`MemoryConnectorsPort`] is the
//! in-memory fake the tests use.
//!
//! # Who decides what
//!
//! For the four authenticated operations the port decides **everything about rights**: who the
//! caller is (from `ctx.credential` on `ctx.site`), which connectors the caller may see, whether
//! the caller may invoke an operation (as the calling user, with that user's rights) or read a
//! run log, the audit of every refusal and the replay of a repeated `Idempotency-Key`. The
//! routes only shape: they validate the request, hand the port the authenticated context and
//! render the answer. A connector the caller may not see is [`ConnectorPortError::NotFound`],
//! the answer a missing one gets (no existence oracle). No type in this file can carry a
//! credential, a secret reference value or an endpoint secret **to a client**: the views have no
//! such field.
//!
//! # The ingress
//!
//! `connector_inbound` has no caller. The route checks the signature itself, in constant time,
//! before it parses a byte of the body, and the port supplies only what the check needs:
//!
//! 1. [`ConnectorsPort::admit_inbound`]: the per-source and per-connector rate limit, before the
//!    body is read;
//! 2. [`ConnectorsPort::inbound_credential`]: the connector's declared credential, or `None` for
//!    an unknown, disabled-for-ingress or other-tenant connector. The route then verifies a dummy
//!    credential so the two cases cost the same;
//! 3. [`ConnectorsPort::inbound_receive`]: called **only for a request that verified**: records
//!    the sender's delivery id (a repeat is [`InboundRefusal::Replayed`]) and enqueues the job
//!    that runs as the connector's service principal.
//!
//! [`ConnectorsPort::now_unix`] is the injected clock the route reads the timestamp window
//! through; the route never reads the wall clock itself.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde_json::{Map, Value as Json};

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`ConnectorsPort`].
pub const KEY: PortKey = PortKey::of::<dyn ConnectorsPort>();

/// How long a delivery id is remembered, in seconds (24 hours).
pub const REPLAY_WINDOW_SECS: i64 = 86_400;

/// A connector's state (`Connector.state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorState {
    /// Accepting calls.
    Enabled,
    /// Switched off.
    Disabled,
    /// Enabled, but recent runs fail.
    Failing,
}

impl ConnectorState {
    /// The contract's token.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectorState::Enabled => "enabled",
            ConnectorState::Disabled => "disabled",
            ConnectorState::Failing => "failing",
        }
    }
}

/// How a run ended (`outcome`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// It succeeded.
    Ok,
    /// It failed.
    Failed,
}

impl RunOutcome {
    /// The contract's token.
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Ok => "ok",
            RunOutcome::Failed => "failed",
        }
    }
}

/// One operation of a connector, with its schemas.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationView {
    /// The operation name (at most 100 characters).
    pub name: String,
    /// What it does (at most 500 characters).
    pub description: Option<String>,
    /// The JSON Schema of its input.
    pub input_schema: Map<String, Json>,
    /// The JSON Schema of its output.
    pub output_schema: Map<String, Json>,
}

/// The last run of a connector (`Connector.last_run`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastRun {
    /// When, a UTC RFC 3339 instant.
    pub at: String,
    /// The operation.
    pub operation: String,
    /// The outcome.
    pub outcome: RunOutcome,
    /// The error code of a failure.
    pub code: Option<String>,
}

/// The contract's `Connector`: no credential, secret reference or endpoint secret is in it.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorView {
    /// The connector's name (at most 100 characters).
    pub name: String,
    /// Its kind, for example `http` or `wasm`.
    pub kind: Option<String>,
    /// The state.
    pub state: ConnectorState,
    /// The operations (at most 200).
    pub operations: Vec<OperationView>,
    /// The last run, when there was one.
    pub last_run: Option<LastRun>,
}

/// One line of the run log: never an input or an output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunView {
    /// When, a UTC RFC 3339 instant.
    pub at: String,
    /// The operation.
    pub operation: String,
    /// The outcome.
    pub outcome: RunOutcome,
    /// How long it took, in milliseconds.
    pub duration_ms: u64,
    /// The error code of a failure.
    pub code: Option<String>,
}

/// The paging parameters of the two list operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageIn {
    /// The `next_cursor` of the previous page (1 to 2048 visible characters).
    pub cursor: Option<String>,
    /// The page size, 1 to 100.
    pub limit: usize,
}

/// One page of connectors.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorPage {
    /// The connectors the caller may see, at most `limit`.
    pub connectors: Vec<ConnectorView>,
    /// The cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// One page of the run log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunsPage {
    /// Runs, newest first, at most `limit`.
    pub runs: Vec<RunView>,
    /// The cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// What an invocation produced.
#[derive(Clone, Debug, PartialEq)]
pub struct InvokeOut {
    /// The output object (empty when the operation went to a job).
    pub output: Map<String, Json>,
    /// The job id when the operation is long: the answer is then `202`.
    pub job_id: Option<String>,
    /// The answer replays an earlier call with the same `Idempotency-Key`.
    pub replayed: bool,
}

/// Why an authenticated operation was refused. No variant carries text: the routes answer with
/// the registry's message, so a port cannot leak a value through an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectorPortError {
    /// No connector the caller may see has this name.
    NotFound,
    /// The connector is disabled.
    Disabled,
    /// The connector has no operation with this name.
    OperationUnknown,
    /// The input does not fit the input schema; the paths of the offending members, never
    /// values.
    InputInvalid {
        /// JSON-pointer-like paths.
        fields: Vec<String>,
    },
    /// The external system failed or could not be reached.
    UpstreamFailed,
    /// The operation did not finish in time.
    DeadlineExceeded,
    /// The caller lacks the right.
    PermissionDenied,
    /// The `Idempotency-Key` was used with another request.
    IdempotencyConflict,
    /// The cursor is not one the port issued.
    CursorInvalid,
    /// The credential is missing, expired or revoked.
    Unauthenticated,
    /// The service is temporarily unavailable.
    Unavailable,
    /// Anything else; nothing about it reaches the client.
    Internal,
}

/// Why a verified inbound request was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundRefusal {
    /// The sender's delivery id was already accepted within the window.
    Replayed,
    /// The connector is disabled.
    Disabled,
    /// The service is temporarily unavailable.
    Unavailable,
    /// Anything else.
    Internal,
}

/// The credential a connector declared for its ingress. The bytes are secret: the type has no
/// `Display`, no serialisation, and its `Debug` shows the scheme only.
#[derive(Clone, PartialEq, Eq)]
pub enum InboundCredential {
    /// The HMAC-SHA256 key of the `X-Spot-Signature: t=<unix>,v1=<hex>` scheme.
    Hmac(Vec<u8>),
    /// A bearer token (`Authorization: Bearer <token>`).
    Bearer(Vec<u8>),
}

impl std::fmt::Debug for InboundCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboundCredential::Hmac(_) => f.write_str("InboundCredential::Hmac(..)"),
            InboundCredential::Bearer(_) => f.write_str("InboundCredential::Bearer(..)"),
        }
    }
}

/// The connector service of one site, as the route group needs it.
pub trait ConnectorsPort: Send + Sync {
    /// The connectors the caller may invoke (`connector_list`), by name, paged.
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: &'a PageIn,
    ) -> BoxFuture<'a, Result<ConnectorPage, ConnectorPortError>>;

    /// One connector (`connector_get`).
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ConnectorView, ConnectorPortError>>;

    /// Runs `operation` of `name` as the calling user (`connector_invoke`). `ctx.idempotency_key`
    /// is always set (the route requires it); the port owns the replay.
    fn invoke<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        operation: &'a str,
        input: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<InvokeOut, ConnectorPortError>>;

    /// The run log (`connector_runs`).
    fn runs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        page: &'a PageIn,
    ) -> BoxFuture<'a, Result<RunsPage, ConnectorPortError>>;

    /// The current Unix time in seconds, from the host's injected clock.
    fn now_unix(&self) -> i64;

    /// Counts one ingress call against the per-source and the per-connector limits;
    /// `Err(seconds)` is the wait. It runs before the body is read and treats a known and an
    /// unknown connector name alike.
    fn admit_inbound(&self, ctx: &RequestCtx, name: &str) -> Result<(), u64>;

    /// The credential connector `name` declared for its ingress, or `None` for an unknown name
    /// (and for a connector of another tenant).
    fn inbound_credential<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Option<InboundCredential>>;

    /// Accepts a **verified** inbound request: remembers `delivery_id` for
    /// [`REPLAY_WINDOW_SECS`] (a repeat is [`InboundRefusal::Replayed`]) and enqueues the job
    /// that handles `body` as the connector's service principal.
    fn inbound_receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        delivery_id: Option<&'a str>,
        body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<(), InboundRefusal>>;
}

/// A small fixed-window limiter keyed by text, timed by an injected Unix time. Bounded: it
/// never holds more than [`FixedWindowLimiter::MAX_KEYS`] keys.
pub struct FixedWindowLimiter {
    limit: u32,
    window_secs: i64,
    windows: Mutex<HashMap<String, (i64, u32)>>,
}

impl FixedWindowLimiter {
    /// The most keys kept; when full, expired windows are dropped and a still-full table
    /// refuses a new key.
    pub const MAX_KEYS: usize = 4096;

    /// A limiter of `limit` calls per `window_secs`.
    pub fn new(limit: u32, window_secs: i64) -> Self {
        FixedWindowLimiter {
            limit,
            window_secs: window_secs.max(1),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Counts one call for `key` at `now`; `Err(seconds)` is the wait until the window ends.
    pub fn check(&self, key: &str, now: i64) -> Result<(), u64> {
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let wait = |start: i64| {
            u64::try_from(start.saturating_add(self.window_secs).saturating_sub(now))
                .unwrap_or(1)
                .max(1)
        };
        if let Some((start, count)) = windows.get_mut(key) {
            if now.saturating_sub(*start) >= self.window_secs {
                *start = now;
                *count = 1;
                return Ok(());
            }
            if *count >= self.limit {
                return Err(wait(*start));
            }
            *count += 1;
            return Ok(());
        }
        if windows.len() >= Self::MAX_KEYS {
            let window = self.window_secs;
            windows.retain(|_, (start, _)| now.saturating_sub(*start) < window);
            if windows.len() >= Self::MAX_KEYS {
                return Err(u64::try_from(window).unwrap_or(1));
            }
        }
        windows.insert(key.to_owned(), (now, 1));
        Ok(())
    }
}

/// A connector to seed [`MemoryConnectorsPort`] with.
#[derive(Clone, Debug)]
pub struct ConnectorSeed {
    /// The connector as the contract shows it.
    pub view: ConnectorView,
    /// The operations that end in a job (`202`), by name.
    pub long_operations: Vec<String>,
    /// The members each operation's input must carry, by operation name.
    pub required_input: BTreeMap<String, Vec<String>>,
    /// The output every operation answers with.
    pub output: Map<String, Json>,
    /// The ingress credential, when the connector has an ingress.
    pub inbound: Option<InboundCredential>,
}

impl ConnectorSeed {
    /// An enabled connector with one operation, `echo`, and no ingress.
    pub fn new(name: &str) -> Self {
        ConnectorSeed {
            view: ConnectorView {
                name: name.to_owned(),
                kind: Some("http".to_owned()),
                state: ConnectorState::Enabled,
                operations: vec![OperationView {
                    name: "echo".to_owned(),
                    description: Some("Echoes".to_owned()),
                    input_schema: Map::new(),
                    output_schema: Map::new(),
                }],
                last_run: None,
            },
            long_operations: Vec::new(),
            required_input: BTreeMap::new(),
            output: Map::new(),
            inbound: None,
        }
    }
}

/// What the fake remembers of one accepted inbound request.
#[derive(Clone, Debug, PartialEq)]
pub struct Enqueued {
    /// The site.
    pub site: String,
    /// The connector.
    pub connector: String,
    /// The sender's delivery id.
    pub delivery_id: Option<String>,
    /// The parsed body.
    pub body: Map<String, Json>,
}

/// What a caller resolves to: the user, the connectors visible (`None` = all), the run-log
/// right and the site.
type Caller = (String, Option<Vec<String>>, bool, String);

/// One recorded invocation: site, connector, operation, input.
pub type Invocation = (String, String, String, Map<String, Json>);

#[derive(Default)]
struct Access {
    /// The user the token names.
    user: String,
    /// The connectors the user may see and invoke; `None` means every connector.
    connectors: Option<Vec<String>>,
    /// The user may read run logs.
    runs: bool,
    /// The user may invoke operations.
    invoke: bool,
}

#[derive(Default)]
struct State {
    connectors: Vec<(String, ConnectorSeed)>,
    runs: Vec<(String, String, RunView)>,
    access: HashMap<(String, String), Access>,
    replay: HashMap<(String, String, String), i64>,
    idempotent: HashMap<(String, String, String), (String, InvokeOut)>,
    invocations: Vec<Invocation>,
    enqueued: Vec<Enqueued>,
    credential_lookups: usize,
    receive_calls: usize,
    now: i64,
}

/// The in-memory [`ConnectorsPort`]. The caller is the credential's token itself (as in
/// `MemoryTaskPort`): a test registers tokens with [`MemoryConnectorsPort::grant`].
pub struct MemoryConnectorsPort {
    state: Mutex<State>,
    per_source: FixedWindowLimiter,
    per_connector: FixedWindowLimiter,
}

impl Default for MemoryConnectorsPort {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryConnectorsPort {
    /// The default ingress limits of the fake: 600 calls per minute per source, 300 per
    /// connector.
    pub const SOURCE_LIMIT: u32 = 600;
    /// See [`Self::SOURCE_LIMIT`].
    pub const CONNECTOR_LIMIT: u32 = 300;

    /// An empty fake whose clock stands at Unix time 1,800,000,000.
    pub fn new() -> Self {
        Self::with_limits(Self::SOURCE_LIMIT, Self::CONNECTOR_LIMIT)
    }

    /// An empty fake with explicit per-minute ingress limits.
    pub fn with_limits(per_source: u32, per_connector: u32) -> Self {
        MemoryConnectorsPort {
            state: Mutex::new(State {
                now: 1_800_000_000,
                ..State::default()
            }),
            per_source: FixedWindowLimiter::new(per_source, 60),
            per_connector: FixedWindowLimiter::new(per_connector, 60),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The mutex guards plain collections; a panic elsewhere cannot leave them inconsistent.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Adds a connector on `site`.
    pub fn add_connector(&self, site: &str, seed: ConnectorSeed) {
        self.lock().connectors.push((site.to_owned(), seed));
    }

    /// Registers `token` on `site` as `user`, who may see and invoke every connector and may
    /// not read run logs.
    pub fn grant(&self, site: &str, token: &str, user: &str) {
        self.lock().access.insert(
            (site.to_owned(), token.to_owned()),
            Access {
                user: user.to_owned(),
                connectors: None,
                runs: false,
                invoke: true,
            },
        );
    }

    /// Restricts `token` to the connectors named (it sees and invokes only those).
    pub fn restrict(&self, site: &str, token: &str, connectors: &[&str]) {
        if let Some(access) = self
            .lock()
            .access
            .get_mut(&(site.to_owned(), token.to_owned()))
        {
            access.connectors = Some(connectors.iter().map(|c| (*c).to_owned()).collect());
        }
    }

    /// Takes the right to invoke operations from `token` (it may still list and get).
    pub fn revoke_invoke(&self, site: &str, token: &str) {
        if let Some(access) = self
            .lock()
            .access
            .get_mut(&(site.to_owned(), token.to_owned()))
        {
            access.invoke = false;
        }
    }

    /// Lets `token` read run logs.
    pub fn grant_runs(&self, site: &str, token: &str) {
        if let Some(access) = self
            .lock()
            .access
            .get_mut(&(site.to_owned(), token.to_owned()))
        {
            access.runs = true;
        }
    }

    /// Adds a run to the log of `connector` (the log lists newest first: add oldest first).
    pub fn add_run(&self, site: &str, connector: &str, run: RunView) {
        self.lock()
            .runs
            .push((site.to_owned(), connector.to_owned(), run));
    }

    /// Sets the clock (Unix seconds).
    pub fn set_now(&self, now: i64) {
        self.lock().now = now;
    }

    /// The invocations the port accepted: (site, connector, operation, input).
    pub fn invocations(&self) -> Vec<Invocation> {
        self.lock().invocations.clone()
    }

    /// The inbound requests that were accepted and enqueued.
    pub fn enqueued(&self) -> Vec<Enqueued> {
        self.lock().enqueued.clone()
    }

    /// How many times the route asked for an ingress credential.
    pub fn credential_lookups(&self) -> usize {
        self.lock().credential_lookups
    }

    /// How many times the route called [`ConnectorsPort::inbound_receive`] (it must stay zero
    /// for a request that did not verify).
    pub fn receive_calls(&self) -> usize {
        self.lock().receive_calls
    }

    fn caller(state: &State, ctx: &RequestCtx) -> Result<Caller, ConnectorPortError> {
        let token = ctx
            .credential
            .as_ref()
            .map(|c| c.token.clone())
            .ok_or(ConnectorPortError::Unauthenticated)?;
        let site = ctx.site.as_str().to_owned();
        let access = state
            .access
            .get(&(site.clone(), token))
            .ok_or(ConnectorPortError::Unauthenticated)?;
        Ok((
            access.user.clone(),
            access.connectors.clone(),
            access.runs,
            site,
        ))
    }

    fn visible(allowed: &Option<Vec<String>>, name: &str) -> bool {
        allowed
            .as_ref()
            .is_none_or(|list| list.iter().any(|n| n == name))
    }

    fn page_bounds(
        cursor: Option<&str>,
        limit: usize,
        total: usize,
    ) -> Result<(usize, usize), ConnectorPortError> {
        let start = match cursor {
            None => 0,
            Some(c) => c
                .strip_prefix('o')
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| *n <= total)
                .ok_or(ConnectorPortError::CursorInvalid)?,
        };
        Ok((start, (start + limit).min(total)))
    }
}

impl ConnectorsPort for MemoryConnectorsPort {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: &'a PageIn,
    ) -> BoxFuture<'a, Result<ConnectorPage, ConnectorPortError>> {
        Box::pin(async move {
            let state = self.lock();
            let (_, allowed, _, site) = Self::caller(&state, ctx)?;
            let mut visible: Vec<&ConnectorSeed> = state
                .connectors
                .iter()
                .filter(|(s, seed)| *s == site && Self::visible(&allowed, &seed.view.name))
                .map(|(_, seed)| seed)
                .collect();
            visible.sort_by(|a, b| a.view.name.cmp(&b.view.name));
            let (start, end) =
                Self::page_bounds(page.cursor.as_deref(), page.limit, visible.len())?;
            Ok(ConnectorPage {
                connectors: visible[start..end]
                    .iter()
                    .map(|seed| seed.view.clone())
                    .collect(),
                next_cursor: (end < visible.len()).then(|| format!("o{end}")),
            })
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Result<ConnectorView, ConnectorPortError>> {
        Box::pin(async move {
            let state = self.lock();
            let (_, allowed, _, site) = Self::caller(&state, ctx)?;
            state
                .connectors
                .iter()
                .find(|(s, seed)| *s == site && seed.view.name == name)
                .filter(|(_, seed)| Self::visible(&allowed, &seed.view.name))
                .map(|(_, seed)| seed.view.clone())
                .ok_or(ConnectorPortError::NotFound)
        })
    }

    fn invoke<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        operation: &'a str,
        input: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<InvokeOut, ConnectorPortError>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (user, allowed, _, site) = Self::caller(&state, ctx)?;
            let seed = state
                .connectors
                .iter()
                .find(|(s, seed)| *s == site && seed.view.name == name)
                .filter(|(_, seed)| Self::visible(&allowed, &seed.view.name))
                .map(|(_, seed)| seed.clone())
                .ok_or(ConnectorPortError::NotFound)?;
            let may_invoke = state
                .access
                .get(&(
                    site.clone(),
                    ctx.credential
                        .as_ref()
                        .map(|c| c.token.clone())
                        .unwrap_or_default(),
                ))
                .is_some_and(|a| a.invoke);
            if !may_invoke {
                return Err(ConnectorPortError::PermissionDenied);
            }
            let key = ctx
                .idempotency_key
                .clone()
                .ok_or(ConnectorPortError::Internal)?;
            let fingerprint = format!("{name}/{operation}/{}", Json::Object(input.clone()));
            let slot = (site.clone(), user.clone(), key);
            if let Some((first, out)) = state.idempotent.get(&slot) {
                if *first != fingerprint {
                    return Err(ConnectorPortError::IdempotencyConflict);
                }
                let mut out = out.clone();
                out.replayed = true;
                return Ok(out);
            }
            if seed.view.state == ConnectorState::Disabled {
                return Err(ConnectorPortError::Disabled);
            }
            if !seed.view.operations.iter().any(|o| o.name == operation) {
                return Err(ConnectorPortError::OperationUnknown);
            }
            let missing: Vec<String> = seed
                .required_input
                .get(operation)
                .map(|req| {
                    req.iter()
                        .filter(|m| !input.contains_key(*m))
                        .map(|m| format!("/{m}"))
                        .collect()
                })
                .unwrap_or_default();
            if !missing.is_empty() {
                return Err(ConnectorPortError::InputInvalid { fields: missing });
            }
            let long = seed.long_operations.iter().any(|o| o == operation);
            let out = InvokeOut {
                output: if long {
                    Map::new()
                } else {
                    seed.output.clone()
                },
                job_id: long.then(|| "01950000-0000-7000-8000-000000000001".to_owned()),
                replayed: false,
            };
            state
                .invocations
                .push((site, name.to_owned(), operation.to_owned(), input.clone()));
            state.idempotent.insert(slot, (fingerprint, out.clone()));
            Ok(out)
        })
    }

    fn runs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        page: &'a PageIn,
    ) -> BoxFuture<'a, Result<RunsPage, ConnectorPortError>> {
        Box::pin(async move {
            let state = self.lock();
            let (_, allowed, may_read, site) = Self::caller(&state, ctx)?;
            let known = state
                .connectors
                .iter()
                .any(|(s, seed)| *s == site && seed.view.name == name)
                && Self::visible(&allowed, name);
            if !known {
                return Err(ConnectorPortError::NotFound);
            }
            if !may_read {
                return Err(ConnectorPortError::PermissionDenied);
            }
            let runs: Vec<&RunView> = state
                .runs
                .iter()
                .rev()
                .filter(|(s, c, _)| *s == site && c == name)
                .map(|(_, _, r)| r)
                .collect();
            let (start, end) = Self::page_bounds(page.cursor.as_deref(), page.limit, runs.len())?;
            Ok(RunsPage {
                runs: runs[start..end].iter().map(|r| (*r).clone()).collect(),
                next_cursor: (end < runs.len()).then(|| format!("o{end}")),
            })
        })
    }

    fn now_unix(&self) -> i64 {
        self.lock().now
    }

    fn admit_inbound(&self, ctx: &RequestCtx, name: &str) -> Result<(), u64> {
        let now = self.lock().now;
        let source = ctx
            .peer
            .map_or_else(|| "unknown".to_owned(), |p| p.to_string());
        self.per_source.check(&format!("src:{source}"), now)?;
        // The connector counter is keyed by the site and the name as received, known or not: an
        // unknown name is limited exactly like a real one.
        self.per_connector
            .check(&format!("con:{}:{}", ctx.site.as_str(), name), now)
    }

    fn inbound_credential<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, Option<InboundCredential>> {
        Box::pin(async move {
            let mut state = self.lock();
            state.credential_lookups += 1;
            let site = ctx.site.as_str();
            state
                .connectors
                .iter()
                .find(|(s, seed)| s == site && seed.view.name == name)
                .and_then(|(_, seed)| seed.inbound.clone())
        })
    }

    fn inbound_receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        delivery_id: Option<&'a str>,
        body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<(), InboundRefusal>> {
        Box::pin(async move {
            let mut state = self.lock();
            state.receive_calls += 1;
            let site = ctx.site.as_str().to_owned();
            let now = state.now;
            let disabled = state
                .connectors
                .iter()
                .find(|(s, seed)| *s == site && seed.view.name == name)
                .is_some_and(|(_, seed)| seed.view.state == ConnectorState::Disabled);
            if disabled {
                return Err(InboundRefusal::Disabled);
            }
            if let Some(id) = delivery_id {
                state
                    .replay
                    .retain(|_, seen| now.saturating_sub(*seen) < REPLAY_WINDOW_SECS);
                let key = (site.clone(), name.to_owned(), id.to_owned());
                if state.replay.contains_key(&key) {
                    return Err(InboundRefusal::Replayed);
                }
                state.replay.insert(key, now);
            }
            state.enqueued.push(Enqueued {
                site,
                connector: name.to_owned(),
                delivery_id: delivery_id.map(str::to_owned),
                body: body.clone(),
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_limiter_counts_per_key_and_resets_after_the_window() {
        let limiter = FixedWindowLimiter::new(2, 60);
        assert!(limiter.check("a", 100).is_ok());
        assert!(limiter.check("a", 101).is_ok());
        assert_eq!(limiter.check("a", 102), Err(58));
        assert!(limiter.check("b", 102).is_ok());
        assert!(limiter.check("a", 160).is_ok());
    }

    #[test]
    fn a_credential_never_prints_its_bytes() {
        let shown = format!("{:?}", InboundCredential::Hmac(b"super-secret".to_vec()));
        assert!(!shown.contains("super"), "{shown}");
    }
}
