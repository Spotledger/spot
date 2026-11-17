//! Provider audiences (WP-IAM-11.T8, ADR-083 sections 2.3 and 6.1 task 8): a consumer provider
//! resolves only to a consumer sign-in, a staff or partner provider never produces one, and the
//! tenant sign-in policy governs staff and partner only.

mod support;

use std::cell::Cell;

use proptest::prelude::*;
use spot_iam_directory::{LinkedIdentity, UserType};
use spot_sso::{
    Audience, DirectoryUser, DirectoryView, JitPolicy, JitRule, ProviderConfig, ProviderId,
    RawProviderConfig, Resolution, SignInMode, SignInPolicy, SsoIdentity, UserOptions,
    check_policy, decide_sso_signin_for, link_identity_for, resolve, resolve_for_audience,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use support::*;

/// A provider configuration for `audience`.
fn provider(audience: Audience) -> ProviderConfig {
    let mut config = TestIdp::new().config();
    config.audience = audience;
    config
}

/// An identity from the fake provider.
fn identity(subject: &str, email: &str, verified: bool) -> SsoIdentity {
    SsoIdentity {
        provider: ProviderId::new("acme"),
        issuer: ISSUER.to_owned(),
        subject: subject.to_owned(),
        email: Some(email.to_owned()),
        email_verified: verified,
        display_name: Some("Ada Lovelace".to_owned()),
        groups: vec!["engineering".to_owned()],
        acr: None,
        amr: Vec::new(),
        auth_time: None,
        raw_claims_hash: "hash".to_owned(),
    }
}

/// A policy that allows SSO with the fake provider and, when `jit` is set, a just-in-time rule
/// for the e-mail domain `example.com` creating `user_type`.
fn policy(jit: Option<UserType>) -> SignInPolicy {
    SignInPolicy {
        mode: SignInMode::SsoOptional,
        providers: vec![ProviderId::new("acme")],
        jit: match jit {
            Some(user_type) => JitPolicy::RuleBased(vec![JitRule {
                email_domain: Some("example.com".to_owned()),
                group: None,
                user_type,
            }]),
            None => JitPolicy::Off,
        },
        ..SignInPolicy::local()
    }
}

/// A directory view that counts every call, so a test can prove a path never reads it.
struct Spy<'a> {
    inner: &'a FakeDirectory,
    calls: Cell<usize>,
    audience: Audience,
}

impl<'a> Spy<'a> {
    fn new(inner: &'a FakeDirectory, audience: Audience) -> Self {
        Self {
            inner,
            calls: Cell::new(0),
            audience,
        }
    }

    fn touch(&self) {
        self.calls.set(self.calls.get() + 1);
    }
}

impl DirectoryView for Spy<'_> {
    fn user(&self, id: RecordId) -> Option<DirectoryUser> {
        self.touch();
        self.inner.user(id)
    }
    fn enabled_tenant_admins(&self) -> usize {
        self.touch();
        self.inner.enabled_tenant_admins()
    }
    fn enabled_break_glass_admins(&self) -> usize {
        self.touch();
        self.inner.enabled_break_glass_admins()
    }
    fn provider_healthy(&self, provider: &ProviderId) -> bool {
        self.touch();
        self.inner.provider_healthy(provider)
    }
    fn provider_audience(&self, _provider: &ProviderId) -> Audience {
        self.audience
    }
    fn user_for_email(&self, email: &str) -> Option<RecordId> {
        self.touch();
        self.inner.user_for_email(email)
    }
    fn linked_user(&self, issuer: &str, subject: &str) -> Option<RecordId> {
        self.touch();
        self.inner.linked_user(issuer, subject)
    }
    fn open_invitation(&self, email: &str, now: OffsetDateTime) -> Option<RecordId> {
        self.touch();
        self.inner.open_invitation(email, now)
    }
}

/// The four situations of the matrix, each in a directory of its own.
#[derive(Clone, Copy, Debug)]
enum Case {
    Known,
    Invited,
    Jit,
    LocalAccount,
}

/// The directory and the identity for one case; `user_type` is the type of the known or local
/// user.
fn world(case: Case, user_type: UserType) -> (FakeDirectory, SsoIdentity, RecordId) {
    let mut dir = FakeDirectory::new();
    let id = new_id();
    match case {
        Case::Known => {
            let mut u = user(id, "ada@example.com");
            u.user_type = user_type;
            dir.add(u);
            dir.link(ISSUER, "subject-1", id);
        }
        Case::Invited => dir.invite("ada@example.com", id),
        Case::Jit => {}
        Case::LocalAccount => {
            let mut u = user(id, "ada@example.com");
            u.user_type = user_type;
            dir.add(u);
        }
    }
    (dir, identity("subject-1", "ada@example.com", true), id)
}

#[test]
fn a_staff_provider_matrix() {
    let provider = provider(Audience::Staff);
    let jit = policy(Some(UserType::Internal));
    let (dir, who, id) = world(Case::Known, UserType::Internal);
    match resolve_for_audience(&provider, Audience::Staff, &who, &dir, &jit, at(0)) {
        Resolution::SignIn { user, .. } => assert_eq!(user, id),
        other => panic!("known staff identity: {other:?}"),
    }
    let (dir, who, id) = world(Case::Invited, UserType::Internal);
    match resolve_for_audience(&provider, Audience::Staff, &who, &dir, &jit, at(0)) {
        Resolution::LinkViaInvitation { invitation, .. } => assert_eq!(invitation, id),
        other => panic!("invited staff identity: {other:?}"),
    }
    let (dir, who, _) = world(Case::Jit, UserType::Internal);
    match resolve_for_audience(&provider, Audience::Staff, &who, &dir, &jit, at(0)) {
        Resolution::CreateUser { user_type, .. } => assert_eq!(user_type, UserType::Internal),
        other => panic!("jit staff identity: {other:?}"),
    }
    let (dir, who, id) = world(Case::LocalAccount, UserType::Internal);
    match resolve_for_audience(&provider, Audience::Staff, &who, &dir, &jit, at(0)) {
        Resolution::RequireLocalConfirmation { user } => assert_eq!(user, id),
        other => panic!("local account staff identity: {other:?}"),
    }
}

#[test]
fn a_staff_provider_never_signs_in_or_creates_a_portal_user() {
    let provider = provider(Audience::Staff);
    let (dir, who, _) = world(Case::Known, UserType::Portal);
    match resolve_for_audience(&provider, Audience::Staff, &who, &dir, &policy(None), at(0)) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso_audience.wrong_audience"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let (dir, who, _) = world(Case::Jit, UserType::Internal);
    match resolve_for_audience(
        &provider,
        Audience::Staff,
        &who,
        &dir,
        &policy(Some(UserType::Portal)),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso_audience.wrong_audience"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_partner_provider_matrix() {
    let provider = provider(Audience::Partner);
    let jit = policy(Some(UserType::Portal));
    let (dir, who, id) = world(Case::Known, UserType::Portal);
    match resolve_for_audience(&provider, Audience::Partner, &who, &dir, &jit, at(0)) {
        Resolution::SignIn { user, .. } => assert_eq!(user, id),
        other => panic!("known partner identity: {other:?}"),
    }
    let (dir, who, id) = world(Case::Invited, UserType::Portal);
    match resolve_for_audience(&provider, Audience::Partner, &who, &dir, &jit, at(0)) {
        Resolution::LinkViaInvitation { invitation, .. } => assert_eq!(invitation, id),
        other => panic!("invited partner identity: {other:?}"),
    }
    let (dir, who, _) = world(Case::Jit, UserType::Portal);
    match resolve_for_audience(&provider, Audience::Partner, &who, &dir, &jit, at(0)) {
        Resolution::CreateUser { user_type, .. } => assert_eq!(user_type, UserType::Portal),
        other => panic!("jit partner identity: {other:?}"),
    }
    let (dir, who, id) = world(Case::LocalAccount, UserType::Portal);
    match resolve_for_audience(&provider, Audience::Partner, &who, &dir, &jit, at(0)) {
        Resolution::RequireLocalConfirmation { user } => assert_eq!(user, id),
        other => panic!("local account partner identity: {other:?}"),
    }
}

#[test]
fn a_partner_provider_never_signs_in_or_creates_an_internal_user() {
    let provider = provider(Audience::Partner);
    let (dir, who, _) = world(Case::Known, UserType::Internal);
    match resolve_for_audience(
        &provider,
        Audience::Partner,
        &who,
        &dir,
        &policy(None),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso_audience.wrong_audience"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let (dir, who, _) = world(Case::Jit, UserType::Internal);
    match resolve_for_audience(
        &provider,
        Audience::Partner,
        &who,
        &dir,
        &policy(Some(UserType::Internal)),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso_audience.wrong_audience"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_consumer_provider_resolves_only_to_a_consumer_sign_in_in_every_situation() {
    let provider = provider(Audience::Consumer);
    // The most tempting directory: a known link, an invitation, a local account and a
    // just-in-time rule all exist for the identity.
    for case in [Case::Known, Case::Invited, Case::Jit, Case::LocalAccount] {
        let (dir, who, _) = world(case, UserType::Internal);
        let spy = Spy::new(&dir, Audience::Consumer);
        let resolution = resolve_for_audience(
            &provider,
            Audience::Consumer,
            &who,
            &spy,
            &policy(Some(UserType::Internal)),
            at(0),
        );
        assert_eq!(
            resolution,
            Resolution::ConsumerSignIn {
                provider: ProviderId::new("acme"),
                subject: "subject-1".to_owned(),
                email_verified: true,
            },
            "{case:?}"
        );
        // The invitation, local-confirmation, linked-identity and just-in-time branches all
        // read the directory view first; none ran.
        assert_eq!(spy.calls.get(), 0, "{case:?} read the directory");
    }
}

#[test]
fn a_consumer_sign_in_without_a_subject_is_refused() {
    let provider = provider(Audience::Consumer);
    let dir = FakeDirectory::new();
    let mut who = identity("", "ada@example.com", true);
    who.subject = String::new();
    match resolve_for_audience(
        &provider,
        Audience::Consumer,
        &who,
        &dir,
        &policy(None),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.claim_missing"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn every_audience_mismatch_is_refused_and_every_match_is_not() {
    let dir = FakeDirectory::new();
    for provider_audience in Audience::ALL {
        for listener in Audience::ALL {
            let resolution = resolve_for_audience(
                &provider(provider_audience),
                listener,
                &identity("subject-1", "ada@example.com", true),
                &dir,
                &policy(None),
                at(0),
            );
            let wrong = matches!(
                &resolution,
                Resolution::Denied(error) if error.code() == "sso_audience.wrong_audience"
            );
            assert_eq!(
                wrong,
                provider_audience != listener,
                "{provider_audience} provider on the {listener} listener: {resolution:?}"
            );
        }
    }
}

#[test]
fn an_identity_from_another_provider_is_not_resolved_with_this_configuration() {
    let dir = FakeDirectory::new();
    let mut who = identity("subject-1", "ada@example.com", true);
    who.provider = ProviderId::new("other");
    match resolve_for_audience(
        &provider(Audience::Consumer),
        Audience::Consumer,
        &who,
        &dir,
        &policy(None),
        at(0),
    ) {
        Resolution::Denied(error) => assert_eq!(error.code(), "sso.provider_not_configured"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn plain_resolve_is_unchanged_for_a_staff_provider() {
    let (dir, who, id) = world(Case::Known, UserType::Internal);
    match resolve(&who, &dir, &policy(None), at(0)) {
        Resolution::SignIn { user, .. } => assert_eq!(user, id),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_consumer_identity_is_never_linked_to_a_user() {
    let linked = LinkedIdentity {
        user: new_id(),
        issuer: ISSUER.to_owned(),
        subject: "subject-1".to_owned(),
    };
    let error = err(link_identity_for(
        &provider(Audience::Consumer),
        &[],
        &linked,
    ));
    assert_eq!(error.code(), "sso_audience.consumer_cannot_link_user");
    for audience in [Audience::Staff, Audience::Partner] {
        assert!(link_identity_for(&provider(audience), &[], &linked).is_ok());
    }
}

// Loader.

/// A raw configuration for `audience` that passes.
fn raw(audience: Audience) -> RawProviderConfig {
    RawProviderConfig {
        id: ProviderId::new("acme"),
        issuer: ISSUER.to_owned(),
        client_id: "spotledger".to_owned(),
        auth: spot_sso::ClientAuth::ClientSecretRef {
            secret_ref: "providers/acme/client-secret".to_owned(),
        },
        scopes: vec!["openid".to_owned()],
        redirect_uri: REDIRECT_URI.to_owned(),
        discovery_url: format!("{ISSUER}/.well-known/openid-configuration"),
        allowed_algs: vec!["RS256".to_owned()],
        clock_skew_seconds: 0,
        max_logout_token_lifetime_seconds: 120,
        claims_map: spot_sso::ClaimsMap::default(),
        require_email_verified: false,
        acr_values: Vec::new(),
        audience,
        user_options: UserOptions::default(),
    }
}

#[test]
fn the_loader_refuses_each_directory_option_on_a_consumer_provider_by_name() {
    let mut one = raw(Audience::Consumer);
    one.user_options.jit_roles = vec!["clerk".to_owned()];
    let mut two = raw(Audience::Consumer);
    two.user_options.link_by_email = true;
    let mut three = raw(Audience::Consumer);
    three.user_options.directory_user_type = Some(UserType::Internal);
    for (config, name) in [
        (one, "jit_roles"),
        (two, "link_by_email"),
        (three, "directory_user_type"),
    ] {
        let errors = match ProviderConfig::load(config) {
            Err(errors) => errors,
            Ok(_) => panic!("{name} on a consumer provider must be refused"),
        };
        assert!(errors.has_code("sso_audience.audience_option_conflict"));
        let problem = &errors.problems()[0];
        assert_eq!(problem.detail(), Some(name));
    }
}

#[test]
fn the_loader_reports_every_conflict_at_once() {
    let mut config = raw(Audience::Consumer);
    config.user_options = UserOptions {
        jit_roles: vec!["clerk".to_owned()],
        link_by_email: true,
        directory_user_type: Some(UserType::Portal),
    };
    let errors = match ProviderConfig::load(config) {
        Err(errors) => errors,
        Ok(_) => panic!("expected refusals"),
    };
    assert_eq!(errors.problems().len(), 3);
}

#[test]
fn staff_and_partner_providers_may_set_directory_options() {
    for audience in [Audience::Staff, Audience::Partner] {
        let mut config = raw(audience);
        config.user_options = UserOptions {
            jit_roles: vec!["clerk".to_owned()],
            link_by_email: true,
            directory_user_type: Some(UserType::Internal),
        };
        let loaded = ProviderConfig::load(config).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(loaded.audience, audience);
    }
}

#[test]
fn a_consumer_provider_without_options_loads() {
    let loaded = ProviderConfig::load(raw(Audience::Consumer)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(loaded.audience, Audience::Consumer);
    assert!(loaded.user_options.is_empty());
}

#[test]
fn an_old_configuration_without_the_new_keys_is_a_staff_provider() {
    let config = provider(Audience::Staff);
    let mut value = serde_json::to_value(&config).unwrap_or_else(|e| panic!("{e}"));
    let object = value.as_object_mut().unwrap_or_else(|| panic!("an object"));
    assert!(object.remove("audience").is_some());
    assert!(
        !object.contains_key("user_options"),
        "empty options are not written"
    );
    let back: ProviderConfig = serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, config);
}

#[test]
fn the_audience_round_trips_and_an_unknown_name_is_refused() {
    for audience in Audience::ALL {
        let config = provider(audience);
        let text = serde_json::to_string(&config).unwrap_or_else(|e| panic!("{e}"));
        let back: ProviderConfig = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, config);
    }
    let mut value =
        serde_json::to_value(provider(Audience::Staff)).unwrap_or_else(|e| panic!("{e}"));
    value["audience"] = serde_json::json!("Staff");
    assert!(serde_json::from_value::<ProviderConfig>(value).is_err());
}

// Policy.

#[test]
fn a_consumer_sign_in_is_not_subject_to_a_staff_sso_required_policy() {
    let required = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![ProviderId::new("staff-idp")],
        ..SignInPolicy::local()
    };
    // A consumer provider the policy does not list, under `sso_required`, and under `local`.
    let consumer = ProviderId::new("marketplace");
    assert!(decide_sso_signin_for(&required, &consumer, Audience::Consumer).is_ok());
    assert!(decide_sso_signin_for(&SignInPolicy::local(), &consumer, Audience::Consumer).is_ok());
    // Staff and partner stay governed by it.
    for audience in [Audience::Staff, Audience::Partner] {
        let error = err(decide_sso_signin_for(&required, &consumer, audience));
        assert_eq!(error.code(), "sso.provider_not_configured");
        let error = err(decide_sso_signin_for(
            &SignInPolicy::local(),
            &consumer,
            audience,
        ));
        assert_eq!(error.code(), "sso.sso_not_allowed");
    }
    assert!(!SignInPolicy::applies_to(Audience::Consumer));
    assert!(SignInPolicy::applies_to(Audience::Staff));
    assert!(SignInPolicy::applies_to(Audience::Partner));
}

#[test]
fn a_consumer_provider_cannot_satisfy_sso_required() {
    let mut dir = FakeDirectory::new();
    let first = dir.add(user(new_id(), "a@example.com"));
    let second = dir.add(user(new_id(), "b@example.com"));
    dir.make_break_glass(first);
    dir.make_break_glass(second);
    dir.healthy_provider(ProviderId::new("acme"));
    let required = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![ProviderId::new("acme")],
        break_glass: vec![first, second],
        ..SignInPolicy::local()
    };
    // Healthy staff provider: the policy is safe.
    let staff_view = Spy::new(&dir, Audience::Staff);
    assert!(check_policy(&required, &staff_view).is_empty());
    // The same provider, but it signs consumers in: nothing satisfies `sso_required`.
    let consumer_view = Spy::new(&dir, Audience::Consumer);
    let problems = check_policy(&required, &consumer_view);
    assert!(
        problems.iter().any(|p| p.code() == "sso.no_provider"),
        "{problems:?}"
    );
    // A partner provider does count.
    let partner_view = Spy::new(&dir, Audience::Partner);
    assert!(check_policy(&required, &partner_view).is_empty());
}

#[test]
fn no_message_carries_a_subject_an_email_or_a_token() {
    let dir = FakeDirectory::new();
    let who = identity("secret-subject-42", "private.person@example.org", false);
    let linked = LinkedIdentity {
        user: new_id(),
        issuer: ISSUER.to_owned(),
        subject: who.subject.clone(),
    };
    let mut errors = vec![err(link_identity_for(
        &provider(Audience::Consumer),
        &[],
        &linked,
    ))];
    if let Resolution::Denied(error) = resolve_for_audience(
        &provider(Audience::Staff),
        Audience::Consumer,
        &who,
        &dir,
        &policy(None),
        at(0),
    ) {
        errors.push(error);
    }
    let mut config = raw(Audience::Consumer);
    config.user_options.link_by_email = true;
    if let Err(list) = ProviderConfig::load(config) {
        errors.extend(list.problems().iter().cloned());
    }
    assert_eq!(errors.len(), 3);
    for error in errors {
        for text in [
            error.public_message().to_owned(),
            error.admin_message().to_owned(),
            format!("{error:?}"),
            error.to_string(),
        ] {
            assert!(
                !text.contains("secret-subject-42") && !text.contains("private.person"),
                "{text}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Over arbitrary identities and directories, a consumer provider never yields a
    /// resolution that names a user, an invitation or a user type.
    #[test]
    fn a_consumer_provider_never_names_a_user(
        subject in ".{0,40}",
        email in "[a-z]{1,8}@(example|other)\\.com",
        verified in any::<bool>(),
        known in any::<bool>(),
        invited in any::<bool>(),
        local in any::<bool>(),
        jit in any::<bool>(),
        mode in 0u8..3,
    ) {
        let mut dir = FakeDirectory::new();
        let id = new_id();
        if known {
            dir.add(user(id, &email));
            dir.link(ISSUER, &subject, id);
        } else if local {
            dir.add(user(id, &email));
        }
        if invited {
            dir.invite(&email, new_id());
        }
        let mut sign_in = policy(jit.then_some(UserType::Internal));
        sign_in.mode = match mode {
            0 => SignInMode::Local,
            1 => SignInMode::SsoOptional,
            _ => SignInMode::SsoRequired,
        };
        let who = identity(&subject, &email, verified);
        let resolution = resolve_for_audience(
            &provider(Audience::Consumer),
            Audience::Consumer,
            &who,
            &dir,
            &sign_in,
            at(0),
        );
        match resolution {
            Resolution::ConsumerSignIn { .. } | Resolution::Denied(_) => {}
            other => prop_assert!(false, "a consumer provider yielded {other:?}"),
        }
    }
}
