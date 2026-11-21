//! Recorded security finding: sign-in is throttled by the caller's source. The native API takes that source from
//! `RequestCtx::peer` (half 2), and a request with no peer is bucketed per account rather
//! than into one shared bucket, so a missing address can never back every sign-in off.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[allow(unused_imports)]
use common::*;
use spot_login::LoginSource;

const RIGHT: &str = "correct horse battery staple 7731";
const WRONG: &str = "not the password";
const PEER_A: &str = "10.0.0.1";
const PEER_B: &str = "10.0.0.2";
const PEER_C: &str = "10.0.0.3";

fn add(fixture: &Fixture, name: &str) {
    fixture.dir.add(
        StoredUser::new(name, UserState::Active).with_password(fixture.hasher.as_ref(), RIGHT),
    );
}

/// A request attributed to `peer`, exactly as the HTTP edge sets it on `RequestCtx`.
fn ctx_from(peer: &str) -> RequestCtx {
    let mut request = ctx(None);
    request.peer = Some(peer.parse().expect("a literal address"));
    request
}

/// A fixture whose backoff starts at `backoff_after` failures, so a test can keep a counter below
/// one threshold while crossing another.
fn fixture_with_backoff(backoff_after: u32) -> Fixture {
    fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        LoginConfig {
            throttle: throttle_config(backoff_after, 10),
            ..LoginConfig::for_tests()
        },
    )
}

#[tokio::test]
async fn failures_on_the_native_path_do_not_throttle_other_accounts() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    // Three failures from anyone, through the native `AuthService::login` (no peer address).
    for _ in 0..3 {
        let refused = fixture
            .service
            .login(&ctx(None), &body("ada", "not the password"))
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    // A different real account signs in first time: one caller cannot back off everyone.
    let session = ok(fixture
        .service
        .login(&ctx(None), &body("grace", RIGHT))
        .await);
    assert_eq!(session.user.full_name, "grace");
    // A name that belongs to nobody is answered like any first attempt, not rate limited.
    let unknown = fixture
        .service
        .login(&ctx(None), &body("nobody", RIGHT))
        .await;
    assert_code(&err(unknown), "auth.invalid_credentials");
}

#[tokio::test]
async fn failures_from_one_source_do_not_throttle_another_source() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    let a = LoginSource::peer("10.0.0.1");
    let b = LoginSource::peer("10.0.0.2");
    for _ in 0..3 {
        let refused = fixture
            .service
            .login_from(&ctx(None), &body("ada", "not the password"), &a)
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    let from_b = ok(fixture
        .service
        .login_from(&ctx(None), &body("grace", RIGHT), &b)
        .await);
    assert_eq!(from_b.user.full_name, "grace");
    let unknown = fixture
        .service
        .login_from(&ctx(None), &body("nobody", RIGHT), &b)
        .await;
    assert_code(&err(unknown), "auth.invalid_credentials");
}

#[tokio::test]
async fn a_source_that_keeps_failing_is_backed_off_across_accounts() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    add(&fixture, "heidi");
    let a = LoginSource::peer("10.0.0.1");
    for name in ["ada", "grace", "nobody"] {
        let refused = fixture
            .service
            .login_from(&ctx(None), &body(name, "not the password"), &a)
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    // The per-source ceiling: this source is now slowed on a fresh account too.
    let slowed = fixture
        .service
        .login_from(&ctx(None), &body("heidi", RIGHT), &a)
        .await;
    assert_code(&err(slowed), "api.rate_limited");
}

#[tokio::test]
async fn the_string_source_entry_point_treats_empty_as_no_peer() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    for _ in 0..3 {
        let _ = fixture
            .service
            .login_with_source(&ctx(None), &body("ada", "not the password"), "")
            .await;
    }
    let session = ok(fixture
        .service
        .login_with_source(&ctx(None), &body("grace", RIGHT), "")
        .await);
    assert_eq!(session.user.full_name, "grace");
}

#[tokio::test]
async fn a_peer_failing_across_accounts_is_backed_off_on_a_fourth_account() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    add(&fixture, "heidi");
    for name in ["ada", "grace", "nobody"] {
        let refused = fixture
            .service
            .login(&ctx_from(PEER_A), &body(name, WRONG))
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    // The per-source ceiling, reached through `AuthService::login` (`ctx.peer`, not a header):
    // this peer is refused on an account it never touched.
    let slowed = fixture
        .service
        .login(&ctx_from(PEER_A), &body("heidi", RIGHT))
        .await;
    let refusal = err(slowed);
    assert_code(&refusal, "api.rate_limited");
    // No account enumeration: a name that belongs to nobody is answered identically.
    let invented = fixture
        .service
        .login(&ctx_from(PEER_A), &body("nobody-at-all", RIGHT))
        .await;
    assert_eq!(
        err(invented),
        refusal,
        "the refusal must not name an account"
    );
    // Refused before any hash work: the three failures verified, the fourth attempt did not.
    assert_eq!(
        fixture.hasher.verify_count(),
        3,
        "a throttled attempt must not reach the hasher"
    );
    // Another peer is untouched.
    let session = ok(fixture
        .service
        .login(&ctx_from(PEER_B), &body("heidi", RIGHT))
        .await);
    assert_eq!(session.user.full_name, "heidi");
}

#[tokio::test]
async fn one_peer_failing_does_not_throttle_another_peer_or_another_account() {
    // Backoff after four, so `ada`'s own counter (three failures) stays below its threshold and
    // only peer A's source counter crosses.
    let fixture = fixture_with_backoff(4);
    add(&fixture, "ada");
    add(&fixture, "grace");
    add(&fixture, "heidi");
    for _ in 0..3 {
        let refused = fixture
            .service
            .login(&ctx_from(PEER_A), &body("ada", WRONG))
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    // One more failure on a second account crosses peer A's source ceiling.
    let refused = fixture
        .service
        .login(&ctx_from(PEER_A), &body("nobody", WRONG))
        .await;
    assert_code(&err(refused), "auth.invalid_credentials");
    let refused = fixture
        .service
        .login(&ctx_from(PEER_A), &body("heidi", RIGHT))
        .await;
    assert_code(&err(refused), "api.rate_limited");

    // Peer B signs in to the very account peer A was failing on...
    let b_on_x = ok(fixture
        .service
        .login(&ctx_from(PEER_B), &body("ada", RIGHT))
        .await);
    assert_eq!(b_on_x.user.full_name, "ada");
    // ...and no peer is throttled on an account peer A did not use.
    for peer in [PEER_B, PEER_C] {
        let session = ok(fixture
            .service
            .login(&ctx_from(peer), &body("grace", RIGHT))
            .await);
        assert_eq!(session.user.full_name, "grace");
    }
}

#[tokio::test]
async fn a_missing_peer_is_bucketed_per_account_never_one_shared_bucket() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    add(&fixture, "heidi");
    // Three failures on three accounts from an unknown source. A shared bucket would slow a
    // fourth account; a per-account bucket cannot.
    for name in ["ada", "grace", "nobody"] {
        let refused = fixture.service.login(&ctx(None), &body(name, WRONG)).await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    let session = ok(fixture
        .service
        .login(&ctx(None), &body("heidi", RIGHT))
        .await);
    assert_eq!(session.user.full_name, "heidi");
    // Fail closed, not open: an unknown source is still a ceiling — that of the account it fails
    // on. Two more failures reach `ada`'s own threshold.
    for _ in 0..2 {
        let refused = fixture.service.login(&ctx(None), &body("ada", WRONG)).await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    let slowed = fixture.service.login(&ctx(None), &body("ada", RIGHT)).await;
    assert_code(&err(slowed), "api.rate_limited");
}

#[tokio::test]
async fn a_throttled_sign_in_is_audited_without_the_password_or_a_token() {
    let fixture = fixture();
    add(&fixture, "ada");
    add(&fixture, "grace");
    add(&fixture, "heidi");
    for name in ["ada", "grace", "nobody"] {
        let refused = fixture
            .service
            .login(&ctx_from(PEER_A), &body(name, WRONG))
            .await;
        assert_code(&err(refused), "auth.invalid_credentials");
    }
    let throttled = fixture
        .service
        .login(&ctx_from(PEER_A), &body("heidi", RIGHT))
        .await;
    let error = err(throttled);
    assert_code(&error, "api.rate_limited");

    // The refusal is recorded with its outcome and its code.
    {
        let attempts = lock(&fixture.events.attempts);
        let last = attempts.last().expect("the refused attempt is recorded");
        assert_eq!(last.account, "heidi", "the attempted name");
        assert_eq!(last.result_code, "login.throttled");
        assert!(
            !last.source_digest.is_empty() && !last.source_digest.contains(PEER_A),
            "the record carries a salted digest, never the address"
        );
    }

    // A real sign-in from another peer mints a token; neither the audit record nor the event log
    // may hold that token, either password, or a raw address.
    let session = ok(fixture
        .service
        .login(&ctx_from(PEER_B), &body("heidi", RIGHT))
        .await);
    let trail = format!(
        "{:?} {:?} {error:?}",
        lock(&fixture.events.attempts),
        lock(&fixture.events.events)
    );
    assert!(
        !trail.contains(RIGHT) && !trail.contains(WRONG),
        "no password in the record or the log"
    );
    assert!(
        !trail.contains(&session.token),
        "no token in the record or the log"
    );
    assert!(
        !trail.contains(PEER_A) && !trail.contains(PEER_B),
        "no raw address in the record or the log"
    );
}
