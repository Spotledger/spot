//! Process throughput (WP-BPM-11.T11): the weekly completed-Run counts the
//! `bpm_throughput_by_week` report needs, served as the [`ThroughputFeed`] port from a cache.
//!
//! # Why a cache
//!
//! [`ThroughputFeed::weeks`] is a synchronous trait called from a report executor, while the
//! numbers live in PostgreSQL behind async code. This module therefore never answers from the
//! database: a small service keeps the latest weekly counts in memory, refreshes them on an
//! interval from [`PgThroughput`] (WP-BPM-11.T10), and `weeks()` only reads the cache. It never
//! blocks a runtime thread and never touches the database.
//!
//! # What it counts
//!
//! Completed Runs by the UTC ISO week of their end, per process version, for the current ISO week
//! and the `weeks` weeks before it. The window is computed from the **site clock**
//! (`ctx.clock()`), never the system clock: `until` is the start of the next ISO week after now,
//! `since` is `weeks` weeks before the start of the current ISO week.
//!
//! # Rules
//!
//! - Before the first successful refresh there is no snapshot and `weeks()` answers `Err(())`
//!   ("the feed cannot answer"), never an empty success.
//! - A failed refresh keeps the last good snapshot and logs one `warn` per failure streak (not per
//!   tick). Nothing logged carries a process name or a count.
//! - The cache belongs to one site: another site's id gets `Err(())`.
//! - The service is optional ([`Level::Optional`]); its start-up check is unhealthy only until the
//!   first refresh has been tried, never for a later failure.
//!
//! # Configuration
//!
//! `[services.process_throughput]`: `enabled` (`true`), `weeks` (26, `1..=104`), `refresh_secs`
//! (300, `30..=3600`). Unknown keys and out-of-range values are start-up errors naming the key.
//! The module is inert when it is disabled or when the process engine is not hosted.
//!
//! # Order
//!
//! [`ORDER`] is 1192: after process insight (1190, which follows the process engine at 1100),
//! before `process_insight_reports` (1195) that looks the port up.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use spot_bpm_pg::{PgThroughput, WeeklyRow};
use spot_core::{Clock, SiteId};
use time::{OffsetDateTime, Time, UtcOffset};
use tokio::sync::watch;

use crate::services::process::ProcessHost;
use crate::services::process_insight_reports::{ThroughputFeed, WeeklyThroughput};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result, error};

/// Start order: after process insight (1190), before the insight reports (1195).
pub const ORDER: u32 = 1192;

/// The most rows one refresh reads (the storage call's own ceiling).
pub const MAX_ROWS: u32 = 5000;

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `[services.process_throughput]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// How many ISO weeks back from the current one the cache covers (`1..=104`).
    pub weeks: u32,
    /// How often the cache is refreshed, in seconds (`30..=3600`).
    pub refresh_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            weeks: 26,
            refresh_secs: 300,
        }
    }
}

impl Settings {
    /// Names the first setting that is out of range.
    ///
    /// # Errors
    /// The key that is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=104).contains(&self.weeks) {
            return Err("weeks");
        }
        if !(30..=3600).contains(&self.refresh_secs) {
            return Err("refresh_secs");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------------------------

/// The refresh window for `now`: `since` is `weeks` weeks before the start of the current UTC ISO
/// week (Monday 00:00), `until` is the start of the next ISO week. `None` only when the arithmetic
/// leaves the representable range of dates.
#[must_use]
pub fn window(now: OffsetDateTime, weeks: u32) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let day = now.to_offset(UtcOffset::UTC).replace_time(Time::MIDNIGHT);
    let back = time::Duration::days(i64::from(day.weekday().number_days_from_monday()));
    let this_week = day.checked_sub(back)?;
    let since = this_week.checked_sub(time::Duration::weeks(i64::from(weeks)))?;
    let until = this_week.checked_add(time::Duration::weeks(1))?;
    Some((since, until))
}

// ---------------------------------------------------------------------------------------------
// The source and the cache
// ---------------------------------------------------------------------------------------------

/// The boxed future of one source read.
pub type SourceFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<Vec<WeeklyThroughput>, ()>> + Send + 'a>>;

/// Where the weekly counts of `[since, until)` come from. The host's source is
/// [`PgWeekSource`]; tests may substitute their own.
pub trait WeekSource: Send + Sync {
    /// The weekly completed Runs in `[since, until)`; `Err(())` for any failure.
    fn weekly(&self, since: OffsetDateTime, until: OffsetDateTime) -> SourceFuture<'_>;
}

/// The PostgreSQL source: [`PgThroughput`] for the site's tenant.
pub struct PgWeekSource(PgThroughput);

impl PgWeekSource {
    /// A source reading `reader`.
    #[must_use]
    pub fn new(reader: PgThroughput) -> Self {
        Self(reader)
    }
}

impl WeekSource for PgWeekSource {
    fn weekly(&self, since: OffsetDateTime, until: OffsetDateTime) -> SourceFuture<'_> {
        Box::pin(async move {
            let rows = self
                .0
                .weekly(since, until, MAX_ROWS)
                .await
                .map_err(|_| ())?;
            Ok(rows.into_iter().map(weekly).collect())
        })
    }
}

fn weekly(row: WeeklyRow) -> WeeklyThroughput {
    WeeklyThroughput {
        process: row.process,
        version: row.version,
        week: row.week,
        completed: row.completed,
    }
}

/// The latest weekly counts: `None` until the first successful refresh.
pub type Cache = Arc<RwLock<Option<Vec<WeeklyThroughput>>>>;

/// The cache reader that is provided as the [`ThroughputFeed`] port.
pub struct CachedFeed {
    site: SiteId,
    cache: Cache,
}

impl ThroughputFeed for CachedFeed {
    fn weeks(&self, site: &SiteId) -> std::result::Result<Vec<WeeklyThroughput>, ()> {
        if site != &self.site {
            return Err(());
        }
        let guard = self.cache.read().map_err(|_| ())?;
        guard.as_ref().cloned().ok_or(())
    }
}

/// One site's throughput cache and its refresh.
pub struct ProcessThroughput {
    site: SiteId,
    source: Arc<dyn WeekSource>,
    clock: Arc<dyn Clock>,
    settings: Settings,
    cache: Cache,
    /// Whether a refresh has been tried at least once (success or failure).
    tried: AtomicBool,
    /// Whether the last refresh failed (one warning per streak).
    failing: AtomicBool,
}

impl ProcessThroughput {
    /// A cache for `site` read from `source`, with the window taken from `clock`.
    #[must_use]
    pub fn new(
        site: SiteId,
        source: Arc<dyn WeekSource>,
        clock: Arc<dyn Clock>,
        settings: Settings,
    ) -> Self {
        Self {
            site,
            source,
            clock,
            settings,
            cache: Arc::new(RwLock::new(None)),
            tried: AtomicBool::new(false),
            failing: AtomicBool::new(false),
        }
    }

    /// The feed that reads this cache.
    #[must_use]
    pub fn feed(&self) -> Arc<dyn ThroughputFeed> {
        Arc::new(CachedFeed {
            site: self.site.clone(),
            cache: Arc::clone(&self.cache),
        })
    }

    /// Refreshes the cache now and answers whether it succeeded. A failure keeps the last good
    /// snapshot and logs once per failure streak. Tests call this instead of waiting for the loop.
    pub async fn refresh_now(&self) -> bool {
        let result = match window(self.clock.now_utc(), self.settings.weeks) {
            Some((since, until)) => self.source.weekly(since, until).await,
            None => Err(()),
        };
        self.tried.store(true, Ordering::SeqCst);
        match result {
            Ok(rows) => {
                if let Ok(mut guard) = self.cache.write() {
                    *guard = Some(rows);
                }
                self.failing.store(false, Ordering::SeqCst);
                true
            }
            Err(()) => {
                if !self.failing.swap(true, Ordering::SeqCst) {
                    tracing::warn!(
                        "the process throughput refresh failed; the last snapshot stays in use"
                    );
                }
                false
            }
        }
    }

    /// Whether the first refresh has been tried.
    #[must_use]
    pub fn tried(&self) -> bool {
        self.tried.load(Ordering::SeqCst)
    }

    /// The refresh interval.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.settings.refresh_secs)
    }
}

// ---------------------------------------------------------------------------------------------
// The service and register()
// ---------------------------------------------------------------------------------------------

/// The hosted service: refreshes the cache once at start and then every `refresh_secs` until the
/// stop signal.
pub struct ThroughputService {
    inner: Arc<ProcessThroughput>,
}

impl ThroughputService {
    /// A service driving `inner`'s refresh loop.
    #[must_use]
    pub fn new(inner: Arc<ProcessThroughput>) -> Self {
        Self { inner }
    }
}

impl Service for ThroughputService {
    fn name(&self) -> &'static str {
        "process_throughput"
    }

    fn level(&self) -> Level {
        // A read model: losing it removes one report, it never stops documents.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if self.inner.tried() {
                Ok(())
            } else {
                Err("the first throughput refresh has not been tried yet".to_owned())
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                self.inner.refresh_now().await;
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(self.inner.interval()) => {}
                }
            }
        })
    }
}

/// The refresh service for `inner`: refreshes at start and then every `refresh_secs` until the stop
/// signal. [`register`] adds exactly this; tests drive it directly.
#[must_use]
pub fn service(inner: &Arc<ProcessThroughput>) -> Arc<dyn Service> {
    Arc::new(ThroughputService {
        inner: Arc::clone(inner),
    })
}

/// Registers the throughput cache: the index, the refresh service and the [`ThroughputFeed`]
/// port. Inert when `services.process_throughput.enabled = false` and when the process engine is
/// not hosted.
///
/// # Errors
/// A setting that does not decode or is out of range (naming the key); the index could not be
/// created (naming the site); a port another module already provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_throughput")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process_throughput.{key}: out of range")))?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    if ctx.port::<ProcessHost>().is_none() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(error)?;
    let reader = PgThroughput::new(env.backend.pool()).with_tenant(site.as_str());
    reader.prepare().await.map_err(|e| {
        Error(format!(
            "services.process_throughput: site `{}`: the throughput index could not be created: {e}",
            site.as_str()
        ))
    })?;
    let inner = Arc::new(ProcessThroughput::new(
        site,
        Arc::new(PgWeekSource::new(reader)),
        Arc::clone(ctx.clock()),
        settings,
    ));
    ctx.provide::<dyn ThroughputFeed>(inner.feed())?;
    ctx.add(Arc::new(ThroughputService::new(inner)));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;

    use spot_core::ManualClock;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn the_window_is_the_current_iso_week_and_the_weeks_before_it() {
        // Thursday 2026-11-12: its week starts Monday 2026-11-09 and the next one on 2026-11-16.
        let (since, until) = window(datetime!(2026-11-12 10:30:00 UTC), 2).unwrap();
        assert_eq!(since, datetime!(2026-10-26 00:00:00 UTC));
        assert_eq!(until, datetime!(2026-11-16 00:00:00 UTC));
        // Monday 00:00 itself is the start of its own week.
        let (since, until) = window(datetime!(2026-11-09 00:00:00 UTC), 1).unwrap();
        assert_eq!(since, datetime!(2026-11-02 00:00:00 UTC));
        assert_eq!(until, datetime!(2026-11-16 00:00:00 UTC));
        // Sunday 23:59:59 is still the same week.
        let (_, until) = window(datetime!(2026-11-15 23:59:59 UTC), 1).unwrap();
        assert_eq!(until, datetime!(2026-11-16 00:00:00 UTC));
    }

    #[test]
    fn the_window_crosses_the_iso_year_boundary_and_a_week_53_year() {
        // 2027-01-01 is a Friday in 2026-W53: the week starts Monday 2026-12-28.
        let (since, until) = window(datetime!(2027-01-01 12:00:00 UTC), 1).unwrap();
        assert_eq!(since, datetime!(2026-12-21 00:00:00 UTC));
        assert_eq!(until, datetime!(2027-01-04 00:00:00 UTC));
        // 2027-01-04 starts 2027-W01; one week back is the start of 2026-W53.
        let (since, until) = window(datetime!(2027-01-04 08:00:00 UTC), 1).unwrap();
        assert_eq!(since, datetime!(2026-12-28 00:00:00 UTC));
        assert_eq!(until, datetime!(2027-01-11 00:00:00 UTC));
        // A year back from a week-53 year's last days spans 52 weeks exactly.
        let (since, _) = window(datetime!(2026-12-31 23:59:59 UTC), 52).unwrap();
        assert_eq!(since, datetime!(2025-12-29 00:00:00 UTC));
    }

    #[test]
    fn a_non_utc_instant_is_taken_in_utc() {
        // 2026-11-15 23:30 at -05:00 is Monday 2026-11-16 04:30 UTC: the next ISO week.
        let (_, until) = window(datetime!(2026-11-15 23:30:00 -5), 1).unwrap();
        assert_eq!(until, datetime!(2026-11-23 00:00:00 UTC));
    }

    #[test]
    fn the_settings_ranges_name_the_key() {
        assert!(Settings::default().validate().is_ok());
        let s = |weeks, refresh_secs| Settings {
            enabled: true,
            weeks,
            refresh_secs,
        };
        assert_eq!(s(0, 300).validate(), Err("weeks"));
        assert_eq!(s(105, 300).validate(), Err("weeks"));
        assert_eq!(s(1, 300).validate(), Ok(()));
        assert_eq!(s(104, 300).validate(), Ok(()));
        assert_eq!(s(26, 29).validate(), Err("refresh_secs"));
        assert_eq!(s(26, 3601).validate(), Err("refresh_secs"));
        assert_eq!(s(26, 30).validate(), Ok(()));
        assert_eq!(s(26, 3600).validate(), Ok(()));
        let unknown = serde_json::json!({"enabled": true, "bogus": 1});
        assert!(serde_json::from_value::<Settings>(unknown).is_err());
    }

    /// A source whose answer the test sets, recording the windows it was asked for.
    struct Scripted {
        answer: Mutex<std::result::Result<Vec<WeeklyThroughput>, ()>>,
        asked: Mutex<Vec<(OffsetDateTime, OffsetDateTime)>>,
    }

    impl Scripted {
        fn new(answer: std::result::Result<Vec<WeeklyThroughput>, ()>) -> Arc<Self> {
            Arc::new(Self {
                answer: Mutex::new(answer),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl WeekSource for Scripted {
        fn weekly(&self, since: OffsetDateTime, until: OffsetDateTime) -> SourceFuture<'_> {
            self.asked.lock().unwrap().push((since, until));
            let answer = self.answer.lock().unwrap().clone();
            Box::pin(async move { answer })
        }
    }

    fn week(process: &str, completed: u64) -> WeeklyThroughput {
        WeeklyThroughput {
            process: process.to_owned(),
            version: 1,
            week: "2026-W46".to_owned(),
            completed,
        }
    }

    fn site(name: &str) -> SiteId {
        SiteId::new(name).unwrap()
    }

    fn service(source: &Arc<Scripted>) -> (ProcessThroughput, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(datetime!(2026-11-12 10:00:00 UTC)));
        let throughput = ProcessThroughput::new(
            site("acme"),
            Arc::clone(source) as Arc<dyn WeekSource>,
            Arc::clone(&clock) as Arc<dyn Clock>,
            Settings {
                weeks: 3,
                ..Settings::default()
            },
        );
        (throughput, clock)
    }

    #[tokio::test]
    async fn before_the_first_refresh_the_feed_cannot_answer_then_it_serves_the_cache() {
        let source = Scripted::new(Ok(vec![week("orders", 4)]));
        let (throughput, _clock) = service(&source);
        let feed = throughput.feed();
        assert_eq!(feed.weeks(&site("acme")), Err(()));
        assert!(!throughput.tried());
        assert!(throughput.refresh_now().await);
        assert!(throughput.tried());
        assert_eq!(feed.weeks(&site("acme")), Ok(vec![week("orders", 4)]));
        // Another site's id is refused, never answered from this cache.
        assert_eq!(feed.weeks(&site("other")), Err(()));
    }

    #[tokio::test]
    async fn an_empty_success_is_an_empty_answer_not_an_error() {
        let source = Scripted::new(Ok(Vec::new()));
        let (throughput, _clock) = service(&source);
        assert!(throughput.refresh_now().await);
        assert_eq!(throughput.feed().weeks(&site("acme")), Ok(Vec::new()));
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_good_snapshot() {
        let source = Scripted::new(Ok(vec![week("orders", 4)]));
        let (throughput, _clock) = service(&source);
        assert!(throughput.refresh_now().await);
        *source.answer.lock().unwrap() = Err(());
        assert!(!throughput.refresh_now().await);
        assert!(!throughput.refresh_now().await);
        assert_eq!(
            throughput.feed().weeks(&site("acme")),
            Ok(vec![week("orders", 4)])
        );
        *source.answer.lock().unwrap() = Ok(vec![week("orders", 5)]);
        assert!(throughput.refresh_now().await);
        assert_eq!(
            throughput.feed().weeks(&site("acme")),
            Ok(vec![week("orders", 5)])
        );
    }

    #[tokio::test]
    async fn a_failure_with_no_snapshot_still_cannot_answer_but_counts_as_tried() {
        let source = Scripted::new(Err(()));
        let (throughput, _clock) = service(&source);
        assert!(!throughput.refresh_now().await);
        assert!(throughput.tried());
        assert_eq!(throughput.feed().weeks(&site("acme")), Err(()));
    }

    #[tokio::test]
    async fn the_window_comes_from_the_site_clock() {
        let source = Scripted::new(Ok(Vec::new()));
        let (throughput, clock) = service(&source);
        throughput.refresh_now().await;
        clock.advance(Duration::from_secs(4 * 24 * 3600));
        throughput.refresh_now().await;
        let asked = source.asked.lock().unwrap().clone();
        // Thursday 2026-11-12 (3 weeks back from Monday 2026-11-09 is 2026-10-19).
        assert_eq!(
            asked[0],
            (
                datetime!(2026-10-19 00:00:00 UTC),
                datetime!(2026-11-16 00:00:00 UTC)
            )
        );
        // Monday 2026-11-16: a new week, so both ends move forward by one week.
        assert_eq!(
            asked[1],
            (
                datetime!(2026-10-26 00:00:00 UTC),
                datetime!(2026-11-23 00:00:00 UTC)
            )
        );
    }

    #[tokio::test]
    async fn the_check_is_unhealthy_only_until_the_first_refresh_has_been_tried() {
        let source = Scripted::new(Err(()));
        let (throughput, _clock) = service(&source);
        let service = ThroughputService::new(Arc::new(throughput));
        assert!(service.check().await.is_err());
        service.inner.refresh_now().await;
        assert!(service.check().await.is_ok(), "a failed first try is tried");
        assert_eq!(service.level(), Level::Optional);
    }
}
