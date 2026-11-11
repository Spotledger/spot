//! Shared support for the native-API suites (WP-PLT-05.T3): the in-memory fake services over
//! [`spot_app_api::stub::StubHost`], and the router plumbing every suite requests through.
//!
//! Each suite declares `mod support;` privately; `dead_code` is allowed at the declaration
//! because a suite uses only the part of the support tree its name implies.

pub mod fake;
pub mod harness;
pub mod permissions;

/// Test-only unwrapping that names the failure. The workspace denies `unwrap_used` and, under
/// `-D warnings`, rejects `expect_used` too; the in-crate `Must` trait is `pub(crate)` +
/// `#[cfg(test)]`, so this separate support module carries its own copy.
pub trait Must<T> {
    /// Unwraps or panics with the error in a named failure message.
    fn must(self) -> T;
}

impl<T, E> Must<T> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}
