//! Rate limits: fixed windows per key, in whole seconds from the injected clock.
//!
//! A credential check and a source address are limited separately. The table is bounded: when it
//! is full of live windows a new key is refused (`429`), which fails closed rather than letting a
//! flood of source addresses grow memory.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::config::{Rates, Rule};

/// The most live windows the table holds.
const MAX_WINDOWS: usize = 200_000;

/// What a limit protects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    /// Public reads, per source address.
    Public,
    /// Publish and yank, per publisher key.
    PublishKey,
    /// Publish and yank, per source address.
    PublishAddress,
    /// Review reads, per publisher key.
    Review,
    /// Operator routes, per source address.
    Admin,
    /// Failed credential checks, per source address.
    AuthFailure,
}

struct Window {
    start: i64,
    count: u32,
}

/// The limiter.
pub(crate) struct Limiter {
    rates: Rates,
    windows: Mutex<HashMap<(Class, String), Window>>,
}

impl Limiter {
    pub fn new(rates: Rates) -> Self {
        Self {
            rates,
            windows: Mutex::new(HashMap::new()),
        }
    }

    fn rule(&self, class: Class) -> Rule {
        match class {
            Class::Public => self.rates.public_per_address,
            Class::PublishKey => self.rates.publish_per_key,
            Class::PublishAddress => self.rates.publish_per_address,
            Class::Review => self.rates.review_per_key,
            Class::Admin => self.rates.admin_per_address,
            Class::AuthFailure => self.rates.auth_failures_per_address,
        }
    }

    /// Counts one call for `key` and says how many seconds to wait when the window is spent.
    pub fn hit(&self, class: Class, key: &str, now: i64) -> Result<(), u64> {
        self.touch(class, key, now, true)
    }

    /// Says whether `key` is already over its limit, without counting a call.
    pub fn check(&self, class: Class, key: &str, now: i64) -> Result<(), u64> {
        self.touch(class, key, now, false)
    }

    fn touch(&self, class: Class, key: &str, now: i64, count: bool) -> Result<(), u64> {
        let rule = self.rule(class);
        let window = i64::from(rule.window_secs);
        let mut windows = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
        if windows.len() >= MAX_WINDOWS {
            let rules = |c: Class| i64::from(self.rule(c).window_secs);
            windows.retain(|(c, _), w| now - w.start < rules(*c));
        }
        let full = windows.len() >= MAX_WINDOWS;
        let slot = (class, key.to_owned());
        let entry = match windows.entry(slot) {
            std::collections::hash_map::Entry::Occupied(occupied) => occupied.into_mut(),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                if full {
                    return Err(1);
                }
                if !count {
                    return Ok(());
                }
                vacant.insert(Window {
                    start: now,
                    count: 1,
                });
                return Ok(());
            }
        };
        if now - entry.start >= window || now < entry.start {
            entry.start = now;
            entry.count = 0;
        }
        if entry.count >= rule.max {
            let wait = (entry.start + window - now).max(1);
            return Err(u64::try_from(wait).unwrap_or(1));
        }
        if count {
            entry.count += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Class, Limiter};
    use crate::config::{Rates, Rule};

    fn limiter(max: u32) -> Limiter {
        let rule = Rule {
            max,
            window_secs: 10,
        };
        Limiter::new(Rates {
            public_per_address: rule,
            auth_failures_per_address: rule,
            ..Rates::default()
        })
    }

    #[test]
    fn the_third_call_in_a_window_of_two_waits() {
        let limiter = limiter(2);
        assert!(limiter.hit(Class::Public, "a", 100).is_ok());
        assert!(limiter.hit(Class::Public, "a", 101).is_ok());
        assert_eq!(limiter.hit(Class::Public, "a", 102), Err(8));
    }

    #[test]
    fn keys_and_classes_are_separate_and_the_window_reopens() {
        let limiter = limiter(1);
        assert!(limiter.hit(Class::Public, "a", 100).is_ok());
        assert!(limiter.hit(Class::Public, "b", 100).is_ok());
        assert!(limiter.hit(Class::AuthFailure, "a", 100).is_ok());
        assert!(limiter.hit(Class::Public, "a", 100).is_err());
        assert!(limiter.hit(Class::Public, "a", 110).is_ok());
    }

    #[test]
    fn check_does_not_count() {
        let limiter = limiter(1);
        for _ in 0..5 {
            assert!(limiter.check(Class::AuthFailure, "a", 100).is_ok());
        }
        assert!(limiter.hit(Class::AuthFailure, "a", 100).is_ok());
        assert!(limiter.check(Class::AuthFailure, "a", 100).is_err());
    }
}
