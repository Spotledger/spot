//! The administration operations: the right, consent re-confirmation, audit before change,
//! refusals audited, and the references a link keeps to its trail.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{
    Chain, Env, consent, env, grant_on, link_id, offer, proof, runtime, tenant, tenant_id,
};
use spot_iam_access::{AuthMethod, StepUpPolicy};
use spot_permissions::Principal;
use spot_tenant_link::{
    Carrier, Consent, Direction, Error, LinkDraft, LinkId, LinkState, LinkStore, Offer, Party,
    PolicyStepUp, RejectReason, RevokeReason,
};
use spot_tenant_link_pg::{
    Caller, DenyAll, EnginePermissions, LinkAdmin, LinkPermissions, OpError, Operation,
    PERMISSION_DENIED, PgLinkStore,
};

/// Holds `link.manage` for every operation if the user is named `admin`, and for no one else.
struct ByName;
impl LinkPermissions for ByName {
    fn may(&self, principal: &Principal, _: Operation, _: Option<&LinkId>) -> bool {
        principal.is_system || principal.user == "admin"
    }
}

fn admin_caller() -> Caller {
    Caller::new(Principal::with_roles("admin", &["Link Admin"]))
}

fn gate() -> PolicyStepUp {
    PolicyStepUp::new(StepUpPolicy::default())
}

fn draft_from(local: &Party, peer: &Party, id: u8, offer: Offer) -> LinkDraft {
    LinkDraft::new(
        link_id(id),
        local.clone(),
        peer.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer,
    )
    .unwrap()
}

/// Tenant 1 proposes; tenant 2 stores the received proposal and its offer, as the transport
/// would after `Handshake::from_proposal`.
fn proposed(e: &Env, chain_a: &Chain) -> (PgLinkStore, PgLinkStore, LinkId) {
    let (a, b) = (tenant(1), tenant(2));
    let store_a = e.store(1);
    let store_b = e.store(2);
    let g = gate();
    let admin_a = LinkAdmin::new(&store_a, chain_a, &g, &ByName);
    let a_offer = offer(&a, Direction::Send);
    let link_a = admin_a
        .propose(
            &admin_caller(),
            draft_from(&a, &b, 1, a_offer.clone()),
            consent(&a, "user:admin", 1_100),
            1_200,
        )
        .unwrap();
    assert_eq!(link_a.state, LinkState::Proposed);
    // Tenant 2's copy of the received proposal.
    let mut link_b = link_a.clone();
    link_b.local = b.clone();
    link_b.version = 0;
    store_b.insert(link_b).unwrap();
    e.rt.block_on(store_b.put_offer(&link_a.id, &a_offer))
        .unwrap();
    (store_a, store_b, link_a.id)
}

#[test]
fn propose_stores_the_link_audits_it_and_keeps_a_reference() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain = Chain::default();
    let (store_a, _b, id) = proposed(&e, &chain);
    assert_eq!(chain.kinds(), vec!["tenant_link.proposed"]);
    let g = gate();
    let admin = LinkAdmin::new(&store_a, &chain, &g, &ByName);
    let refs = admin.show_audit(&admin_caller(), &id, 2_000).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].kind, "tenant_link.proposed");
    assert_eq!(refs[0].actor, "user:admin");
    assert_eq!(refs[0].chain_seq, 1);
    // The offer is on record for the later accept and grant.
    let (mine, theirs) = rt.block_on(store_a.offers(&id)).unwrap();
    assert!(mine.is_some() && theirs.is_none());
}

#[test]
fn a_caller_without_link_manage_is_refused_and_the_refusal_is_audited() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain = Chain::default();
    let (store_a, _b, id) = proposed(&e, &chain);
    let g = gate();
    let admin = LinkAdmin::new(&store_a, &chain, &g, &ByName);
    let clerk = Caller::new(Principal::with_roles("clerk", &["Accounts User"]));

    for result in [
        admin
            .reject(&clerk, &id, RejectReason::TermsDeclined, 3_000)
            .map(|_| ()),
        admin
            .revoke(&clerk, &id, RevokeReason::TermsEnded, 3_001)
            .map(|_| ()),
        admin.show(&clerk, &id, 3_002).map(|_| ()),
        admin.show_audit(&clerk, &id, 3_003).map(|_| ()),
        admin.list(&clerk, 3_004).map(|_| ()),
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, OpError::Denied { .. }), "{error}");
        assert_eq!(error.code(), PERMISSION_DENIED);
    }
    // Nothing changed.
    assert_eq!(store_a.get(&id).unwrap().state, LinkState::Proposed);
    assert_eq!(store_a.get(&id).unwrap().version, 1);
    // The ones that name a link left a refusal in the trail (the list names none).
    let refused = chain
        .kinds()
        .iter()
        .filter(|kind| *kind == "tenant_link.refused")
        .count();
    assert_eq!(refused, 4);
    // A principal the audit chain would not accept as an actor is denied too.
    let nameless = Caller::new(Principal::with_roles("two words", &[]));
    assert!(matches!(
        admin.show(&nameless, &id, 3_005).unwrap_err(),
        OpError::Denied { .. }
    ));
    // DenyAll denies even the system.
    let closed = LinkAdmin::new(&store_a, &chain, &g, &DenyAll);
    assert!(matches!(
        closed
            .show(&Caller::new(Principal::system()), &id, 3_006)
            .unwrap_err(),
        OpError::Denied { .. }
    ));
}

#[test]
fn the_permission_engine_adapter_denies_a_tenant_that_defined_no_link_right() {
    let registry = spot_meta::Registry::new(Vec::new()).unwrap();
    let snapshot = spot_permissions::Snapshot::empty(&registry.effective()).unwrap();
    let permissions = EnginePermissions::new(spot_permissions::PermissionEngine::new(
        std::sync::Arc::new(snapshot),
    ));
    let person = Principal::with_roles("admin", &["Link Admin"]);
    for operation in Operation::all() {
        assert!(
            !permissions.may(&person, operation, None),
            "{}",
            operation.id()
        );
        assert!(permissions.may(&Principal::system(), operation, None));
    }
}

#[test]
fn a_consent_is_reconfirmed_through_the_gate_and_must_be_the_callers() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (a, b) = (tenant(1), tenant(2));
    let store = e.store(1);
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &ByName);
    let draft = || draft_from(&a, &b, 1, offer(&a, Direction::Send));

    // A consent built by hand with a weak, stale proof (Consent has public fields) never went
    // through the gate; the operation checks it again.
    let stale = Consent {
        party: a.clone(),
        actor: "user:admin".to_owned(),
        at: 10_000_000,
        proof: spot_tenant_link::StepUpProof {
            authenticated_at: 0,
            ..proof(10_000_000)
        },
    };
    assert!(matches!(
        admin
            .propose(&admin_caller(), draft(), stale, 10_000_001)
            .unwrap_err(),
        OpError::Link(Error::StepUpRequired { .. })
    ));
    let weak = Consent {
        party: a.clone(),
        actor: "user:admin".to_owned(),
        at: 1_100,
        proof: spot_tenant_link::StepUpProof {
            method: AuthMethod::Password,
            ..proof(1_100)
        },
    };
    assert!(
        admin
            .propose(&admin_caller(), draft(), weak, 1_200)
            .is_err()
    );
    // Another administrator's consent is not the caller's.
    let someone_else = consent(&a, "user:other", 1_100);
    assert!(matches!(
        admin
            .propose(&admin_caller(), draft(), someone_else, 1_200)
            .unwrap_err(),
        OpError::Link(Error::ConsentInvalid { .. })
    ));
    // A consent for the peer is not this side's.
    let wrong_party = consent(&b, "user:admin", 1_100);
    assert!(
        admin
            .propose(&admin_caller(), draft(), wrong_party, 1_200)
            .is_err()
    );
    // A tenant proposes only from itself.
    let from_peer = draft_from(&b, &a, 1, offer(&b, Direction::Send));
    assert!(matches!(
        admin
            .propose(
                &admin_caller(),
                from_peer,
                consent(&b, "user:admin", 1_100),
                1_200
            )
            .unwrap_err(),
        OpError::Link(Error::InvalidParty { .. })
    ));
    assert!(store.list_for(&a).unwrap().is_empty());
}

#[test]
fn accept_keeps_the_intersection_and_needs_a_different_administrator() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain_a = Chain::default();
    let (_a, store_b, id) = proposed(&e, &chain_a);
    let chain_b = Chain::default();
    let g = gate();
    let admin_b = LinkAdmin::new(&store_b, &chain_b, &g, &ByName);
    let b = tenant(2);

    // The same administrator on both sides is refused.
    let same_actor = consent(&b, "user:admin", 1_300);
    assert!(matches!(
        admin_b
            .accept(
                &admin_caller(),
                &id,
                offer(&b, Direction::Receive),
                same_actor,
                1_400
            )
            .unwrap_err(),
        OpError::Link(Error::ConsentDuplicate { .. })
    ));
    assert_eq!(store_b.get(&id).unwrap().state, LinkState::Proposed);

    // An offer for a topic the proposer never listed does not cross; the shared topic does.
    let both = Offer::new(
        b.clone(),
        vec![
            common::grant(Direction::Receive),
            grant_on("bpm.extra", Direction::Receive, &["extra.value"], 100_000),
        ],
    )
    .unwrap();
    struct Admin2;
    impl LinkPermissions for Admin2 {
        fn may(&self, p: &Principal, _: Operation, _: Option<&LinkId>) -> bool {
            p.user == "admin2"
        }
    }
    let admin_b = LinkAdmin::new(&store_b, &chain_b, &g, &Admin2);
    let caller_b = Caller::new(Principal::with_roles("admin2", &["Link Admin"]));
    let accepted = admin_b
        .accept(
            &caller_b,
            &id,
            both,
            consent(&b, "user:admin2", 1_300),
            1_400,
        )
        .unwrap();
    assert_eq!(accepted.state, LinkState::AcceptedByPeer);
    assert_eq!(accepted.consents.len(), 2);
    assert_eq!(accepted.grants.len(), 1);
    assert_eq!(accepted.grants[0].topic.as_str(), "bpm.message");
    assert_eq!(accepted.grants[0].direction, Direction::Receive);
    assert_eq!(accepted.version, 2);
    assert_eq!(chain_b.kinds(), vec!["tenant_link.accepted"]);
    // A second accept is an invalid transition.
    assert!(
        admin_b
            .accept(
                &caller_b,
                &id,
                offer(&b, Direction::Receive),
                consent(&b, "user:admin2", 1_300),
                1_500
            )
            .is_err()
    );
}

#[test]
fn the_proposer_cannot_accept_its_own_proposal() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain = Chain::default();
    let (store_a, _b, id) = proposed(&e, &chain);
    let g = gate();
    let admin = LinkAdmin::new(&store_a, &chain, &g, &ByName);
    let a = tenant(1);
    let error = admin
        .accept(
            &admin_caller(),
            &id,
            offer(&a, Direction::Receive),
            consent(&a, "user:admin", 1_300),
            1_400,
        )
        .unwrap_err();
    assert!(
        matches!(error, OpError::Link(Error::WrongStep { .. })),
        "{error}"
    );
}

#[test]
fn reject_ends_a_link_still_in_handshake_and_not_an_active_one() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain = Chain::default();
    let (_a, store_b, id) = proposed(&e, &chain);
    let g = gate();
    let admin = LinkAdmin::new(&store_b, &chain, &g, &ByName);
    let rejected = admin
        .reject(&admin_caller(), &id, RejectReason::TermsDeclined, 2_000)
        .unwrap();
    assert_eq!(rejected.state, LinkState::Revoked);
    assert_eq!(rejected.revocation.unwrap().by, tenant(2));
    assert!(chain.kinds().contains(&"tenant_link.rejected".to_owned()));
    assert!(matches!(
        admin
            .reject(&admin_caller(), &id, RejectReason::TermsDeclined, 2_001)
            .unwrap_err(),
        OpError::Link(Error::InvalidTransition { .. })
    ));

    // An active link is revoked, not rejected.
    let mut other = common::linked().b;
    other.id = link_id(9);
    let active = store_b.insert(other).unwrap();
    assert!(matches!(
        admin
            .reject(
                &admin_caller(),
                &active.id,
                RejectReason::TermsDeclined,
                2_002
            )
            .unwrap_err(),
        OpError::Link(Error::InvalidTransition { .. })
    ));
}

#[test]
fn grant_is_never_wider_than_either_offer_and_names_the_dimension() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let both = common::linked();
    let link = store.insert(both.a.clone()).unwrap();
    // The two offers the handshake agreed: A sends, B receives.
    rt.block_on(store.put_offer(&link.id, &offer(&tenant(1), Direction::Send)))
        .unwrap();
    rt.block_on(store.put_offer(&link.id, &offer(&tenant(2), Direction::Receive)))
        .unwrap();
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &ByName);

    // Narrower: fewer fields and an earlier expiry are fine.
    let narrower = grant_on("bpm.message", Direction::Send, &["message.body"], 50_000);
    let updated = admin
        .grant(&admin_caller(), &link.id, narrower.clone(), 5_000)
        .unwrap();
    assert_eq!(updated.grants[0].expires, 50_000);
    assert_eq!(updated.version, 2);
    assert!(chain.kinds().contains(&"tenant_link_pg.granted".to_owned()));

    for (label, wider, dimension) in [
        (
            "a field neither offer lists",
            grant_on(
                "bpm.message",
                Direction::Send,
                &["message.body", "message.secret"],
                50_000,
            ),
            "fields",
        ),
        (
            "a later expiry",
            grant_on("bpm.message", Direction::Send, &["message.body"], 200_000),
            "expiry",
        ),
        (
            "both directions",
            grant_on("bpm.message", Direction::Both, &["message.body"], 50_000),
            "direction",
        ),
        (
            "a topic neither offer lists",
            grant_on("bpm.other", Direction::Send, &["x.y"], 50_000),
            "topic",
        ),
    ] {
        match admin
            .grant(&admin_caller(), &link.id, wider, 5_001)
            .unwrap_err()
        {
            OpError::Link(Error::GrantWider {
                dimension: found, ..
            }) => {
                assert_eq!(found, dimension, "{label}");
            }
            other => panic!("{label}: {other}"),
        }
    }
    // Nothing the refusals touched changed the link.
    assert_eq!(store.get(&link.id).unwrap(), updated);
}

#[test]
fn grant_needs_an_active_link() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let chain = Chain::default();
    let (store_a, _b, id) = proposed(&e, &chain);
    let g = gate();
    let admin = LinkAdmin::new(&store_a, &chain, &g, &ByName);
    assert!(matches!(
        admin
            .grant(&admin_caller(), &id, common::grant(Direction::Send), 3_000)
            .unwrap_err(),
        OpError::Link(Error::NotActive { .. })
    ));
}

#[test]
fn an_audit_failure_stops_the_change_and_a_store_failure_is_recorded() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = store.insert(common::linked().a).unwrap();
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &ByName);

    chain.set_failing(true);
    let error = admin
        .revoke(&admin_caller(), &link.id, RevokeReason::TermsEnded, 6_000)
        .unwrap_err();
    assert!(
        matches!(error, OpError::Link(Error::AuditFailed { .. })),
        "{error}"
    );
    chain.set_failing(false);
    // The audit came first, failed, and the link is still active.
    assert_eq!(store.get(&link.id).unwrap().state, LinkState::Active);
    assert_eq!(store.get(&link.id).unwrap().version, 1);

    // A store conflict after the audit leaves a change_failed record behind.
    let stale = store.get(&link.id).unwrap();
    store
        .update(
            stale
                .revoked_by(&stale.local, RevokeReason::TermsEnded, 6_001)
                .unwrap(),
            1,
        )
        .unwrap();
    let before = chain.records().len();
    let error = admin
        .reject(
            &admin_caller(),
            &link.id,
            RejectReason::TermsDeclined,
            6_002,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        OpError::Link(Error::InvalidTransition { .. })
    ));
    assert_eq!(
        chain.records().len(),
        before,
        "refused before any audit write"
    );
}

#[test]
fn list_show_and_audit_read_back_what_was_done() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = store.insert(common::linked().a).unwrap();
    let chain = Chain::default();
    let g = gate();
    let admin = LinkAdmin::new(&store, &chain, &g, &ByName);
    let caller = admin_caller();
    assert_eq!(admin.list(&caller, 7_000).unwrap().len(), 1);
    assert_eq!(admin.show(&caller, &link.id, 7_000).unwrap().id, link.id);
    admin
        .revoke(&caller, &link.id, RevokeReason::TermsEnded, 7_001)
        .unwrap();
    let refs = admin.show_audit(&caller, &link.id, 7_002).unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].kind, "tenant_link.revoked");
    assert_eq!(refs[0].revision, 1);
    assert_eq!(refs[0].at, 7_001);
    assert!(matches!(
        admin.show(&caller, &link_id(99), 7_003).unwrap_err(),
        OpError::Link(Error::NotFound { .. })
    ));
    let _ = tenant_id(1);
}
