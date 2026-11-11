//! Shared fixture: one open request, one step at attempt 1, one vault secret and a manual clock.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_bpm_request_authority::{
    FixedSecrets, MemoryRequestStore, ProductionRequestAuthority, SecretRef,
};
use spot_core::ManualClock;
use time::OffsetDateTime;

/// The request every test runs.
pub const REQUEST: &str = "req-7f3a";
/// The step every test acts at.
pub const STEP: &str = "approve";
/// The person the grants are issued to.
pub const ACTOR: &str = "user:asha";
/// Somebody else.
pub const OTHER: &str = "user:ravi";
/// The name the vault secret is stored under.
pub const SECRET_NAME: &str = "bpm.request_grant";
/// The secret's value: thirty-five bytes, well over the minimum.
pub const SECRET: &str = "correct horse battery staple grant key";
/// The record type the step may create.
pub const DOCTYPE: &str = "Purchase Order";
/// The fixed instant every test starts at.
pub const START: i64 = 1_790_000_000;

/// The instant every test starts at.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(START).unwrap()
}

/// `seconds` after [`now`].
pub fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(START + seconds).unwrap()
}

/// One authority over an open request, and everything a test needs to move it.
pub struct Fixture {
    pub store: Arc<MemoryRequestStore>,
    pub clock: Arc<ManualClock>,
    pub secrets: Arc<FixedSecrets>,
    pub authority: ProductionRequestAuthority,
}

impl Fixture {
    /// The standard fixture: [`REQUEST`] runs a purchase process, [`STEP`] is at attempt 1 and may
    /// create [`DOCTYPE`], and the vault holds [`SECRET`] under [`SECRET_NAME`].
    pub fn new() -> Self {
        let store = MemoryRequestStore::new();
        store.open(REQUEST, "purchase");
        store.start_attempt(REQUEST, STEP, 1);
        store.allow(REQUEST, STEP, DOCTYPE);
        let store = store.shared();

        let mut fixed = FixedSecrets::new();
        fixed.insert(SECRET_NAME, SECRET.as_bytes());
        let secrets = fixed.shared();

        let clock = Arc::new(ManualClock::new(now()));
        let authority = ProductionRequestAuthority::new(
            store.clone(),
            secrets.clone(),
            SecretRef::new(SECRET_NAME),
            clock.clone(),
        );
        Self {
            store,
            clock,
            secrets,
            authority,
        }
    }

    /// A grant for `attempt` of [`STEP`], held by [`ACTOR`], valid for ten minutes.
    pub fn issue(&self, attempt: u32) -> String {
        self.authority
            .issue_until(REQUEST, STEP, attempt, ACTOR, at(600))
            .unwrap()
    }
}

/// The three parts of a grant, or a panic when it is not shaped like one.
pub fn parts(grant: &str) -> (&str, &str, &str) {
    let parts: Vec<&str> = grant.split('.').collect();
    assert_eq!(parts.len(), 3, "a grant has three parts: {grant}");
    (parts[0], parts[1], parts[2])
}
