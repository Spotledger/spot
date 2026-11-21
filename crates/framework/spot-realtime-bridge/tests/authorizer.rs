//! The authorizer suite (WP-PLT-08.T3): the room-join matrix over six principals × four
//! room kinds under the TOML permission fixture, the identical-denial rule, the guest and
//! unknown-principal handling, the document-facts look-up, and the revocation evictions
//! of `revalidate` after a permission snapshot swap.

mod support;

use support::Must;

use std::sync::Arc;

use spot_core::SiteId;
use spot_permissions::PermissionEngine;
use spot_realtime::{Denied, JoinError, Principal, RealtimeBus, Room};
use spot_realtime_bridge::PermissionRoomAuthorizer;

use support::{
    GRANTS_A, GRANTS_B, LiveEngine, MapFacts, MapPrincipals, bus_with, doc_a, doc_p1, fact_sheet,
    principal_worlds, shares_for_frank, site, snapshot, stored_id, user_text,
};

type Auth = PermissionRoomAuthorizer<LiveEngine, MapFacts, MapPrincipals>;

struct Stand {
    bus: RealtimeBus,
    site: SiteId,
    auth: Auth,
    engine: Arc<LiveEngine>,
    principals: Arc<MapPrincipals>,
}

fn stand() -> Stand {
    let engine = Arc::new(LiveEngine::new(PermissionEngine::new(Arc::new(snapshot(
        GRANTS_A,
        &shares_for_frank(),
    )))));
    let facts = Arc::new(MapFacts::new(fact_sheet()));
    let principals = Arc::new(MapPrincipals::new(principal_worlds()));
    let auth = PermissionRoomAuthorizer::new(
        Arc::clone(&engine),
        Arc::clone(&facts),
        Arc::clone(&principals),
    );
    // The bus carries the real (permission-checked) authorizer: every join below goes
    // through `PermissionRoomAuthorizer`.
    let bus = bus_with(64, Arc::new(auth.clone()));
    Stand {
        bus,
        site: site("acme.example"),
        auth,
        engine,
        principals,
    }
}

/// The six principals of the matrix; the wire ids are the users' names, which the
/// principal seam maps onto their permission worlds.
fn principals() -> [Principal; 6] {
    [
        Principal::user("alice"),
        Principal::user("bob"),
        Principal::user("carol"),
        Principal::user("dave"),
        Principal::user("erin"),
        Principal::user("frank"),
    ]
}

/// Joins `room` as `who` on a fresh subscription, answering allowed/denied instead of
/// panicking on a denial.
async fn probe(bus: &RealtimeBus, site: &SiteId, who: &Principal, room: &Room) -> bool {
    let mut sub = bus.subscribe(site.clone(), who.clone());
    sub.join(room, None).await.is_ok()
}

#[tokio::test]
async fn the_join_matrix_over_six_principals_and_four_room_kinds() {
    let st = stand();
    let users: Vec<Principal> = principals().to_vec();

    // `user:<id>`: only its user; `site`: any signed-in non-guest.
    for who in &users {
        let own = Room::user(who.id()).must();
        assert!(
            probe(&st.bus, &st.site, who, &own).await,
            "{} joins their own user room",
            who.id()
        );
        let other = users
            .iter()
            .find(|p| p.id() != who.id())
            .map(|p| Room::user(p.id()).must())
            .unwrap_or_else(|| panic!("six principals"));
        assert!(
            !probe(&st.bus, &st.site, who, &other).await,
            "{} must not join another user's room",
            who.id()
        );
        assert!(
            probe(&st.bus, &st.site, who, &Room::site()).await,
            "{} joins the site room",
            who.id()
        );
    }

    // `doctype:<dt>`: list read on the DocType (snapshot A's roles decide). The roles:
    // alice Reader-everywhere, bob Viewer-on-project, carol none, dave Manager-on-project,
    // erin OwnerRole, frank none (his share is on one document, not the list).
    let sales = Room::doctype("sales_invoice").must();
    let project = Room::doctype("project").must();
    let customer = Room::doctype("customer").must();
    assert!(
        probe(&st.bus, &st.site, &users[0], &sales).await,
        "Reader reads the invoice list"
    );
    assert!(probe(&st.bus, &st.site, &users[0], &project).await);
    assert!(probe(&st.bus, &st.site, &users[0], &customer).await);
    assert!(
        probe(&st.bus, &st.site, &users[1], &project).await,
        "Viewer reads the project list"
    );
    assert!(
        !probe(&st.bus, &st.site, &users[1], &sales).await,
        "Viewer must not read the invoice list"
    );
    assert!(!probe(&st.bus, &st.site, &users[1], &customer).await);
    assert!(
        !probe(&st.bus, &st.site, &users[2], &sales).await,
        "no roles, no list"
    );
    assert!(!probe(&st.bus, &st.site, &users[2], &project).await);
    assert!(
        probe(&st.bus, &st.site, &users[3], &project).await,
        "Manager reads the project list"
    );
    assert!(!probe(&st.bus, &st.site, &users[3], &sales).await);
    assert!(
        !probe(&st.bus, &st.site, &users[5], &sales).await,
        "a share is no list right"
    );

    // `doc:<dt>:<id>`: the engine's read decision on the document facts.
    let invoice = Room::document("sales_invoice", &doc_a()).must();
    let project_doc = Room::document("project", &doc_p1()).must();
    assert!(
        probe(&st.bus, &st.site, &users[0], &invoice).await,
        "Reader may read the invoice"
    );
    assert!(probe(&st.bus, &st.site, &users[0], &project_doc).await);
    // `erin` holds no outright read on sales_invoice, but the document is hers
    // (`OwnerRole` with `if_owner`, facts say `created_by: erin`).
    assert!(
        probe(&st.bus, &st.site, &users[4], &invoice).await,
        "the owner reads her own invoice"
    );
    assert!(
        !probe(&st.bus, &st.site, &users[4], &project_doc).await,
        "not a document she may read"
    );
    // `frank` holds a share on the invoice: a share adds rights on one document.
    assert!(
        probe(&st.bus, &st.site, &users[5], &invoice).await,
        "the share admits frank"
    );
    assert!(!probe(&st.bus, &st.site, &users[5], &project_doc).await);

    // Guests are refused every room kind (no public rooms are declared yet).
    let guest = Principal::guest();
    for room in [
        Room::site(),
        Room::user("alice").must(),
        sales.clone(),
        invoice.clone(),
    ] {
        assert!(
            !probe(&st.bus, &st.site, &guest, &room).await,
            "a guest must not join `{}`",
            room.as_str()
        );
    }
}

#[tokio::test]
async fn every_refusal_carries_the_identical_message() {
    let st = stand();
    // Five different refusals: a guest on the site room, a stranger's user room, a list
    // without read, a document the engine denies, a document whose facts cannot be
    // loaded — one message, none revealing whether the resource exists.
    let attempts = [
        (Principal::guest(), Room::site()),
        (Principal::user("alice"), Room::user("bob").must()),
        (
            Principal::user("carol"),
            Room::doctype("sales_invoice").must(),
        ),
        (
            Principal::user("carol"),
            Room::document("sales_invoice", &doc_a()).must(),
        ),
        (
            Principal::user("alice"),
            Room::document("sales_invoice", &stored_id(999).to_string()).must(),
        ),
        (
            Principal::user("zoe"),
            Room::doctype("sales_invoice").must(),
        ),
    ];
    for (who, room) in attempts {
        let mut sub = st.bus.subscribe(st.site.clone(), who.clone());
        match sub.join(&room, None).await {
            Err(JoinError::Denied(denied)) => {
                assert_eq!(
                    denied.reason(),
                    spot_realtime_bridge::DENY_MESSAGE,
                    "the refusal of `{}` on `{}` must not say why",
                    who.id(),
                    room.as_str()
                );
            }
            other => panic!(
                "`{}` on `{}` must be denied, got {other:?}",
                who.id(),
                room.as_str()
            ),
        }
    }
    // The one-word difference the contract pins: a `Denied` is an error variant, never a
    // panic or a `JoinError::Closed`.
    let denied = Denied::new(spot_realtime_bridge::DENY_MESSAGE);
    assert_eq!(denied.reason(), spot_realtime_bridge::DENY_MESSAGE);
}

#[tokio::test]
async fn a_doc_room_of_an_unknown_doctype_refuses_like_any_denial() {
    let st = stand();
    // The fixture's meta does not know `ledger_entry`; the snapshot holds no facts for it.
    let mut sub = st.bus.subscribe(st.site.clone(), Principal::user("alice"));
    match sub.join(&Room::doctype("ledger_entry").must(), None).await {
        Err(JoinError::Denied(denied)) => {
            assert_eq!(denied.reason(), spot_realtime_bridge::DENY_MESSAGE);
        }
        other => panic!("an unknown DocType must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn revalidate_after_a_snapshot_swap_evicts_exactly_the_revoked_rooms() {
    let st = stand();

    let alice = Principal::user("alice");
    let bob = Principal::user("bob");
    let rooms_alice = [
        Room::doctype("sales_invoice").must(),
        Room::doctype("project").must(),
        Room::doctype("customer").must(),
        Room::document("sales_invoice", &doc_a()).must(),
        Room::user("alice").must(),
    ];

    // Snapshot A: alice reads everything (and her own user room follows her).
    for room in &rooms_alice {
        assert!(
            probe(&st.bus, &st.site, &alice, room).await,
            "precondition: alice may join `{}` under snapshot A",
            room.as_str()
        );
    }

    // The swap: only Viewer/project survives the snapshot.
    st.engine
        .swap(PermissionEngine::new(Arc::new(snapshot(GRANTS_B, ""))));

    let evicted = st.auth.revalidate(&st.site, &alice, &rooms_alice).await;
    let names: Vec<_> = evicted.iter().map(|r| r.as_str().to_owned()).collect();
    assert_eq!(
        names,
        [
            "doctype:sales_invoice".to_owned(),
            "doctype:project".to_owned(),
            "doctype:customer".to_owned(),
            format!("doc:sales_invoice:{}", doc_a()),
        ],
        "every permission-decided room alice may no longer read is evicted; her user room never is"
    );

    // Bob keeps the one room he may still read; a user room is never evicted.
    let bob_rooms = [Room::doctype("project").must(), Room::user("bob").must()];
    assert!(
        st.auth
            .revalidate(&st.site, &bob, &bob_rooms)
            .await
            .is_empty(),
        "bob keeps what he may still read, plus his user room"
    );
}

#[tokio::test]
async fn revalidate_on_an_unknown_principal_fails_closed() {
    let st = stand();
    // carol's world disappears (the seam forgets her — a disabled account, a lost
    // look-up): staying joined without a verifiable permission would be the leak.
    st.principals.forget("carol");
    let joined = [Room::doctype("project").must()];
    let evicted = st
        .auth
        .revalidate(&st.site, &Principal::user("carol"), &joined)
        .await;
    assert_eq!(
        evicted
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect::<Vec<_>>(),
        joined
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect::<Vec<_>>(),
        "an unresolvable principal is treated as fully revoked"
    );
}

#[tokio::test]
async fn revalidate_on_a_guest_evicts_everything_evictable() {
    let st = stand();
    let joined = [
        Room::site(),
        Room::user("bob").must(),
        Room::doctype("project").must(),
    ];
    // A guest resolves to no permission world: the evictable rooms are all returned
    // (fail closed); site and user rooms are never evicted — they are not permission
    // questions.
    let evicted = st
        .auth
        .revalidate(&st.site, &Principal::guest(), &joined)
        .await;
    assert_eq!(
        evicted
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect::<Vec<_>>(),
        ["doctype:project".to_owned()],
        "site and user rooms are not permission questions"
    );
}

#[tokio::test]
async fn eviction_is_enforced_on_the_next_join_after_the_swap() {
    let st = stand();
    let sales = Room::doctype("sales_invoice").must();
    let mut sub = st.bus.subscribe(st.site.clone(), Principal::user("alice"));
    sub.join(&sales, None).await.must();

    // The swap revokes alice's read; the host has her session re-join (resume path) —
    // every join is permission-checked, so the rejoin is refused.
    st.engine
        .swap(PermissionEngine::new(Arc::new(snapshot(GRANTS_B, ""))));
    match sub.join(&sales, None).await {
        Err(JoinError::Denied(denied)) => {
            assert_eq!(denied.reason(), spot_realtime_bridge::DENY_MESSAGE);
        }
        other => panic!("a revoked rejoin must be denied, got {other:?}"),
    }
}

#[tokio::test]
async fn a_permission_change_is_enforced_at_the_next_join_too() {
    let st = stand();
    // The same enforcement, from the other side: a world that gains rights admits the
    // next join. `zoe` is unknown; the seam learns her (disabled account re-enabled).
    let zoe = Principal::user("zoe");
    let sales = Room::doctype("sales_invoice").must();
    let mut sub = st.bus.subscribe(st.site.clone(), zoe.clone());
    matches!(sub.join(&sales, None).await, Err(JoinError::Denied(_)))
        .then_some(())
        .unwrap_or_else(|| panic!("precondition: zoe is refused while unknown"));
    st.principals.restore(
        "zoe",
        spot_permissions::model::Principal::with_roles("zoe", &["Reader"]),
    );
    sub.join(&sales, None)
        .await
        .unwrap_or_else(|error| panic!("zoe joins after her assignment lands: {error:?}"));
    let _ = user_text(1); // support stays exercised where the suite needs it
}
