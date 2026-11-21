//! The occupancy seam of `/api/v1/occupancy/*` (`occupancy_claim`, `occupancy_confirm`,
//! `occupancy_release`, `occupancy_availability`, contract 0.23.0, WP-CON-03.T52): tentative
//! holds on `[start, end)` intervals of a unit, confirmation, release and the free and held
//! intervals of one unit.
//!
//! `spot-http` does not depend on the booking crate. It defines [`OccupancyPort`] and the host
//! writes the adapter over `spot-occupancy`. Everything that decides is the port's: who the
//! caller is (from `ctx.credential` on `ctx.site`), the create, confirm, cancel and read rights,
//! whether a hold is the caller's, partition ownership and the maximums. The routes pass **no
//! holder, no user and no tenant**: the holder of a claim is always the caller, and there is no
//! way to name another through this trait.
//!
//! `claim` and `confirm` carry the `Idempotency-Key` in `ctx.idempotency_key` (a registered
//! group sits outside the router's idempotency middleware, see the import port for the same
//! note); a repeat with the same key and request answers the first answer.
//!
//! [`InMemoryOccupancyPort`] is the deterministic fake the tests use (an injected clock, ids
//! from a counter).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use spot_core::Clock;
use time::{Duration, OffsetDateTime};

use super::PortKey;
use crate::api::registry::Code;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`OccupancyPort`].
pub const KEY: PortKey = PortKey::of::<dyn OccupancyPort>();

/// What a port call answers.
pub type OccupancyResult<T> = Result<T, OccupancyError>;

/// The longest availability window the contract allows, in days.
pub const MAX_WINDOW_DAYS: i64 = 366;
/// The default hold lifetime in seconds.
pub const DEFAULT_TTL_SECONDS: u32 = 900;
/// The contract's largest `ttl_seconds`.
pub const CONTRACT_MAX_TTL_SECONDS: u32 = 86_400;

/// Why a call failed: the typed refusals the contract names, and every other API error.
#[derive(Clone, Debug, PartialEq)]
pub enum OccupancyError {
    /// The interval is held or occupied (`409 occupancy.conflict`). Carries nothing: the answer
    /// never says who holds it or until when.
    Conflict,
    /// The hold expired and another row took its place (`409 occupancy.hold_expired`).
    Expired,
    /// The caller is not the holder and lacks the right (`403 occupancy.not_holder`).
    NotHolder,
    /// The end is not after the start, or the interval is too long (`422 occupancy.invalid_range`).
    InvalidRange,
    /// `ttl_seconds` is over the server's maximum (`422 occupancy.ttl_too_long`).
    TtlTooLong,
    /// This node does not own the unit's partition (`403 partition.not_owner`).
    NotOwner,
    /// Anything else: not found, not authenticated, unavailable.
    Api(ApiError),
}

impl From<ApiError> for OccupancyError {
    fn from(error: ApiError) -> Self {
        OccupancyError::Api(error)
    }
}

/// The server's maximums, reported by the port so the route can refuse before storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OccupancyLimits {
    /// The longest claimed interval.
    pub max_span: Duration,
    /// The largest `ttl_seconds`.
    pub max_ttl_seconds: u32,
}

/// The body of `occupancy_claim`, shape-checked by the route. There is no holder: it is the
/// caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimIn {
    /// The unit (a record id or a name, 1 to 200 characters).
    pub unit: String,
    /// Start, included.
    pub start: OffsetDateTime,
    /// End, excluded; after `start`.
    pub end: OffsetDateTime,
    /// The hold's lifetime; `None` takes [`DEFAULT_TTL_SECONDS`].
    pub ttl_seconds: Option<u32>,
}

/// The state of a hold (the contract's `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldStateOut {
    /// A tentative hold that expires.
    Tentative,
    /// Confirmed.
    Confirmed,
    /// Released.
    Released,
}

impl HoldStateOut {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            HoldStateOut::Tentative => "tentative",
            HoldStateOut::Confirmed => "confirmed",
            HoldStateOut::Released => "released",
        }
    }
}

/// One hold (the contract's `OccupancyHold`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldOut {
    /// The hold's id (a record id).
    pub id: String,
    /// The unit.
    pub unit: String,
    /// Start, included.
    pub start: OffsetDateTime,
    /// End, excluded.
    pub end: OffsetDateTime,
    /// The state.
    pub state: HoldStateOut,
    /// When a tentative hold expires.
    pub expires_at: Option<OffsetDateTime>,
    /// The typed holder reference (`user:...` or `consumer:...`).
    pub holder: String,
}

/// The status of an availability interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalStatusOut {
    /// Nothing holds it.
    Free,
    /// Occupied; all a caller who cannot read the occupancy rows learns.
    Busy,
    /// A tentative hold.
    Held,
    /// A confirmed booking.
    Confirmed,
}

impl IntervalStatusOut {
    /// The contract's text.
    pub fn as_str(self) -> &'static str {
        match self {
            IntervalStatusOut::Free => "free",
            IntervalStatusOut::Busy => "busy",
            IntervalStatusOut::Held => "held",
            IntervalStatusOut::Confirmed => "confirmed",
        }
    }
}

/// One interval of an availability answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntervalOut {
    /// Start, included.
    pub start: OffsetDateTime,
    /// End, excluded.
    pub end: OffsetDateTime,
    /// The status.
    pub status: IntervalStatusOut,
    /// The holder; only for a caller who may read the occupancy rows.
    pub holder: Option<String>,
}

/// The answer of `occupancy_availability`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailabilityOut {
    /// The unit.
    pub unit: String,
    /// Window start.
    pub from: OffsetDateTime,
    /// Window end.
    pub to: OffsetDateTime,
    /// The intervals in time order, at most 1,000.
    pub intervals: Vec<IntervalOut>,
}

/// Occupancy for the caller the credential names.
pub trait OccupancyPort: Send + Sync {
    /// The server's maximums.
    fn limits(&self) -> OccupancyLimits;

    /// Claims an interval of `unit` of `entity` for the caller.
    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        request: ClaimIn,
    ) -> BoxFuture<'a, OccupancyResult<HoldOut>>;

    /// Confirms a hold.
    fn confirm<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, OccupancyResult<HoldOut>>;

    /// Releases a hold; releasing a released hold succeeds.
    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, OccupancyResult<()>>;

    /// The free and held intervals of `unit` in `[from, to)`; the window is at most
    /// [`MAX_WINDOW_DAYS`] days (the route checks).
    fn availability<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        unit: &'a str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> BoxFuture<'a, OccupancyResult<AvailabilityOut>>;
}

// ---------------------------------------------------------------------------------------------
// The in-memory port

struct Hold {
    site: String,
    entity: String,
    out: HoldOut,
    owner: String,
}

struct Replay {
    fingerprint: String,
    hold: HoldOut,
}

#[derive(Default)]
struct State {
    /// (site, token) to the user's record id.
    tokens: HashMap<(String, String), String>,
    /// (site, entity) registered.
    entities: Vec<(String, String)>,
    /// (site, entity) whose partition this node does not own.
    unowned: Vec<(String, String)>,
    /// (site, user) who may read the occupancy rows, confirm and cancel any hold.
    staff: Vec<(String, String)>,
    holds: Vec<Hold>,
    replays: HashMap<(String, String, &'static str, String), Replay>,
    next: u64,
}

/// A deterministic in-memory [`OccupancyPort`] for tests.
pub struct InMemoryOccupancyPort {
    clock: Arc<dyn Clock>,
    limits: OccupancyLimits,
    state: Mutex<State>,
}

impl InMemoryOccupancyPort {
    /// A port with a 30 day span limit and the contract's largest ttl.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        InMemoryOccupancyPort {
            clock,
            limits: OccupancyLimits {
                max_span: Duration::days(30),
                max_ttl_seconds: CONTRACT_MAX_TTL_SECONDS,
            },
            state: Mutex::new(State::default()),
        }
    }

    /// Replaces the limits.
    pub fn with_limits(mut self, limits: OccupancyLimits) -> Self {
        self.limits = limits;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Lets `token` act as `user` (a record id) on `site`.
    pub fn grant_token(&self, site: &str, token: &str, user: &str) {
        self.lock()
            .tokens
            .insert((site.to_owned(), token.to_owned()), user.to_owned());
    }

    /// Registers an occupancy entity.
    pub fn add_entity(&self, site: &str, entity: &str) {
        self.lock()
            .entities
            .push((site.to_owned(), entity.to_owned()));
    }

    /// Makes this node not own the partition of `entity` (claims answer `NotOwner`).
    pub fn set_unowned(&self, site: &str, entity: &str) {
        self.lock()
            .unowned
            .push((site.to_owned(), entity.to_owned()));
    }

    /// Lets `user` read the occupancy rows and confirm or cancel any hold.
    pub fn grant_staff(&self, site: &str, user: &str) {
        self.lock().staff.push((site.to_owned(), user.to_owned()));
    }

    fn caller(
        state: &State,
        ctx: &RequestCtx,
        entity: &str,
    ) -> OccupancyResult<(String, String, bool)> {
        let site = ctx.site.as_str().to_owned();
        let token = ctx
            .credential
            .as_ref()
            .map(|c| c.token.clone())
            .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
        let user = state
            .tokens
            .get(&(site.clone(), token))
            .cloned()
            .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
        if !state
            .entities
            .iter()
            .any(|(s, e)| *s == site && e == entity)
        {
            return Err(ApiError::from_code(Code::DocumentNotFound).into());
        }
        let staff = state.staff.iter().any(|(s, u)| *s == site && *u == user);
        Ok((site, user, staff))
    }

    /// Whether a hold still occupies its interval at `now`.
    fn occupies(hold: &HoldOut, now: OffsetDateTime) -> bool {
        match hold.state {
            HoldStateOut::Released => false,
            HoldStateOut::Confirmed => true,
            HoldStateOut::Tentative => hold.expires_at.is_none_or(|at| at > now),
        }
    }

    fn overlaps(a: &HoldOut, start: OffsetDateTime, end: OffsetDateTime) -> bool {
        a.start < end && start < a.end
    }
}

fn typed(user: &str) -> String {
    format!("user:{user}")
}

impl OccupancyPort for InMemoryOccupancyPort {
    fn limits(&self) -> OccupancyLimits {
        self.limits
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        request: ClaimIn,
    ) -> BoxFuture<'a, OccupancyResult<HoldOut>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user, _) = Self::caller(&state, ctx, entity)?;
            let fingerprint = format!("{entity}:{request:?}");
            if let Some(key) = &ctx.idempotency_key
                && let Some(seen) =
                    state
                        .replays
                        .get(&(site.clone(), user.clone(), "claim", key.clone()))
            {
                return if seen.fingerprint == fingerprint {
                    Ok(seen.hold.clone())
                } else {
                    Err(ApiError::from_code(Code::ApiIdempotencyConflict).into())
                };
            }
            if state.unowned.iter().any(|(s, e)| *s == site && e == entity) {
                return Err(OccupancyError::NotOwner);
            }
            if request.end <= request.start || request.end - request.start > self.limits.max_span {
                return Err(OccupancyError::InvalidRange);
            }
            let ttl = request.ttl_seconds.unwrap_or(DEFAULT_TTL_SECONDS);
            if ttl > self.limits.max_ttl_seconds {
                return Err(OccupancyError::TtlTooLong);
            }
            let now = self.clock.now_utc();
            let taken = state.holds.iter().any(|h| {
                h.site == site
                    && h.entity == entity
                    && h.out.unit == request.unit
                    && Self::occupies(&h.out, now)
                    && Self::overlaps(&h.out, request.start, request.end)
            });
            if taken {
                return Err(OccupancyError::Conflict);
            }
            state.next += 1;
            let out = HoldOut {
                id: format!("0192f3c4-0000-7000-8000-{:012x}", state.next),
                unit: request.unit,
                start: request.start,
                end: request.end,
                state: HoldStateOut::Tentative,
                expires_at: Some(now + Duration::seconds(i64::from(ttl))),
                holder: typed(&user),
            };
            state.holds.push(Hold {
                site: site.clone(),
                entity: entity.to_owned(),
                out: out.clone(),
                owner: user.clone(),
            });
            if let Some(key) = &ctx.idempotency_key {
                state.replays.insert(
                    (site, user, "claim", key.clone()),
                    Replay {
                        fingerprint,
                        hold: out.clone(),
                    },
                );
            }
            Ok(out)
        })
    }

    fn confirm<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, OccupancyResult<HoldOut>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user, staff) = Self::caller(&state, ctx, entity)?;
            let fingerprint = id.to_owned();
            if let Some(key) = &ctx.idempotency_key
                && let Some(seen) =
                    state
                        .replays
                        .get(&(site.clone(), user.clone(), "confirm", key.clone()))
            {
                return if seen.fingerprint == fingerprint {
                    Ok(seen.hold.clone())
                } else {
                    Err(ApiError::from_code(Code::ApiIdempotencyConflict).into())
                };
            }
            let now = self.clock.now_utc();
            let index = state
                .holds
                .iter()
                .position(|h| h.site == site && h.entity == entity && h.out.id == id)
                .ok_or_else(|| ApiError::from_code(Code::DocumentNotFound))?;
            let (owner, hold) = {
                let h = &state.holds[index];
                (h.owner.clone(), h.out.clone())
            };
            if owner != user && !staff {
                // A hold the caller may not see is the answer for one that is not there.
                return Err(ApiError::from_code(Code::DocumentNotFound).into());
            }
            match hold.state {
                HoldStateOut::Confirmed => return Ok(hold),
                HoldStateOut::Released => return Err(OccupancyError::Expired),
                HoldStateOut::Tentative => {}
            }
            if !Self::occupies(&hold, now) {
                // The late-confirm rule: an expired, unswept hold confirms only if no other
                // row took its place.
                let replaced = state.holds.iter().enumerate().any(|(i, h)| {
                    i != index
                        && h.site == site
                        && h.entity == entity
                        && h.out.unit == hold.unit
                        && Self::occupies(&h.out, now)
                        && Self::overlaps(&h.out, hold.start, hold.end)
                });
                if replaced {
                    return Err(OccupancyError::Expired);
                }
            }
            let confirmed = &mut state.holds[index].out;
            confirmed.state = HoldStateOut::Confirmed;
            confirmed.expires_at = None;
            let out = confirmed.clone();
            if let Some(key) = &ctx.idempotency_key {
                state.replays.insert(
                    (site, user, "confirm", key.clone()),
                    Replay {
                        fingerprint,
                        hold: out.clone(),
                    },
                );
            }
            Ok(out)
        })
    }

    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, OccupancyResult<()>> {
        Box::pin(async move {
            let mut state = self.lock();
            let (site, user, staff) = Self::caller(&state, ctx, entity)?;
            let hold = state
                .holds
                .iter_mut()
                .find(|h| h.site == site && h.entity == entity && h.out.id == id)
                .ok_or_else(|| ApiError::from_code(Code::DocumentNotFound))?;
            if hold.owner != user && !staff {
                return Err(OccupancyError::NotHolder);
            }
            hold.out.state = HoldStateOut::Released;
            hold.out.expires_at = None;
            Ok(())
        })
    }

    fn availability<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        unit: &'a str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> BoxFuture<'a, OccupancyResult<AvailabilityOut>> {
        Box::pin(async move {
            let state = self.lock();
            let (site, _, staff) = Self::caller(&state, ctx, entity)?;
            let now = self.clock.now_utc();
            let mut busy: Vec<&HoldOut> = state
                .holds
                .iter()
                .filter(|h| h.site == site && h.entity == entity && h.out.unit == unit)
                .map(|h| &h.out)
                .filter(|h| Self::occupies(h, now) && Self::overlaps(h, from, to))
                .collect();
            busy.sort_by_key(|h| h.start);
            let mut intervals = Vec::new();
            let mut cursor = from;
            for hold in busy {
                let start = hold.start.max(cursor);
                let end = hold.end.min(to);
                if start >= end {
                    continue;
                }
                if cursor < start {
                    intervals.push(IntervalOut {
                        start: cursor,
                        end: start,
                        status: IntervalStatusOut::Free,
                        holder: None,
                    });
                }
                intervals.push(IntervalOut {
                    start,
                    end,
                    status: if staff {
                        match hold.state {
                            HoldStateOut::Confirmed => IntervalStatusOut::Confirmed,
                            _ => IntervalStatusOut::Held,
                        }
                    } else {
                        IntervalStatusOut::Busy
                    },
                    holder: staff.then(|| hold.holder.clone()),
                });
                cursor = end;
            }
            if cursor < to {
                intervals.push(IntervalOut {
                    start: cursor,
                    end: to,
                    status: IntervalStatusOut::Free,
                    holder: None,
                });
            }
            Ok(AvailabilityOut {
                unit: unit.to_owned(),
                from,
                to,
                intervals,
            })
        })
    }
}
