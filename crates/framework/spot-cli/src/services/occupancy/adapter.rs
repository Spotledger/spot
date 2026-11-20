//! The production [`OccupancyPort`]: the HTTP seam of `/api/v1/occupancy/*` over `spot-occupancy`.
//!
//! `spot-http` owns the routes and passes **no holder, no user and no tenant**. This adapter
//! decides what is left:
//!
//! - **Holder.** Always the authenticated principal ([`Identity`]); there is no way to name
//!   another through the trait.
//! - **Rights.** Anyone authenticated may claim for themselves. Confirm and release are the
//!   holder's; a staff user holding `occupancy_manage` may act on any hold. Reading the state of
//!   an interval needs the same permission: everyone else learns `busy` and nothing more. A
//!   consumer never gets staff read rights.
//! - **Partition.** A write for a unit another node owns is `partition.not_owner`, answered
//!   before storage is touched (the service checks again).
//! - **Limits.** The configured maximum span and time-to-live; a ttl over the maximum is
//!   `occupancy.ttl_too_long`.
//! - **Idempotency.** A claim repeated with the same `Idempotency-Key` and request answers the
//!   first answer; the same key with another request is `api.idempotency_conflict`. The replay
//!   record is per holder, in memory and bounded; confirm is idempotent by itself.
//! - **Tenancy.** The adapter belongs to one site: its storage backend and catalog are the
//!   site's, so another tenant's units are not reachable. A request for another site's id has no
//!   path here.
//!
//! The conflict answer carries nothing: not the other holder, not the other range, not the
//! expiry. A hold that is not the caller's and that the caller may not manage answers
//! `occupancy.not_holder` and never says whether the id exists as someone else's.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use spot_core::Clock;
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::occupancy::{
    AvailabilityOut, ClaimIn, HoldOut, HoldStateOut, IntervalOut, IntervalStatusOut,
    OccupancyError, OccupancyLimits, OccupancyPort, OccupancyResult,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_occupancy::{
    Busy, Caller as Actor, ClaimRequest, Hold, OccupancyError as ServiceError, OwnerRoutes,
    Released, Span, State,
};
use spot_postgres::PgBackend;
use spot_storage_contract::{ErrorKind, RangePoint, RangeValue, RecordId};
use time::{Duration, OffsetDateTime, Time};

use super::identity::{Identity, Who};

/// The entity the service stores holds in (`spot_occupancy::declaration::ENTITY`). The `{entity}`
/// segment of every route must name it.
pub const ENTITY: &str = spot_occupancy::declaration::ENTITY;

/// Most availability intervals one answer lists (free gaps included).
const MAX_INTERVALS: usize = 1000;
/// Replay records kept at most.
const REPLAY_CAP: usize = 10_000;
/// How long a replay record answers.
const REPLAY_TTL: Duration = Duration::hours(24);

type Service = spot_occupancy::Occupancy<PgBackend>;

/// Whether the start-up check passed. A port whose check failed answers `server.unavailable`:
/// without the exclusion constraint a claim could double-book silently.
#[derive(Debug, Default)]
pub struct Ready(AtomicBool);

impl Ready {
    /// A gate that starts closed.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Opens or closes the gate.
    pub fn set(&self, open: bool) {
        self.0.store(open, Ordering::SeqCst);
    }

    /// Whether claims are served.
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct Replay {
    fingerprint: String,
    hold: HoldOut,
    at: OffsetDateTime,
}

/// The settings the adapter needs, already validated.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The longest claimed interval.
    pub max_span: Duration,
    /// The largest `ttl_seconds` a request may ask for.
    pub max_ttl_seconds: u32,
    /// What the range is made of.
    pub span: Span,
}

/// The [`OccupancyPort`] of one site.
pub struct OccupancyApi {
    service: Arc<Service>,
    identity: Arc<dyn Identity>,
    routes: Arc<dyn OwnerRoutes>,
    node: spot_occupancy::Owner,
    clock: Arc<dyn Clock>,
    ready: Arc<Ready>,
    settings: Settings,
    replays: Mutex<HashMap<(String, String), Replay>>,
}

impl OccupancyApi {
    /// The port over `service` for the node `node`.
    pub fn new(
        service: Arc<Service>,
        identity: Arc<dyn Identity>,
        routes: Arc<dyn OwnerRoutes>,
        node: spot_occupancy::Owner,
        clock: Arc<dyn Clock>,
        ready: Arc<Ready>,
        settings: Settings,
    ) -> Self {
        Self {
            service,
            identity,
            routes,
            node,
            clock,
            ready,
            settings,
            replays: Mutex::new(HashMap::new()),
        }
    }

    fn gate(&self, entity: &str) -> OccupancyResult<()> {
        if entity != ENTITY {
            return Err(ApiError::from_code(Code::DocumentNotFound).into());
        }
        if !self.ready.is_open() {
            return Err(ApiError::from_code(Code::ServerUnavailable).into());
        }
        Ok(())
    }

    fn actor(who: &Who) -> Actor {
        if who.manage {
            Actor::Operator
        } else {
            Actor::Holder(who.holder.clone())
        }
    }

    fn replays(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Replay>> {
        self.replays.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn remember(&self, holder: &str, key: &str, fingerprint: String, hold: &HoldOut) {
        let now = self.clock.now_utc();
        let mut replays = self.replays();
        if replays.len() >= REPLAY_CAP {
            replays.retain(|_, r| now - r.at < REPLAY_TTL);
        }
        if replays.len() < REPLAY_CAP {
            replays.insert(
                (holder.to_owned(), key.to_owned()),
                Replay {
                    fingerprint,
                    hold: hold.clone(),
                    at: now,
                },
            );
        }
    }

    fn replay(
        &self,
        holder: &str,
        key: &str,
        fingerprint: &str,
    ) -> Option<OccupancyResult<HoldOut>> {
        let now = self.clock.now_utc();
        let replays = self.replays();
        let seen = replays.get(&(holder.to_owned(), key.to_owned()))?;
        if now - seen.at >= REPLAY_TTL {
            return None;
        }
        Some(if seen.fingerprint == fingerprint {
            Ok(seen.hold.clone())
        } else {
            Err(ApiError::from_code(Code::ApiIdempotencyConflict).into())
        })
    }

    /// The range for `[start, end)` in the kind the entity stores.
    fn range(&self, start: OffsetDateTime, end: OffsetDateTime) -> OccupancyResult<RangeValue> {
        let (a, b) = match self.settings.span {
            Span::Instants => (RangePoint::DateTime(start), RangePoint::DateTime(end)),
            Span::Dates => {
                // Whole days only: a time of day cannot be stored, and rounding would book more
                // or less than was asked.
                let midnight =
                    |t: OffsetDateTime| t.time() == Time::MIDNIGHT && t.offset().is_utc();
                if !midnight(start) || !midnight(end) {
                    return Err(OccupancyError::InvalidRange);
                }
                (RangePoint::Date(start.date()), RangePoint::Date(end.date()))
            }
        };
        let range = RangeValue::half_open(a, b);
        range.validate().map_err(|_| OccupancyError::InvalidRange)?;
        Ok(range)
    }

    fn unit(unit: &str) -> OccupancyResult<RecordId> {
        parse_record_id(unit).ok_or_else(|| {
            ApiError::new(Code::ApiInvalidRequest, "the unit must be a record id").into()
        })
    }
}

fn instant(point: RangePoint) -> Option<OffsetDateTime> {
    match point {
        RangePoint::DateTime(t) => Some(t),
        RangePoint::Date(d) => Some(d.midnight().assume_utc()),
        _ => None,
    }
}

fn hold_out(hold: &Hold) -> OccupancyResult<HoldOut> {
    let (start, end) = hold
        .during
        .bounds()
        .and_then(|(a, b)| Some((instant(a)?, instant(b)?)))
        .ok_or_else(|| ApiError::from_code(Code::ServerUnavailable))?;
    Ok(HoldOut {
        id: hold.id.to_string(),
        unit: hold.unit.to_string(),
        start,
        end,
        state: match hold.state {
            State::Tentative => HoldStateOut::Tentative,
            State::Confirmed => HoldStateOut::Confirmed,
        },
        expires_at: hold.expires_at,
        holder: hold.holder.to_text(),
    })
}

/// Maps the service's refusals to the port's. Nothing of the storage error is carried: not the
/// message, not a constraint detail, not another holder.
pub fn map_error(error: ServiceError) -> OccupancyError {
    match error {
        ServiceError::Conflict(_) => OccupancyError::Conflict,
        ServiceError::Expired(_) => OccupancyError::Expired,
        ServiceError::NotHolder => OccupancyError::NotHolder,
        ServiceError::InvalidRange(_) => OccupancyError::InvalidRange,
        ServiceError::NotOwner { .. } => OccupancyError::NotOwner,
        ServiceError::Storage(e) => {
            if e.kind == ErrorKind::InvalidValue {
                OccupancyError::InvalidRange
            } else {
                // The kind's code is a fixed word, never a value.
                tracing::warn!(code = e.kind.code(), "an occupancy storage call failed");
                ApiError::from_code(Code::ServerUnavailable).into()
            }
        }
    }
}

/// The intervals of `[from, to)`: the busy stretches in time order with the free gaps between.
fn intervals(
    from: OffsetDateTime,
    to: OffsetDateTime,
    busy: &[Busy],
    staff: bool,
) -> Vec<IntervalOut> {
    let mut stretches: Vec<(OffsetDateTime, OffsetDateTime, State)> = busy
        .iter()
        .filter_map(|b| {
            let (s, e) = b.during.bounds()?;
            Some((instant(s)?.max(from), instant(e)?.min(to), b.state))
        })
        .filter(|(s, e, _)| s < e)
        .collect();
    stretches.sort_by_key(|(s, _, _)| *s);
    let mut out = Vec::new();
    let mut cursor = from;
    for (s, e, state) in stretches {
        if s > cursor {
            out.push(IntervalOut {
                start: cursor,
                end: s,
                status: IntervalStatusOut::Free,
                holder: None,
            });
        }
        out.push(IntervalOut {
            start: s,
            end: e,
            status: if !staff {
                IntervalStatusOut::Busy
            } else if state == State::Confirmed {
                IntervalStatusOut::Confirmed
            } else {
                IntervalStatusOut::Held
            },
            // The service never returns a holder, so there is nothing to leak even to staff.
            holder: None,
        });
        cursor = cursor.max(e);
    }
    if cursor < to {
        out.push(IntervalOut {
            start: cursor,
            end: to,
            status: IntervalStatusOut::Free,
            holder: None,
        });
    }
    out.truncate(MAX_INTERVALS);
    out
}

impl OccupancyPort for OccupancyApi {
    fn limits(&self) -> OccupancyLimits {
        OccupancyLimits {
            max_span: self.settings.max_span,
            max_ttl_seconds: self.settings.max_ttl_seconds,
        }
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        request: ClaimIn,
    ) -> BoxFuture<'a, OccupancyResult<HoldOut>> {
        Box::pin(async move {
            self.gate(entity)?;
            let who = self.identity.who(ctx).await?;
            let holder = who.holder.to_text();
            let fingerprint = format!(
                "{}|{}|{}|{:?}",
                request.unit, request.start, request.end, request.ttl_seconds
            );
            if let Some(key) = &ctx.idempotency_key
                && let Some(answer) = self.replay(&holder, key, &fingerprint)
            {
                return answer;
            }
            let unit = Self::unit(&request.unit)?;
            // One owning node per resource: refused before any storage call.
            if self.routes.owner_of(unit) != self.node {
                return Err(OccupancyError::NotOwner);
            }
            if request.end <= request.start || request.end - request.start > self.settings.max_span
            {
                return Err(OccupancyError::InvalidRange);
            }
            if let Some(ttl) = request.ttl_seconds
                && ttl > self.settings.max_ttl_seconds
            {
                return Err(OccupancyError::TtlTooLong);
            }
            let claim = ClaimRequest {
                unit,
                during: self.range(request.start, request.end)?,
                holder: who.holder,
                ttl: request
                    .ttl_seconds
                    .map(|s| StdDuration::from_secs(u64::from(s))),
            };
            let hold = self.service.hold(&claim).await.map_err(map_error)?;
            let out = hold_out(&hold)?;
            if let Some(key) = &ctx.idempotency_key {
                self.remember(&holder, key, fingerprint, &out);
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
            self.gate(entity)?;
            let who = self.identity.who(ctx).await?;
            let id =
                parse_record_id(id).ok_or_else(|| ApiError::from_code(Code::DocumentNotFound))?;
            let hold = self
                .service
                .confirm(id, &Self::actor(&who))
                .await
                .map_err(map_error)?;
            hold_out(&hold)
        })
    }

    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        entity: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, OccupancyResult<()>> {
        Box::pin(async move {
            self.gate(entity)?;
            let who = self.identity.who(ctx).await?;
            let id =
                parse_record_id(id).ok_or_else(|| ApiError::from_code(Code::DocumentNotFound))?;
            match self
                .service
                .release(id, &Self::actor(&who))
                .await
                .map_err(map_error)?
            {
                Released::Released(_) | Released::AlreadyGone => Ok(()),
            }
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
            self.gate(entity)?;
            let who = self.identity.who(ctx).await?;
            let unit_id = Self::unit(unit)?;
            let during = self.range(from, to)?;
            let busy = self
                .service
                .availability(unit_id, &during)
                .await
                .map_err(map_error)?;
            Ok(AvailabilityOut {
                unit: unit.to_owned(),
                from,
                to,
                intervals: intervals(from, to, &busy, who.manage),
            })
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_storage_contract::StorageError;
    use time::macros::datetime;

    fn busy(a: OffsetDateTime, b: OffsetDateTime, state: State) -> Busy {
        Busy {
            during: RangeValue::half_open(RangePoint::DateTime(a), RangePoint::DateTime(b)),
            state,
        }
    }

    #[test]
    fn intervals_fill_the_gaps_and_hide_the_state_from_non_staff() {
        let from = datetime!(2030-01-01 00:00 UTC);
        let to = datetime!(2030-01-10 00:00 UTC);
        let held = busy(
            datetime!(2030-01-02 00:00 UTC),
            datetime!(2030-01-04 00:00 UTC),
            State::Tentative,
        );
        let booked = busy(
            datetime!(2030-01-06 00:00 UTC),
            datetime!(2030-01-08 00:00 UTC),
            State::Confirmed,
        );
        let public = intervals(from, to, &[booked.clone(), held.clone()], false);
        let kinds: Vec<_> = public.iter().map(|i| i.status).collect();
        assert_eq!(
            kinds,
            vec![
                IntervalStatusOut::Free,
                IntervalStatusOut::Busy,
                IntervalStatusOut::Free,
                IntervalStatusOut::Busy,
                IntervalStatusOut::Free
            ]
        );
        assert!(public.iter().all(|i| i.holder.is_none()));
        let staff = intervals(from, to, &[held, booked], true);
        assert_eq!(staff[1].status, IntervalStatusOut::Held);
        assert_eq!(staff[3].status, IntervalStatusOut::Confirmed);
        // The intervals tile the window.
        assert_eq!(staff[0].start, from);
        assert_eq!(staff.last().unwrap().end, to);
        for pair in staff.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    #[test]
    fn a_busy_stretch_is_clamped_to_the_window() {
        let from = datetime!(2030-01-03 00:00 UTC);
        let to = datetime!(2030-01-05 00:00 UTC);
        let wide = busy(
            datetime!(2030-01-01 00:00 UTC),
            datetime!(2030-01-09 00:00 UTC),
            State::Confirmed,
        );
        let out = intervals(from, to, &[wide], false);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].start, out[0].end), (from, to));
    }

    #[test]
    fn service_refusals_map_to_the_ports_without_detail() {
        let storage = StorageError::new(ErrorKind::ExclusionViolation, "secret detail");
        assert_eq!(
            map_error(ServiceError::Conflict(storage.clone())),
            OccupancyError::Conflict
        );
        assert_eq!(
            map_error(ServiceError::Expired(storage.clone())),
            OccupancyError::Expired
        );
        assert_eq!(
            map_error(ServiceError::NotHolder),
            OccupancyError::NotHolder
        );
        let bad = StorageError::new(ErrorKind::InvalidValue, "x");
        assert_eq!(
            map_error(ServiceError::Storage(bad)),
            OccupancyError::InvalidRange
        );
        let down = StorageError::new(ErrorKind::SerializationFailure, "x");
        assert!(matches!(
            map_error(ServiceError::Storage(down)),
            OccupancyError::Api(_)
        ));
    }

    #[test]
    fn the_ready_gate_starts_closed() {
        let ready = Ready::new();
        assert!(!ready.is_open());
        ready.set(true);
        assert!(ready.is_open());
    }
}
