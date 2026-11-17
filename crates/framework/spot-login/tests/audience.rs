//! Session audience and principal (ADR-083 Barrier 1): the audience is chosen once at sign-in
//! from the user's type, a session whose audience does not fit the user is refused, an API key
//! is staff, and a stored session with no readable audience is invalid.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[allow(unused_imports)]
use common::*;

use spot_core::Audience;
use spot_iam_directory::UserType;
use spot_login::{audience_for, parse_user_type};
use spot_sessions::{ClientInfo, SaltKey, SessionManager, SessionPolicy, SessionStore, UserKey};

/// A service over a session store the test can also write to directly (no consumer login
/// exists, so a consumer session can only be made with the store).
struct Rig {
    service: Arc<LoginService>,
    dir: Arc<MemDirectory>,
    keys: Arc<MemKeyStore>,
    hasher: Arc<CountingHasher>,
    store: Arc<InMemorySessionStore>,
}

fn rig() -> Rig {
    let dir = Arc::new(MemDirectory::default());
    let keys = Arc::new(MemKeyStore::default());
    let events = Arc::new(MemEvents::default());
    let hasher = Arc::new(CountingHasher::new());
    let clock = Arc::new(ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(
        1_000_000,
    ))));
    let store = Arc::new(InMemorySessionStore::new());
    let throttle: Arc<dyn ThrottleStore> = Arc::new(InMemoryThrottleStore::new());
    let service = Arc::new(ok(LoginService::new(
        LoginConfig::for_tests(),
        store.clone() as Arc<dyn SessionStore>,
        throttle,
        dir.clone(),
        keys.clone(),
        events,
        clock,
        hasher.clone(),
        Arc::new(spot_login::AllowAll),
    )));
    Rig {
        service,
        dir,
        keys,
        hasher,
        store,
    }
}

impl Rig {
    fn add(&self, name: &str, user_type: UserType) -> RecordId {
        let mut user = StoredUser::new(name, UserState::Active)
            .with_password(self.hasher.as_ref(), "right-password");
        user.user_type = user_type;
        self.dir.add(user)
    }

    /// Starts a session for `user` straight in the store, with `audience` and a fixed time (the
    /// service's clock is at unix second 1,000,000).
    async fn start_directly(&self, user: RecordId, audience: Audience) -> String {
        let manager = SessionManager::new(
            self.store.clone() as Arc<dyn SessionStore>,
            SessionPolicy::DEFAULT,
            Arc::new(SaltKey::generate()),
        );
        let key = ok(UserKey::new(user.to_string()));
        let started = ok(manager
            .start(key, audience, "10.0.0.1", ClientInfo::default(), 1_000_000)
            .await);
        started.token.as_str().to_owned()
    }
}

/// Every user type. The `match` has no wildcard: a new type breaks this test until it is
/// decided here and in `audience_for`.
fn every_user_type() -> [UserType; 4] {
    let all = [
        UserType::Internal,
        UserType::Portal,
        UserType::Service,
        UserType::Agent,
    ];
    for user_type in all {
        match user_type {
            UserType::Internal | UserType::Portal | UserType::Service | UserType::Agent => {}
        }
    }
    all
}

#[test]
fn audience_for_is_exhaustive_and_never_consumer() {
    for user_type in every_user_type() {
        let audience = audience_for(user_type);
        assert_ne!(audience, Audience::Consumer, "{user_type:?}");
        let expected = match user_type {
            UserType::Portal => Audience::Partner,
            UserType::Internal | UserType::Service | UserType::Agent => Audience::Staff,
        };
        assert_eq!(audience, expected);
    }
}

#[test]
fn user_type_text_parses_exactly() {
    assert_eq!(parse_user_type("portal"), Some(UserType::Portal));
    for bad in ["", "Portal", "consumer", "admin", " internal"] {
        assert_eq!(parse_user_type(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn a_staff_login_issues_the_audience_of_the_users_type_and_never_consumer() {
    let rig = rig();
    for (index, user_type) in every_user_type().into_iter().enumerate() {
        let name = format!("user-{index}");
        rig.add(&name, user_type);
        let session = ok(rig
            .service
            .login_with_source(&ctx(None), &body(&name, "right-password"), "10.0.0.1")
            .await);
        let principal = ok(rig.service.authenticate(&ctx(Some(&session.token))).await);
        assert_eq!(principal.audience, audience_for(user_type));
        assert_eq!(principal.user_type, user_type);
        assert_ne!(principal.audience, Audience::Consumer);
        assert_eq!(
            ok(rig.service.audience_of(&ctx(Some(&session.token))).await),
            audience_for(user_type)
        );
    }
}

#[tokio::test]
async fn a_session_whose_audience_does_not_fit_the_user_is_refused_and_stays_refused() {
    let rig = rig();
    let staff_user = rig.add("staff", UserType::Internal);
    let portal_user = rig.add("portal", UserType::Portal);
    // A staff user with a partner session, a portal user with a staff session, and a
    // consumer session for either: none fits.
    let mismatched = [
        rig.start_directly(staff_user, Audience::Partner).await,
        rig.start_directly(portal_user, Audience::Staff).await,
        rig.start_directly(staff_user, Audience::Consumer).await,
        rig.start_directly(portal_user, Audience::Consumer).await,
    ];
    for token in &mismatched {
        let error = err(rig.service.authenticate(&ctx(Some(token))).await);
        assert_code(&error, "auth.session_expired");
        // A second try is still refused (the cache never holds the refused session).
        let again = err(rig.service.audience_of(&ctx(Some(token))).await);
        assert_code(&again, "auth.session_expired");
    }
}

#[tokio::test]
async fn a_matching_directly_started_session_is_accepted_with_its_audience() {
    let rig = rig();
    let portal_user = rig.add("portal", UserType::Portal);
    let token = rig.start_directly(portal_user, Audience::Partner).await;
    let principal = ok(rig.service.authenticate(&ctx(Some(&token))).await);
    assert_eq!(principal.audience, Audience::Partner);
    assert_eq!(principal.user_type, UserType::Portal);
}

#[tokio::test]
async fn an_api_key_principal_is_staff_and_a_portal_owner_is_refused() {
    let rig = rig();
    for (index, user_type) in every_user_type().into_iter().enumerate() {
        let id = rig.add(&format!("owner-{index}"), user_type);
        let (key, stored) = ApiKey::generate();
        rig.keys.add(StoredKey {
            key_id: stored.key_id.clone(),
            digest: *stored.digest.as_bytes(),
            owner: id,
            state: KeyState::Active,
            expires_at: None,
        });
        let result = rig.service.authenticate(&ctx(Some(&key.text()))).await;
        match user_type {
            UserType::Portal => assert_code(&err(result), "auth.invalid_credentials"),
            UserType::Internal | UserType::Service | UserType::Agent => {
                let principal = ok(result);
                assert_eq!(principal.audience, Audience::Staff);
                assert_eq!(principal.user_type, user_type);
            }
        }
    }
}

#[tokio::test]
async fn a_cached_session_keeps_its_audience() {
    let rig = rig();
    rig.add("ada", UserType::Internal);
    let session = ok(rig
        .service
        .login_with_source(&ctx(None), &body("ada", "right-password"), "10.0.0.1")
        .await);
    let first = ok(rig.service.authenticate(&ctx(Some(&session.token))).await);
    let second = ok(rig.service.authenticate(&ctx(Some(&session.token))).await);
    assert_eq!(first.audience, Audience::Staff);
    assert_eq!(second.audience, Audience::Staff);
}

#[tokio::test]
async fn a_missing_credential_is_auth_required_not_an_audience() {
    let rig = rig();
    assert_code(
        &err(rig.service.audience_of(&ctx(None)).await),
        "auth.required",
    );
}
