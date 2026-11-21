//! WP-PLT-02.T9: second-factor self-service over the storage-backed stores, on the in-memory
//! backend and on PostgreSQL (`SPOT_TEST_PG_URL`).
//!
//! Codes come from the RFC 6238 vector: seed [`RFC_SEED`], whose codes at counters 1, 2 and 3 are
//! `287082`, `359152` and `969429`. The rig's clock stands at second 60 (counter 2), the service
//! accepts one step either side, and a pending enrolment is seeded through the directory so a test
//! knows its secret (what `totp_enrol` generates is random).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::storage as st;
use common::{CountingHasher, MemEvents};
use spot_auth::TotpSecret;
use spot_http::api::{ApiError, Credential, LoginBody, RequestCtx};
use spot_login::{LoginConfig, LoginService, UserDirectory};
use spot_storage_contract::StorageBackend;
use time::OffsetDateTime;

const NOW: i64 = 60;
const PASSWORD: &str = "correct horse battery staple 7731";
const RFC_SEED: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
const STEP_1: &str = "287082";
const STEP_2: &str = "359152";
const STEP_3: &str = "969429";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("a valid instant")
}

struct Site<B: StorageBackend> {
    rig: st::Rig<B>,
    service: Arc<LoginService>,
    events: Arc<MemEvents>,
    user: st::RecordId,
}

async fn site<B: StorageBackend>(rig: st::Rig<B>) -> Site<B> {
    let user = rig.user("ada", "active").await;
    rig.set_password(user, PASSWORD).await;
    let events = Arc::new(MemEvents::default());
    let service = Arc::new(rig.service_with(
        LoginConfig {
            cache_ttl: std::time::Duration::ZERO,
            ..LoginConfig::for_tests()
        },
        events.clone(),
        Arc::new(CountingHasher::new()),
    ));
    Site {
        rig,
        service,
        events,
        user,
    }
}

impl<B: StorageBackend> Site<B> {
    /// Signs in and answers a request context carrying the session token.
    async fn session(&self, source: &str) -> RequestCtx {
        let session = self
            .service
            .login_with_source(
                &common::ctx(None),
                &LoginBody {
                    user: "ada".to_owned(),
                    password: PASSWORD.to_owned(),
                    totp: None,
                    recovery_code: None,
                },
                source,
            )
            .await
            .expect("a first factor signs in");
        let mut ctx = common::ctx(None);
        ctx.credential = Some(Credential::new(&session.token));
        ctx
    }

    /// Seeds a pending enrolment with the RFC secret.
    async fn pending(&self) {
        self.rig
            .stores
            .user_directory()
            .begin_totp_enrolment(self.user, TotpSecret::from_base32(RFC_SEED).unwrap())
            .await
            .expect("a pending enrolment");
    }

    async fn enrolled(&self) -> (RequestCtx, Vec<String>) {
        let ctx = self.session("10.0.0.1").await;
        self.pending().await;
        let codes = self
            .service
            .totp_confirm(&ctx, STEP_2)
            .await
            .expect("the first code confirms");
        (ctx, codes)
    }

    async fn record(&self) -> spot_login::UserRecord {
        self.rig
            .stores
            .user_directory()
            .find_by_id(self.user)
            .await
            .unwrap()
            .unwrap()
    }
}

fn code_of(error: &ApiError) -> &str {
    error.code.as_str()
}

async fn case_enrol_is_pending_until_confirmed<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    let ctx = s.session("10.0.0.1").await;
    let shown = s.service.totp_enrol(&ctx).await.expect("enrol");
    assert!(shown.provisioning_uri.starts_with("otpauth://"));
    assert!(!shown.secret.is_empty());
    assert!(
        !format!("{shown:?}").contains(&shown.secret),
        "Debug is redacted"
    );
    // Not a factor yet: the record has none, and a sign-in asks for no code.
    assert!(s.record().await.totp_secret.is_none());
    s.session("10.0.0.2").await;
    // A confirm with the wrong code leaves it pending.
    let wrong = s.service.totp_confirm(&ctx, "000000").await.unwrap_err();
    assert_eq!(code_of(&wrong), "auth.totp_invalid");
    assert!(s.record().await.totp_secret.is_none());
}

async fn case_confirm_activates_and_spends_the_step<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    let (_ctx, codes) = s.enrolled().await;
    assert_eq!(codes.len(), 10, "ten recovery codes, once");
    let record = s.record().await;
    assert!(record.totp_secret.is_some(), "the factor is active");
    assert_eq!(record.recovery_digests.len(), 10);
    // The confirming code's step is spent: the same code does not sign in.
    let login = |code: &str, source: &str| {
        let service = s.service.clone();
        let body = LoginBody {
            user: "ada".to_owned(),
            password: PASSWORD.to_owned(),
            totp: Some(code.to_owned()),
            recovery_code: None,
        };
        let source = source.to_owned();
        async move {
            service
                .login_with_source(&common::ctx(None), &body, &source)
                .await
        }
    };
    let replay = login(STEP_2, "10.0.0.3").await.unwrap_err();
    assert_eq!(code_of(&replay), "auth.totp_invalid");
    assert!(
        login(STEP_3, "10.0.0.4").await.is_ok(),
        "the next step signs in"
    );
    assert!(
        lock(&s.events)
            .iter()
            .any(|e| e == "identity_second_factor_enrolled"),
        "the enrolment is audited"
    );
}

fn lock(events: &MemEvents) -> Vec<String> {
    events
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.topic.to_owned())
        .collect()
}

async fn case_enrol_on_an_enrolled_user_is_refused<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    let (ctx, _) = s.enrolled().await;
    let again = s.service.totp_enrol(&ctx).await.unwrap_err();
    assert_eq!(code_of(&again), "identity.totp_state");
    assert_eq!(again.status().as_u16(), 409);
    // Not replaced: the active factor still verifies.
    assert!(s.record().await.totp_secret.is_some());
}

async fn case_disable_needs_the_password_and_a_code<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    // A second session exists before the factor does: after it, a sign-in asks for a code.
    let other = s.session("10.0.0.9").await;
    let (ctx, codes) = s.enrolled().await;

    let bad_password = s
        .service
        .totp_disable(&ctx, "nope", STEP_3)
        .await
        .unwrap_err();
    assert_eq!(code_of(&bad_password), "auth.invalid_credentials");
    let bad_code = s
        .service
        .totp_disable(&ctx, PASSWORD, "000000")
        .await
        .unwrap_err();
    assert_eq!(code_of(&bad_code), "auth.totp_invalid");
    assert!(
        s.record().await.totp_secret.is_some(),
        "a refusal changes nothing"
    );

    // A recovery code is a proof too, and is consumed.
    s.service
        .totp_disable(&ctx, PASSWORD, &codes[0])
        .await
        .expect("the password and a recovery code remove the factor");
    let record = s.record().await;
    assert!(record.totp_secret.is_none());
    assert!(
        record.recovery_digests.is_empty(),
        "the batch went with the factor"
    );
    // The user's other sessions ended; this one did not.
    assert!(s.service.authenticate(&other).await.is_err());
    assert!(s.service.authenticate(&ctx).await.is_ok());
    assert!(
        lock(&s.events)
            .iter()
            .any(|e| e == "identity_second_factor_removed")
    );
    // Nothing to remove now.
    let gone = s
        .service
        .totp_disable(&ctx, PASSWORD, STEP_3)
        .await
        .unwrap_err();
    assert_eq!(code_of(&gone), "identity.totp_state");
}

async fn case_regenerate_replaces_the_batch<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    let (ctx, old) = s.enrolled().await;
    let new = s
        .service
        .totp_regenerate_recovery(&ctx, PASSWORD, STEP_3)
        .await
        .expect("regenerate");
    assert_eq!(new.len(), 10);
    assert!(new.iter().all(|code| !old.contains(code)));
    // The old batch is dead, the new one signs in.
    let try_code = |code: &str, source: &str| {
        let service = s.service.clone();
        let body = LoginBody {
            user: "ada".to_owned(),
            password: PASSWORD.to_owned(),
            totp: None,
            recovery_code: Some(code.to_owned()),
        };
        let source = source.to_owned();
        async move {
            service
                .login_with_source(&common::ctx(None), &body, &source)
                .await
        }
    };
    assert_eq!(
        code_of(&try_code(&old[0], "10.0.1.1").await.unwrap_err()),
        "auth.totp_invalid"
    );
    assert!(try_code(&new[0], "10.0.1.2").await.is_ok());
    assert!(
        lock(&s.events)
            .iter()
            .any(|e| e == "identity_recovery_codes_regenerated")
    );
    // Both proofs are needed.
    let no_password = s
        .service
        .totp_regenerate_recovery(&ctx, "nope", STEP_1)
        .await
        .unwrap_err();
    assert_eq!(code_of(&no_password), "auth.invalid_credentials");
}

async fn case_only_a_person_with_a_session<B: StorageBackend>(rig: st::Rig<B>) {
    let s = site(rig).await;
    // No credential, and a token that is nothing.
    let none = s.service.totp_enrol(&common::ctx(None)).await.unwrap_err();
    assert_eq!(code_of(&none), "auth.required");
    let forged = s
        .service
        .totp_enrol(&common::ctx(Some("not-a-token")))
        .await
        .unwrap_err();
    // A token that does not parse is an expired session, as everywhere else (`authenticate`).
    assert_eq!(code_of(&forged), "auth.session_expired");
}

macro_rules! both {
    ($fake:ident, $pg:ident, $case:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $fake() {
            $case(st::fake_at(at(NOW)).await).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn $pg() {
            if let Some(rig) = st::postgres_at(at(NOW)).await {
                $case(rig).await;
            }
        }
    };
}

both!(
    enrol_is_pending_until_confirmed_on_the_fake_backend,
    enrol_is_pending_until_confirmed_on_postgres,
    case_enrol_is_pending_until_confirmed
);
both!(
    confirm_activates_and_spends_the_step_on_the_fake_backend,
    confirm_activates_and_spends_the_step_on_postgres,
    case_confirm_activates_and_spends_the_step
);
both!(
    enrol_on_an_enrolled_user_is_refused_on_the_fake_backend,
    enrol_on_an_enrolled_user_is_refused_on_postgres,
    case_enrol_on_an_enrolled_user_is_refused
);
both!(
    disable_needs_the_password_and_a_code_on_the_fake_backend,
    disable_needs_the_password_and_a_code_on_postgres,
    case_disable_needs_the_password_and_a_code
);
both!(
    regenerate_replaces_the_batch_on_the_fake_backend,
    regenerate_replaces_the_batch_on_postgres,
    case_regenerate_replaces_the_batch
);
both!(
    only_a_person_with_a_session_on_the_fake_backend,
    only_a_person_with_a_session_on_postgres,
    case_only_a_person_with_a_session
);
