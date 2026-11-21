//! The form event's limits: the per-user token bucket and the request deadline.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::prelude::Clock;
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_permissions::Principal;

use super::RateLimit;

/// The in-process token bucket, keyed per user. The coordinator's limiter is the clustered
/// alternative; this one needs no shared state and is what a single server uses.
pub(super) struct RateLimiter {
    limit: RateLimit,
    buckets: Mutex<BTreeMap<String, Bucket>>,
}

struct Bucket {
    tokens: u32,
    refilled_at: Duration,
}

impl RateLimiter {
    pub(super) fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            buckets: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn check(&self, key: &str, now: Duration) -> Result<(), ApiError> {
        let window = self.limit.window.max(Duration::from_millis(1));
        let burst = self.limit.burst.max(1);
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: burst,
            refilled_at: now,
        });
        let elapsed = now.saturating_sub(bucket.refilled_at);
        let windows = elapsed.as_nanos() / window.as_nanos();
        if windows > 0 {
            let added = u32::try_from(windows)
                .unwrap_or(burst)
                .saturating_mul(burst);
            bucket.tokens = burst.min(bucket.tokens.saturating_add(added));
            bucket.refilled_at = now;
        }
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            return Ok(());
        }
        let wait = window.saturating_sub(now.saturating_sub(bucket.refilled_at));
        let retry_after = wait.as_secs().max(1);
        Err(ApiError::new(
            Code::ApiRateLimited,
            "Too many form events; wait for `retry_after` seconds.",
        )
        .detail("retry_after", Json::from(retry_after)))
    }
}

/// The bucket key: one user's form events do not spend another's budget.
pub(super) fn rate_key(principal: &Principal) -> String {
    if principal.is_system {
        "system".to_owned()
    } else {
        format!("user:{}", principal.user)
    }
}

/// The request's remaining budget, checked whenever the future is polled. Dropping the future
/// at any point leaves nothing behind: the form event holds no transaction, lock or write.
pub(super) struct Deadline<F> {
    pub(super) inner: Pin<Box<F>>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) at: Duration,
}

impl<F, T> std::future::Future for Deadline<F>
where
    F: std::future::Future<Output = Result<T, ApiError>>,
{
    type Output = Result<T, ApiError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        if this.clock.monotonic() >= this.at {
            return Poll::Ready(Err(timeout()));
        }
        this.inner.as_mut().poll(cx)
    }
}

fn timeout() -> ApiError {
    ApiError::new(
        Code::HttpTimeout,
        "The form event took longer than the request's remaining budget.",
    )
}
