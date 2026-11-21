//! WP-IAM-11.T11: every sign-in is admitted through the throttle port before the name is looked
//! up and before any password is hashed; a denial is the existing rate-limited answer; an error
//! from the port refuses the sign-in (fail closed); an unknown name is counted like a real one;
//! the source is the connection's, never one shared bucket.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[allow(unused_imports)]
use common::*;
use spot_login::{
    Admission, AdmissionError, BoxFuture, LoginSource, SignInAdmission, SignInAttempt,
};

const RIGHT: &str = "correct horse battery staple 7731";
const WRONG: &str = "not the password";

fn add(fixture: &Fixture, name: &str) {
    fixture.dir.add(
        StoredUser::new(name, UserState::Active).with_password(fixture.hasher.as_ref(), RIGHT),
    );
}

fn clock() -> ManualClock {
    ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000)))
}

/// What a recording admission saw, with no raw value kept beyond what the port is given.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    tenant: String,
    account: String,
    peer: Option<String>,
    connection_id: Option<String>,
}

/// Answers a fixed verdict and records every attempt.
struct Scripted {
    answer: Result<Admission, AdmissionError>,
    seen: Mutex<Vec<Seen>>,
}

impl Scripted {
    fn new(answer: Result<Admission, AdmissionError>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl SignInAdmission for Scripted {
    fn admit<'a>(
        &'a self,
        attempt: SignInAttempt<'a>,
    ) -> BoxFuture<'a, Result<Admission, AdmissionError>> {
        lock(&self.seen).push(Seen {
            tenant: attempt.tenant.to_owned(),
            account: attempt.account.to_owned(),
            peer: attempt.peer.map(str::to_owned),
            connection_id: attempt.connection_id.map(str::to_owned),
        });
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

#[tokio::test]
async fn a_denied_admission_refuses_before_the_hasher_runs_and_before_the_lookup() {
    let admission = Scripted::new(Ok(Admission::Denied {
        retry_after: Duration::from_millis(2_500),
    }));
    let fixture = fixture_admitted(clock(), LoginConfig::for_tests(), admission.clone());
    add(&fixture, "ada");
    let before = fixture.hasher.verify_count();

    let refused = err(fixture.service.login(&ctx(None), &body("ada", RIGHT)).await);

    // The existing rate-limited answer, with a whole-second Retry-After rounded up.
    assert_code(&refused, "api.rate_limited");
    assert_eq!(
        refused.details.get("retry_after"),
        Some(&serde_json::Value::from(3u64))
    );
    assert_eq!(
        fixture.hasher.verify_count(),
        before,
        "no password was hashed for a denied attempt"
    );
    // The audit row has the existing `throttled` outcome.
    let attempts = lock(&fixture.events.attempts);
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].result_code, "login.throttled");
}

#[tokio::test]
async fn an_unavailable_coordinator_refuses_even_the_correct_password() {
    let admission = Scripted::new(Err(AdmissionError::Unavailable {
        code: "throttle_coordinator.unavailable",
    }));
    let fixture = fixture_admitted(clock(), LoginConfig::for_tests(), admission);
    add(&fixture, "ada");

    let refused = err(fixture.service.login(&ctx(None), &body("ada", RIGHT)).await);

    assert_code(&refused, "server.unavailable");
    assert_eq!(refused.status().as_u16(), 503);
    assert!(refused.details.contains_key("retry_after"));
    assert!(
        !format!("{refused:?}").contains("coordinator"),
        "the answer names no internals: {refused:?}"
    );
    assert_eq!(fixture.hasher.verify_count(), 0);
    assert!(
        lock(&fixture.events.events)
            .iter()
            .all(|event| event.topic != "identity_session_started"),
        "no session was started while admission failed"
    );
}

#[tokio::test]
async fn the_port_gets_the_resolved_tenant_the_name_as_typed_and_no_shared_source() {
    let admission = Scripted::new(Ok(Admission::Allowed));
    let fixture = fixture_admitted(clock(), LoginConfig::for_tests(), admission.clone());
    add(&fixture, "ada");

    // No peer: the source is the per-caller connection key, never empty and never shared.
    let _ = fixture
        .service
        .login(&ctx(None), &body("  Ada ", WRONG))
        .await;
    // A peer: the address is the source.
    let _ = fixture
        .service
        .login_from(
            &ctx(None),
            &body("nobody", WRONG),
            &LoginSource::peer("10.9.8.7"),
        )
        .await;

    let seen = lock(&admission.seen).clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].tenant, site().to_string());
    assert_eq!(seen[0].account, "ada");
    assert_eq!(seen[0].peer, None);
    assert_eq!(seen[0].connection_id.as_deref(), Some("no-peer/ada"));
    assert_eq!(
        seen[1].account, "nobody",
        "an unknown name is passed like a real one"
    );
    assert_eq!(seen[1].peer.as_deref(), Some("10.9.8.7"));
}

#[tokio::test]
async fn an_empty_name_is_admitted_under_a_placeholder_not_skipped() {
    let admission = Scripted::new(Ok(Admission::Allowed));
    let fixture = fixture_admitted(clock(), LoginConfig::for_tests(), admission.clone());
    let refused = err(fixture.service.login(&ctx(None), &body("   ", WRONG)).await);
    assert_code(&refused, "auth.invalid_credentials");
    assert_eq!(lock(&admission.seen)[0].account, "(empty)");
}

#[tokio::test]
async fn an_unknown_name_and_a_real_one_answer_alike_under_the_coordinator() {
    let clock = Arc::new(clock());
    let (admission, _coordinator) = coordinator_admission(clock.clone(), 3, 1_000).await;
    let dir = Arc::new(MemDirectory::default());
    let hasher = Arc::new(CountingHasher::new());
    dir.add(StoredUser::new("ada", UserState::Active).with_password(hasher.as_ref(), RIGHT));
    let service = build(&dir, &hasher, &clock, admission);

    let mut real = Vec::new();
    let mut unknown = Vec::new();
    for n in 0..6 {
        let from = LoginSource::peer(format!("10.1.0.{n}"));
        real.push(shape(
            &service
                .login_from(&ctx(None), &body("ada", WRONG), &from)
                .await,
        ));
        let from = LoginSource::peer(format!("10.2.0.{n}"));
        unknown.push(shape(
            &service
                .login_from(&ctx(None), &body("nosuchuser", WRONG), &from)
                .await,
        ));
    }
    assert_eq!(
        real, unknown,
        "status and body keys match attempt for attempt"
    );
    assert_eq!(real[0].0, "auth.invalid_credentials");
    assert_eq!(real[5].0, "api.rate_limited");
}

#[tokio::test]
async fn ten_failures_from_one_source_never_deny_a_different_source() {
    let clock = Arc::new(clock());
    let (admission, _coordinator) = coordinator_admission(clock.clone(), 1_000, 10).await;
    let dir = Arc::new(MemDirectory::default());
    let hasher = Arc::new(CountingHasher::new());
    dir.add(StoredUser::new("ada", UserState::Active).with_password(hasher.as_ref(), RIGHT));
    let service = build(&dir, &hasher, &clock, admission);

    // Ten different names from source A: each is an ordinary failure.
    for n in 0..10 {
        let result = service
            .login_from(
                &ctx(None),
                &body(&format!("name{n}"), WRONG),
                &LoginSource::peer("10.0.0.1"),
            )
            .await;
        assert_code(&err(result), "auth.invalid_credentials");
    }
    // The eleventh from A is over the source limit.
    let denied = service
        .login_from(
            &ctx(None),
            &body("name10", WRONG),
            &LoginSource::peer("10.0.0.1"),
        )
        .await;
    assert_code(&err(denied), "api.rate_limited");
    // Source B is untouched and signs in.
    let session = service
        .login_from(
            &ctx(None),
            &body("ada", RIGHT),
            &LoginSource::peer("10.0.0.2"),
        )
        .await;
    assert!(session.is_ok(), "{session:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_services_on_one_coordinator_never_admit_more_than_the_account_limit() {
    let clock = Arc::new(clock());
    let (admission, _coordinator) = coordinator_admission(clock.clone(), 5, 100_000).await;
    let dir = Arc::new(MemDirectory::default());
    let hasher = Arc::new(CountingHasher::new());
    dir.add(StoredUser::new("ada", UserState::Active).with_password(hasher.as_ref(), RIGHT));
    let a = Arc::new(build(&dir, &hasher, &clock, admission.clone()));
    let b = Arc::new(build(&dir, &hasher, &clock, admission));

    let mut tasks = Vec::new();
    for n in 0..100 {
        let service = if n % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            let from = LoginSource::peer(format!("10.3.{}.{}", n / 250, n % 250));
            shape(
                &service
                    .login_from(&ctx(None), &body("ada", WRONG), &from)
                    .await,
            )
            .0
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        if task.await.unwrap() == "auth.invalid_credentials" {
            admitted += 1;
        }
    }
    assert!(admitted >= 1, "something is admitted");
    assert!(admitted <= 5, "{admitted} were admitted, the limit is 5");
}

fn build(
    dir: &Arc<MemDirectory>,
    hasher: &Arc<CountingHasher>,
    clock: &Arc<ManualClock>,
    admission: Arc<dyn SignInAdmission>,
) -> LoginService {
    let store: Arc<dyn spot_sessions::SessionStore> = Arc::new(InMemorySessionStore::new());
    let throttle: Arc<dyn ThrottleStore> = Arc::new(InMemoryThrottleStore::new());
    // The service's own back-off is out of the way: these cases are about the admission.
    let mut config = LoginConfig::for_tests();
    config.throttle = common::throttle_config(100_000, 200_000);
    ok(LoginService::new(
        config,
        store,
        throttle,
        dir.clone(),
        Arc::new(MemKeyStore::default()),
        Arc::new(MemEvents::default()),
        clock.clone(),
        hasher.clone(),
        admission,
    ))
}

fn shape(
    result: &Result<spot_http::api::Session, spot_http::api::ApiError>,
) -> (String, Vec<String>) {
    match result {
        Ok(_) => ("ok".to_owned(), Vec::new()),
        Err(error) => {
            let mut keys: Vec<String> = error.details.keys().map(|key| key.to_string()).collect();
            keys.sort();
            (error.code.as_str().to_owned(), keys)
        }
    }
}
