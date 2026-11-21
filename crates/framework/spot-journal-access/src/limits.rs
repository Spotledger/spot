//! Per-principal concurrency and the daily byte budget of segment streams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::model::DAY_MILLIS;

#[derive(Default)]
struct State {
    streams: HashMap<String, u32>,
    /// principal -> (UTC day number, bytes read that day)
    bytes: HashMap<String, (i64, u64)>,
}

/// The limits of one reader.
pub struct Limits {
    max_streams: u32,
    daily_budget: u64,
    state: Arc<Mutex<State>>,
}

/// A held stream slot; dropping it frees the slot.
pub struct Permit {
    principal: String,
    state: Arc<Mutex<State>>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = state.streams.get_mut(&self.principal) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.streams.remove(&self.principal);
            }
        }
    }
}

/// A refusal with the seconds until a retry can succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverLimit {
    /// Whole seconds, at least 1.
    pub retry_after_secs: u64,
}

impl Limits {
    /// Limits of `max_streams` simultaneous streams and `daily_budget` bytes per UTC day.
    #[must_use]
    pub fn new(max_streams: u32, daily_budget: u64) -> Self {
        Self {
            max_streams,
            daily_budget,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Takes a stream slot and charges `bytes` against today's budget, or refuses with nothing
    /// taken. The bytes are charged up front, for the range about to be served.
    ///
    /// # Errors
    /// [`OverLimit`] when the principal already streams the maximum, or the bytes would pass
    /// the daily budget.
    pub fn acquire(
        &self,
        principal: &str,
        bytes: u64,
        now_millis: i64,
    ) -> Result<Permit, OverLimit> {
        let day = now_millis.div_euclid(DAY_MILLIS);
        let mut state = self.lock();
        let open = state.streams.get(principal).copied().unwrap_or(0);
        if open >= self.max_streams {
            return Err(OverLimit {
                retry_after_secs: 1,
            });
        }
        let used = match state.bytes.get(principal) {
            Some(&(d, used)) if d == day => used,
            _ => 0,
        };
        if used.saturating_add(bytes) > self.daily_budget {
            let next_midnight = (day + 1).saturating_mul(DAY_MILLIS);
            let wait = u64::try_from((next_midnight - now_millis).max(1000) / 1000).unwrap_or(1);
            return Err(OverLimit {
                retry_after_secs: wait.max(1),
            });
        }
        state
            .bytes
            .insert(principal.to_owned(), (day, used.saturating_add(bytes)));
        *state.streams.entry(principal.to_owned()).or_insert(0) += 1;
        drop(state);
        Ok(Permit {
            principal: principal.to_owned(),
            state: Arc::clone(&self.state),
        })
    }

    /// Bytes charged to `principal` today (the metric the host exports).
    #[must_use]
    pub fn bytes_today(&self, principal: &str, now_millis: i64) -> u64 {
        let day = now_millis.div_euclid(DAY_MILLIS);
        match self.lock().bytes.get(principal) {
            Some(&(d, used)) if d == day => used,
            _ => 0,
        }
    }

    /// Every principal's bytes today, sorted by principal.
    #[must_use]
    pub fn snapshot(&self, now_millis: i64) -> Vec<(String, u64)> {
        let day = now_millis.div_euclid(DAY_MILLIS);
        let mut out: Vec<(String, u64)> = self
            .lock()
            .bytes
            .iter()
            .filter(|(_, (d, _))| *d == day)
            .map(|(p, (_, n))| (p.clone(), *n))
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_and_budget() {
        let limits = Limits::new(2, 100);
        let a = limits
            .acquire("p", 40, 0)
            .unwrap_or_else(|_| unreachable!());
        let b = limits
            .acquire("p", 40, 0)
            .unwrap_or_else(|_| unreachable!());
        assert!(limits.acquire("p", 1, 0).is_err(), "third stream refused");
        drop(a);
        assert!(limits.acquire("p", 40, 0).is_err(), "budget: 80 + 40 > 100");
        let c = limits.acquire("p", 20, 0);
        assert!(c.is_ok());
        assert_eq!(limits.bytes_today("p", 0), 100);
        // Another principal and the next day start fresh.
        assert!(limits.acquire("q", 100, 0).is_ok());
        drop(b);
        assert!(limits.acquire("p", 100, DAY_MILLIS).is_ok());
        let refused = limits.acquire("p", 1, DAY_MILLIS + 1000);
        assert!(refused.is_err());
    }
}
