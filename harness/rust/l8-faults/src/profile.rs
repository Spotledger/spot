//! Profiles and the "skip or fail" rule.
//!
//! `SPOT_L8_FAULTS_PROFILE=smoke|standard|nightly` selects how big a scenario run is; the sizes
//! come from the shared L7 profile ([`Profile`]). The PostgreSQL binaries are found through
//! `SPOT_PG_BIN` (as the supervisor's own tests use) or the CI default
//! `/usr/lib/postgresql/18/bin`; when they are absent a scenario prints `SKIPPED` and passes,
//! unless `SPOT_REQUIRE_PG=1` makes the absence a failure instead — so CI can never pass by
//! skipping.

pub use spot_l7_invariants::schedule::Profile;

/// The profile named by `SPOT_L8_FAULTS_PROFILE` (or `SPOT_L7_PROFILE`), or `smoke`.
pub fn profile_from_env() -> Profile {
    std::env::var("SPOT_L8_FAULTS_PROFILE")
        .or_else(|_| std::env::var("SPOT_L7_PROFILE"))
        .ok()
        .and_then(|s| Profile::parse(&s))
        .unwrap_or(Profile::Smoke)
}

/// Whether a missing PostgreSQL must fail (`SPOT_REQUIRE_PG=1`) rather than skip.
pub fn require_pg() -> bool {
    std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1")
}

/// The decision a scenario makes when PostgreSQL is unavailable: `true` means "run".
///
/// With `SPOT_REQUIRE_PG=1` this panics with `what` named; otherwise it prints one `SKIPPED`
/// line and returns `false`.
pub fn skip_or_fail(what: &str) -> bool {
    if require_pg() {
        panic!("SPOT_REQUIRE_PG=1 but {what} needs PostgreSQL, which is not available");
    }
    println!("SKIPPED: {what} needs PostgreSQL (set SPOT_PG_BIN or SPOT_REQUIRE_PG=1)");
    false
}
