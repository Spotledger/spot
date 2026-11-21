//! Key scope: subset at creation, narrowing on every request, standing on every use.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::{DOCTYPES, engine, engine_with, grants};
use proptest::prelude::*;
use spot_iam_access::{
    AllowRule, AuthOutcome, KeyAction, KeyRecord, KeyScope, OwnerState, ScopeError, Standing,
    check_new_key, codes, standing,
};
use spot_iam_directory::UserType;
use spot_permissions::{
    Action, AppliesTo, DocFacts, Principal, TENANT_ADMINISTRATOR, UserPermission,
};
use spot_storage_contract::Value;

fn scope(rules: &[(&str, &[KeyAction])]) -> KeyScope {
    KeyScope {
        allow: rules
            .iter()
            .map(|(d, a)| AllowRule::new(d, a.iter().copied()))
            .collect(),
        scope: vec![],
        expires_at: None,
    }
}

fn doc(doctype: &str) -> DocFacts {
    DocFacts {
        doctype: doctype.to_owned(),
        ..DocFacts::default()
    }
}

fn restrict_customer(allowed: &[&str]) -> UserPermission {
    UserPermission {
        user: String::new(),
        doctype: None,
        field_or_link_doctype: "customer".to_owned(),
        allowed: allowed.iter().map(|s| (*s).to_owned()).collect(),
        applies_to: AppliesTo::AllDoctypes,
        include_descendants: false,
    }
}

#[test]
fn key_action_round_trips_with_the_engine() {
    for k in KeyAction::all() {
        assert_eq!(KeyAction::from_action(k.to_action()), k);
        assert_eq!(k.to_action().as_str(), k.as_str());
    }
    assert_eq!(
        Action::parse("read").map(KeyAction::from_action),
        Some(KeyAction::Read)
    );
}

#[test]
fn creation_within_the_owners_rights_passes() {
    let owner = Principal::with_roles("svc", &["Clerk"]);
    let s = scope(&[("customer", &[KeyAction::Read, KeyAction::Update])]);
    assert!(s.check_creation(&owner, &engine()).is_ok());
}

#[test]
fn creation_lists_every_excess() {
    let owner = Principal::with_roles("svc", &["Reader"]);
    let s = scope(&[
        (
            "customer",
            &[KeyAction::Read, KeyAction::Update, KeyAction::Delete],
        ),
        ("sales_invoice", &[KeyAction::Post]),
        ("unknown_doctype", &[KeyAction::Read]),
    ]);
    let errors = s.check_creation(&owner, &engine()).unwrap_err();
    let got: Vec<(String, KeyAction)> = errors
        .excesses()
        .iter()
        .map(|x| (x.doctype.clone(), x.action))
        .collect();
    assert_eq!(
        got,
        vec![
            ("customer".to_owned(), KeyAction::Update),
            ("customer".to_owned(), KeyAction::Delete),
            ("sales_invoice".to_owned(), KeyAction::Post),
            ("unknown_doctype".to_owned(), KeyAction::Read),
        ]
    );
    assert!(errors.codes().iter().all(|c| *c == codes::SCOPE_EXCESS));
}

#[test]
fn wildcard_needs_the_action_on_every_known_doctype() {
    let eng = engine();
    let s = scope(&[("*", &[KeyAction::Read])]);
    assert!(
        s.check_creation(&Principal::with_roles("a", &["Reader"]), &eng)
            .is_ok()
    );
    let errors = s
        .check_creation(&Principal::with_roles("a", &["Weak"]), &eng)
        .unwrap_err();
    let x = errors.excesses();
    assert_eq!(x.len(), 1);
    assert_eq!(x[0].doctype, "*");
    assert_eq!(
        x[0].missing_on,
        vec!["project".to_owned(), "sales_invoice".to_owned()]
    );
    // The administrator holds everything.
    let admin = Principal::with_roles("adm", &[TENANT_ADMINISTRATOR]);
    assert!(
        scope(&[("*", &[KeyAction::Delete])])
            .check_creation(&admin, &eng)
            .is_ok()
    );
}

#[test]
fn an_empty_scope_is_refused() {
    let owner = Principal::with_roles("svc", &["Clerk"]);
    let errors = KeyScope::default()
        .check_creation(&owner, &engine())
        .unwrap_err();
    assert_eq!(errors.codes(), vec![codes::SCOPE_EMPTY]);
    let s = scope(&[("customer", &[])]);
    assert!(s.check_creation(&owner, &engine()).is_err());
}

#[test]
fn a_key_shrinks_when_the_owner_loses_a_role_without_being_edited() {
    let owner = Principal::with_roles("svc", &["Clerk"]);
    let s = scope(&[("customer", &[KeyAction::Read, KeyAction::Update])]);
    assert!(s.check_creation(&owner, &engine()).is_ok());
    let d = doc("customer");
    assert!(s.decide(&engine(), &owner, Action::Read, &d).allowed);
    assert!(
        s.decide(&engine(), &owner, Action::parse("update").unwrap(), &d)
            .allowed
    );

    // The owner's role is removed from the snapshot: same key, same owner, new world.
    let without: Vec<_> = grants()
        .into_iter()
        .filter(|g| g.role.as_str() != "Clerk")
        .collect();
    let shrunk = engine_with(without);
    assert!(!s.decide(&shrunk, &owner, Action::Read, &d).allowed);
    assert!(
        !s.decide(&shrunk, &owner, Action::parse("update").unwrap(), &d)
            .allowed
    );

    // Or the owner's assignment is removed.
    let nobody = Principal::user("svc", vec![]);
    assert!(!s.decide(&engine(), &nobody, Action::Read, &d).allowed);
}

#[test]
fn the_allow_list_narrows_the_owner() {
    let owner = Principal::with_roles("svc", &["Clerk"]);
    let s = scope(&[("customer", &[KeyAction::Read])]);
    let d = doc("customer");
    let update = Action::parse("update").unwrap();
    let narrowed = s.decide(&engine(), &owner, update, &d);
    assert!(!narrowed.allowed);
    // Both reasons are present: the owner's grant and the key's refusal.
    assert!(narrowed.reasons.iter().any(|r| matches!(
        r,
        spot_permissions::Reason::NoGrant { wanted, .. } if wanted == "key:update"
    )));
    assert!(narrowed.has_kind("role_grant"));
    assert!(narrowed.reasons.len() >= 2);
    // A DocType the key does not name is refused although the owner may.
    assert!(
        !s.decide(&engine(), &owner, Action::Read, &doc("project"))
            .allowed
    );
}

#[test]
fn the_value_restriction_narrows_the_owner() {
    let owner = Principal::with_roles("svc", &["Clerk"]);
    let mut s = scope(&[("project", &[KeyAction::Read])]);
    s.scope = vec![restrict_customer(&["acme"])];
    let mut inside = doc("project");
    inside.values = BTreeMap::from([("customer".to_owned(), Value::Text("acme".to_owned()))]);
    let mut outside = doc("project");
    outside.values = BTreeMap::from([("customer".to_owned(), Value::Text("other".to_owned()))]);
    assert!(s.decide(&engine(), &owner, Action::Read, &inside).allowed);
    let d = s.decide(&engine(), &owner, Action::Read, &outside);
    assert!(!d.allowed);
    assert!(d.has_kind("user_permission_blocked") || !d.codes().is_empty());
}

#[test]
fn a_wildcard_key_never_exceeds_the_owner() {
    let owner = Principal::with_roles("svc", &["Reader"]);
    let s = scope(&[("*", &KeyAction::all())]);
    for d in DOCTYPES {
        assert!(s.decide(&engine(), &owner, Action::Read, &doc(d)).allowed);
        assert!(
            !s.decide(&engine(), &owner, Action::parse("delete").unwrap(), &doc(d))
                .allowed
        );
    }
}

#[test]
fn if_owner_grants_count_at_creation_but_bind_per_document() {
    let owner = Principal::with_roles("svc", &["Own"]);
    let update = Action::parse("update").unwrap();
    let s = scope(&[("project", &[KeyAction::Update])]);
    assert!(s.check_creation(&owner, &engine()).is_ok());
    let mut mine = doc("project");
    mine.created_by = Some("svc".to_owned());
    let mut theirs = doc("project");
    theirs.created_by = Some("someone".to_owned());
    assert!(s.decide(&engine(), &owner, update, &mine).allowed);
    assert!(!s.decide(&engine(), &owner, update, &theirs).allowed);
}

#[test]
fn scope_round_trips_and_rejects_unknown_fields() {
    let s = scope(&[("customer", &[KeyAction::Read, KeyAction::UpdatePosted])]);
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(serde_json::from_str::<KeyScope>(&json).unwrap(), s);
    assert!(serde_json::from_str::<KeyScope>(r#"{"allow":[],"extra":1}"#).is_err());
    let toml_text = "[[allow]]\ndoctype = \"customer\"\nactions = [\"read\", \"update_posted\"]\n";
    assert_eq!(toml::from_str::<KeyScope>(toml_text).unwrap(), s);
}

fn owner(user_type: UserType, enabled: bool, anonymised: bool) -> OwnerState {
    OwnerState {
        user_type,
        enabled,
        anonymised,
    }
}

fn record(expires_at: Option<i64>, revoked_at: Option<i64>) -> KeyRecord {
    KeyRecord {
        key_id: "abcdefghijkl".to_owned(),
        owner: "svc".to_owned(),
        scope: KeyScope {
            expires_at,
            ..scope(&[("customer", &[KeyAction::Read])])
        },
        revoked_at,
    }
}

#[test]
fn standing_matrix() {
    let ok = owner(UserType::Service, true, false);
    let key = record(Some(100), None);
    assert_eq!(standing(&ok, &key, 99), Standing::Active);
    assert_eq!(standing(&ok, &key, 100), Standing::Expired);
    assert_eq!(
        standing(&owner(UserType::Internal, true, false), &key, 1),
        Standing::Active
    );
    assert_eq!(
        standing(&owner(UserType::Service, false, false), &key, 1),
        Standing::OwnerDisabled
    );
    assert_eq!(
        standing(&owner(UserType::Service, true, true), &key, 1),
        Standing::OwnerAnonymised
    );
    assert_eq!(
        standing(&owner(UserType::Portal, true, false), &key, 1),
        Standing::OwnerNotService
    );
    assert_eq!(
        standing(&owner(UserType::Agent, true, false), &key, 1),
        Standing::OwnerNotService
    );
    assert_eq!(standing(&ok, &record(None, Some(5)), 1), Standing::Revoked);
    assert_eq!(
        standing(&ok, &record(None, None), i64::MAX),
        Standing::Active
    );
}

#[test]
fn deactivating_the_owner_stops_the_key_on_the_next_request() {
    let key = record(None, None);
    let mut state = owner(UserType::Service, true, false);
    assert!(standing(&state, &key, 10).is_active());
    state.enabled = false;
    assert_eq!(standing(&state, &key, 11), Standing::OwnerDisabled);
}

#[test]
fn refusals_look_the_same_whatever_the_reason() {
    let reasons = [
        Standing::OwnerDisabled,
        Standing::OwnerAnonymised,
        Standing::Expired,
        Standing::Revoked,
        Standing::OwnerNotService,
    ];
    let first = AuthOutcome::from_standing(reasons[0]);
    for r in reasons {
        assert_eq!(AuthOutcome::from_standing(r), first);
    }
    assert_eq!(first, AuthOutcome::refused());
    assert!(AuthOutcome::from_standing(Standing::Active).is_authenticated());
    match first {
        AuthOutcome::Refused { code, message } => {
            assert_eq!(code, codes::KEY_NOT_USABLE);
            assert!(!message.to_lowercase().contains("owner"));
        }
        AuthOutcome::Authenticated => panic!("refused expected"),
    }
}

#[test]
fn only_service_or_internal_enabled_owners_may_get_a_key() {
    assert!(check_new_key(&owner(UserType::Service, true, false)).is_ok());
    assert!(check_new_key(&owner(UserType::Internal, true, false)).is_ok());
    assert_eq!(
        check_new_key(&owner(UserType::Portal, true, false)),
        Err(ScopeError::OwnerType)
    );
    assert_eq!(
        check_new_key(&owner(UserType::Agent, true, false)),
        Err(ScopeError::OwnerType)
    );
    assert_eq!(
        check_new_key(&owner(UserType::Service, false, false)),
        Err(ScopeError::OwnerInactive)
    );
    assert_eq!(
        check_new_key(&owner(UserType::Service, true, true)),
        Err(ScopeError::OwnerInactive)
    );
}

#[test]
fn a_key_record_is_made_from_a_stored_key() {
    let (_key, stored) = spot_auth::ApiKey::generate();
    let rec = KeyRecord::new(&stored, "svc", KeyScope::default());
    assert_eq!(rec.key_id, stored.key_id);
}

// ---------------------------------------------------------------- property test

const ROLES: [&str; 5] = ["Reader", "Clerk", "Poster", "Own", "Weak"];
const CUSTOMERS: [&str; 3] = ["acme", "beta", "gamma"];

fn arb_owner() -> impl Strategy<Value = Principal> {
    let assignment = (
        proptest::sample::subsequence(ROLES.to_vec(), 0..=3),
        proptest::option::of(proptest::sample::subsequence(CUSTOMERS.to_vec(), 1..=2)),
    );
    proptest::collection::vec(assignment, 0..=3).prop_map(|list| {
        let assignments = list
            .into_iter()
            .enumerate()
            .map(|(i, (roles, scope))| spot_permissions::AssignmentContext {
                id: format!("a{i}"),
                roles: roles
                    .into_iter()
                    .map(|r| spot_permissions::Role(r.to_owned()))
                    .collect(),
                scope: scope
                    .map(|s| vec![restrict_customer(&s)])
                    .unwrap_or_default(),
            })
            .collect();
        Principal::user("svc", assignments)
    })
}

fn arb_key() -> impl Strategy<Value = KeyScope> {
    let rule = (
        proptest::sample::select(vec!["*", "customer", "sales_invoice", "project"]),
        proptest::sample::subsequence(KeyAction::all().to_vec(), 1..=9),
    );
    (
        proptest::collection::vec(rule, 1..=4),
        proptest::option::of(proptest::sample::subsequence(CUSTOMERS.to_vec(), 1..=3)),
    )
        .prop_map(|(rules, restrict)| KeyScope {
            allow: rules
                .into_iter()
                .map(|(d, a)| AllowRule::new(d, a))
                .collect(),
            scope: restrict
                .map(|r| vec![restrict_customer(&r)])
                .unwrap_or_default(),
            expires_at: None,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// The key's decision is never more permissive than the owner's, for every action and
    /// DocType of the fixture, on documents the owner created and ones it did not.
    #[test]
    fn a_key_is_never_more_permissive_than_its_owner(
        owner in arb_owner(),
        key in arb_key(),
        customer in proptest::sample::select(CUSTOMERS.to_vec()),
        state in proptest::sample::select(vec![None, Some("draft"), Some("posted")]),
        mine in any::<bool>(),
    ) {
        let eng = engine();
        for dt in DOCTYPES {
            for action in KeyAction::all() {
                let mut d = doc(dt);
                d.state = state.map(str::to_owned);
                d.created_by = Some(if mine { "svc" } else { "other" }.to_owned());
                d.values = BTreeMap::from([("customer".to_owned(), Value::Text(customer.to_owned()))]);
                let by_owner = eng.can(&owner, action.to_action(), &d).allowed;
                let by_key = key.decide(&eng, &owner, action.to_action(), &d).allowed;
                prop_assert!(!by_key || by_owner, "{dt} {action:?}");
            }
        }
    }

    /// A scope that passes creation holds nothing the owner lacks on a neutral document.
    #[test]
    fn a_created_key_is_within_the_owners_rights(owner in arb_owner(), key in arb_key()) {
        let eng = engine();
        if key.check_creation(&owner, &eng).is_ok() {
            for rule in &key.allow {
                let dts: Vec<&str> = if rule.doctype == "*" { DOCTYPES.to_vec() } else { vec![rule.doctype.as_str()] };
                for dt in dts {
                    for action in &rule.actions {
                        let mut d = doc(dt);
                        d.created_by = Some("svc".to_owned());
                        prop_assert!(eng.can(&owner, action.to_action(), &d).allowed);
                    }
                }
            }
        }
    }
}
