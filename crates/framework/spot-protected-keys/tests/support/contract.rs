//! The `KeyStore` contract suite (WP-SEC-10.T4): one set of checks that every implementation must
//! pass. `tests/memory_store.rs` runs it on `MemoryKeyStore`; `spot-protected-keys-pg` compiles
//! this file into its own test with `#[path]` and runs it on a real PostgreSQL server (and, with
//! `SPOT_REQUIRE_PG=1`, cannot skip).
//!
//! Every check works in its own tenant, so one backing database serves the whole suite, and the
//! tenant-isolation check opens a second tenant on the same backing data.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;

use spot_protected_keys::{
    Audit, GroupName, Hub, KeyEpoch, KeyEventKind, KeyStore, KeyStoreError, LockedKey,
    MemoryRootKeyProvider, MonoTime, NodeId, NodeStatus, ProtectedKeyError, RefusalReason,
    ReleaseRecord, RootRef, Rotation, TenantId, WrappedGroupKey, WrappedNodeKey,
};

/// Opens a store of `tenant` over the one backing data every call shares.
pub trait Harness {
    fn open(&self, tenant: &str) -> Arc<dyn KeyStore>;
}

pub fn tenant(name: &str) -> TenantId {
    TenantId::new(name).unwrap()
}
pub fn group(name: &str) -> GroupName {
    GroupName::new(name).unwrap()
}
pub fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}
pub fn epoch(n: u64) -> KeyEpoch {
    KeyEpoch::new(n)
}
pub fn audit(ms: u64) -> Audit {
    Audit::new("hub-admin", MonoTime::from_millis(ms)).unwrap()
}
pub fn root() -> RootRef {
    RootRef {
        id: "memory-root".to_owned(),
        version: 1,
    }
}
/// A wrapped group key whose bytes are `fill` repeated (72 bytes, the size of a real wrap).
pub fn key(group_name: &str, at: u64, fill: u8) -> WrappedGroupKey {
    WrappedGroupKey {
        group: group(group_name),
        epoch: epoch(at),
        root: root(),
        bytes: vec![fill; 72],
    }
}
pub fn node_key(fill: u8) -> WrappedNodeKey {
    WrappedNodeKey {
        root: root(),
        bytes: vec![fill; 72],
    }
}
pub fn lease(n: &str, g: &str, at: u64, until: u64) -> ReleaseRecord {
    ReleaseRecord {
        node: node(n),
        group: group(g),
        epoch: epoch(at),
        lease_until: MonoTime::from_millis(until),
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, KeyStoreError>) -> &'static str {
    result.unwrap_err().code()
}

pub fn run_all(h: &dyn Harness) {
    create_and_read(h);
    rotation_is_compare_and_set(h);
    rotation_race_has_one_winner(h);
    retirement_erases_old_epochs(h);
    node_lifecycle(h);
    release_records_leases(h);
    revoke_then_release_is_refused(h);
    a_failed_revocation_writes_nothing(h);
    revocation_races_release(h);
    tenant_isolation(h);
    history_is_ordered_and_complete(h);
    restart_loses_nothing(h);
    inputs_are_bounded(h);
}

pub fn create_and_read(h: &dyn Harness) {
    let s = h.open("t_create");
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), None);
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(1)));
    assert_eq!(
        s.group_key(&group("bank"), epoch(1)).unwrap(),
        key("bank", 1, 1)
    );
    assert_eq!(s.groups().unwrap(), vec![group("bank")]);
    s.create_group(&key("alpha", 1, 2), &audit(2)).unwrap();
    assert_eq!(s.groups().unwrap(), vec![group("alpha"), group("bank")]);
    assert_eq!(
        code(s.create_group(&key("bank", 1, 9), &audit(3))),
        "protected_keys.group_exists"
    );
    // The refused create changed nothing.
    assert_eq!(
        s.group_key(&group("bank"), epoch(1)).unwrap(),
        key("bank", 1, 1)
    );
    assert_eq!(
        code(s.create_group(&key("late", 2, 1), &audit(3))),
        "protected_keys_pg.bad_input"
    );
    assert_eq!(
        code(s.group_key(&group("nope"), epoch(1))),
        "protected_keys.unknown_group"
    );
    assert_eq!(
        code(s.group_key(&group("bank"), epoch(7))),
        "protected_keys.unknown_epoch"
    );
}

pub fn rotation_is_compare_and_set(h: &dyn Harness) {
    let s = h.open("t_rotate");
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    s.rotate_group(epoch(1), &key("bank", 2, 2), &audit(2))
        .unwrap();
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
    assert_eq!(
        s.group_key(&group("bank"), epoch(1)).unwrap(),
        key("bank", 1, 1)
    );
    assert_eq!(
        s.group_key(&group("bank"), epoch(2)).unwrap(),
        key("bank", 2, 2)
    );
    // A second rotation from the epoch already left loses and writes nothing.
    let lost = s.rotate_group(epoch(1), &key("bank", 2, 3), &audit(3));
    assert_eq!(code(lost), "protected_keys_pg.epoch_conflict");
    assert_eq!(
        s.group_key(&group("bank"), epoch(2)).unwrap(),
        key("bank", 2, 2)
    );
    // A stale or future `from` is a conflict as well.
    assert_eq!(
        code(s.rotate_group(epoch(5), &key("bank", 6, 4), &audit(4))),
        "protected_keys_pg.epoch_conflict"
    );
    // The next key must be exactly one epoch on.
    assert_eq!(
        code(s.rotate_group(epoch(2), &key("bank", 4, 4), &audit(4))),
        "protected_keys_pg.bad_input"
    );
    assert_eq!(
        code(s.rotate_group(epoch(1), &key("ghost", 2, 4), &audit(4))),
        "protected_keys.unknown_group"
    );
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
}

pub fn rotation_race_has_one_winner(h: &dyn Harness) {
    let s = h.open("t_race");
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    let results: Vec<Result<(), KeyStoreError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6u8)
            .map(|i| {
                let s = Arc::clone(&s);
                scope.spawn(move || {
                    s.rotate_group(epoch(1), &key("bank", 2, 10 + i), &audit(2 + u64::from(i)))
                })
            })
            .collect();
        handles.into_iter().map(|j| j.join().unwrap()).collect()
    });
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one hub may rotate one group to its next key"
    );
    for r in results.iter().filter(|r| r.is_err()) {
        assert_eq!(
            r.as_ref().unwrap_err().code(),
            "protected_keys_pg.epoch_conflict"
        );
    }
    let stored = s.group_key(&group("bank"), epoch(2)).unwrap();
    assert!(
        stored.bytes[0] >= 10,
        "the stored key is one of the contenders"
    );
    assert!(
        stored.bytes.iter().all(|b| *b == stored.bytes[0]),
        "no mix of two keys"
    );
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
    let rotates = s
        .events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == KeyEventKind::Rotate)
        .count();
    assert_eq!(rotates, 1, "losers leave no event");
}

pub fn retirement_erases_old_epochs(h: &dyn Harness) {
    let s = h.open("t_retire");
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    s.register_node(&node("n1"), &node_key(5), &audit(1))
        .unwrap();
    assert_eq!(
        code(s.retire_epoch(&group("bank"), epoch(1), &audit(2))),
        "protected_keys_pg.bad_input",
        "the current epoch cannot be retired"
    );
    s.rotate_group(epoch(1), &key("bank", 2, 2), &audit(2))
        .unwrap();
    s.retire_epoch(&group("bank"), epoch(1), &audit(3)).unwrap();
    assert_eq!(
        code(s.group_key(&group("bank"), epoch(1))),
        "protected_keys.epoch_retired"
    );
    assert_eq!(
        code(s.retire_epoch(&group("bank"), epoch(1), &audit(4))),
        "protected_keys.epoch_retired"
    );
    assert_eq!(
        code(s.retire_epoch(&group("bank"), epoch(9), &audit(4))),
        "protected_keys.unknown_epoch"
    );
    assert_eq!(
        code(s.retire_epoch(&group("ghost"), epoch(1), &audit(4))),
        "protected_keys.unknown_group"
    );
    let backup = s.backup(&root()).unwrap();
    assert_eq!(
        backup.groups,
        vec![key("bank", 2, 2)],
        "a backup holds live keys only"
    );
    assert_eq!(
        code(s.record_release(&lease("n1", "bank", 1, 100), &audit(5))),
        "protected_keys.epoch_retired"
    );
    s.record_release(&lease("n1", "bank", 2, 100), &audit(5))
        .unwrap();
}

pub fn node_lifecycle(h: &dyn Harness) {
    let s = h.open("t_node");
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    assert_eq!(s.node(&node("n1")).unwrap(), None);
    s.register_node(&node("n1"), &node_key(5), &audit(10))
        .unwrap();
    let stored = s.node(&node("n1")).unwrap().unwrap();
    assert_eq!(stored.status, NodeStatus::Active);
    assert_eq!(stored.wrap, Some(node_key(5)));
    assert_eq!(stored.registered_at, MonoTime::from_millis(10));

    s.set_node_status(&node("n1"), NodeStatus::Suspended, &audit(11))
        .unwrap();
    assert_eq!(
        s.node(&node("n1")).unwrap().unwrap().status,
        NodeStatus::Suspended
    );
    assert_eq!(
        code(s.record_release(&lease("n1", "bank", 1, 100), &audit(12))),
        "protected_keys.node_not_active"
    );
    s.set_node_status(&node("n1"), NodeStatus::Active, &audit(13))
        .unwrap();
    s.record_release(&lease("n1", "bank", 1, 100), &audit(14))
        .unwrap();

    // A key change keeps the node's holds and brings it back to active.
    s.set_node_status(&node("n1"), NodeStatus::Suspended, &audit(15))
        .unwrap();
    s.register_node(&node("n1"), &node_key(6), &audit(16))
        .unwrap();
    let stored = s.node(&node("n1")).unwrap().unwrap();
    assert_eq!(stored.status, NodeStatus::Active);
    assert_eq!(stored.wrap, Some(node_key(6)));
    assert_eq!(stored.registered_at, MonoTime::from_millis(10));
    assert_eq!(s.held(&node("n1")).unwrap().len(), 1);

    assert_eq!(
        code(s.set_node_status(&node("n1"), NodeStatus::Revoked, &audit(17))),
        "protected_keys_pg.bad_input"
    );
    assert_eq!(
        code(s.set_node_status(&node("ghost"), NodeStatus::Active, &audit(17))),
        "protected_keys.unknown_node"
    );
}

pub fn release_records_leases(h: &dyn Harness) {
    let s = h.open("t_release");
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    s.create_group(&key("pay", 1, 2), &audit(1)).unwrap();
    s.register_node(&node("n1"), &node_key(5), &audit(1))
        .unwrap();
    s.record_release(&lease("n1", "pay", 1, 300), &audit(2))
        .unwrap();
    s.record_release(&lease("n1", "bank", 1, 200), &audit(3))
        .unwrap();
    assert_eq!(
        s.held(&node("n1")).unwrap(),
        vec![lease("n1", "bank", 1, 200), lease("n1", "pay", 1, 300)]
    );
    // A later release of a newer epoch replaces the hold of that group.
    s.rotate_group(epoch(1), &key("bank", 2, 3), &audit(4))
        .unwrap();
    s.record_release(&lease("n1", "bank", 2, 900), &audit(5))
        .unwrap();
    assert_eq!(
        s.held(&node("n1")).unwrap(),
        vec![lease("n1", "bank", 2, 900), lease("n1", "pay", 1, 300)]
    );
    assert_eq!(
        code(s.record_release(&lease("ghost", "bank", 1, 1), &audit(6))),
        "protected_keys.unknown_node"
    );
    assert_eq!(
        code(s.record_release(&lease("n1", "ghost", 1, 1), &audit(6))),
        "protected_keys.unknown_group"
    );
    assert_eq!(
        code(s.record_release(&lease("n1", "bank", 9, 1), &audit(6))),
        "protected_keys.unknown_epoch"
    );
    assert_eq!(
        code(s.record_release(&lease("n1", "bank", 0, 1), &audit(6))),
        "protected_keys_pg.bad_input"
    );
    // The refused releases recorded no event of kind release beyond the three that succeeded.
    let releases = s
        .events(0, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == KeyEventKind::Release)
        .count();
    assert_eq!(releases, 3);
}

fn setup_two_groups(s: &dyn KeyStore) {
    s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    s.create_group(&key("pay", 1, 2), &audit(1)).unwrap();
    s.register_node(&node("n1"), &node_key(5), &audit(1))
        .unwrap();
    s.register_node(&node("n2"), &node_key(6), &audit(1))
        .unwrap();
    s.record_release(&lease("n1", "bank", 1, 500), &audit(2))
        .unwrap();
    s.record_release(&lease("n1", "pay", 1, 500), &audit(2))
        .unwrap();
    s.record_release(&lease("n2", "bank", 1, 500), &audit(2))
        .unwrap();
}

pub fn revoke_then_release_is_refused(h: &dyn Harness) {
    let s = h.open("t_revoke");
    setup_two_groups(&*s);
    let rotations = vec![
        Rotation {
            from: epoch(1),
            next: key("pay", 2, 8),
        },
        Rotation {
            from: epoch(1),
            next: key("bank", 2, 7),
        },
    ];
    let report = s.revoke_node(&node("n1"), &rotations, &audit(10)).unwrap();
    assert_eq!(report.node, node("n1"));
    assert_eq!(report.revoked_at, MonoTime::from_millis(10));
    let groups: Vec<_> = report
        .rotated
        .iter()
        .map(|r| (r.group.as_str(), r.from, r.to))
        .collect();
    assert_eq!(
        groups,
        vec![("bank", epoch(1), epoch(2)), ("pay", epoch(1), epoch(2))]
    );

    let stored = s.node(&node("n1")).unwrap().unwrap();
    assert_eq!(stored.status, NodeStatus::Revoked);
    assert_eq!(stored.wrap, None, "the wrapping path is erased");
    assert!(s.held(&node("n1")).unwrap().is_empty());
    assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
    assert_eq!(
        s.group_key(&group("pay"), epoch(2)).unwrap(),
        key("pay", 2, 8)
    );
    assert_eq!(
        s.revocation_epoch(&node("n1"), &group("bank")).unwrap(),
        Some(epoch(1))
    );
    assert_eq!(
        s.revocation_epoch(&node("n1"), &group("ghost")).unwrap(),
        None
    );
    assert_eq!(
        s.revocation_epoch(&node("n2"), &group("bank")).unwrap(),
        None
    );

    // Revoked, then release: refused, for the old epoch and the new one alike.
    for at in [1, 2] {
        assert_eq!(
            code(s.record_release(&lease("n1", "bank", at, 900), &audit(11))),
            "protected_keys.node_revoked"
        );
    }
    assert!(s.held(&node("n1")).unwrap().is_empty());
    // Other nodes are untouched and read the new epoch.
    s.record_release(&lease("n2", "bank", 2, 900), &audit(12))
        .unwrap();
    assert_eq!(
        code(s.register_node(&node("n1"), &node_key(9), &audit(13))),
        "protected_keys.node_revoked"
    );
    assert_eq!(
        code(s.set_node_status(&node("n1"), NodeStatus::Active, &audit(13))),
        "protected_keys.node_revoked"
    );
    assert_eq!(
        code(s.revoke_node(&node("n1"), &[], &audit(13))),
        "protected_keys.node_revoked"
    );
    assert_eq!(
        code(s.revoke_node(&node("ghost"), &[], &audit(13))),
        "protected_keys.unknown_node"
    );
    // A node that held nothing revokes with no rotation.
    s.register_node(&node("n3"), &node_key(7), &audit(14))
        .unwrap();
    let empty = s.revoke_node(&node("n3"), &[], &audit(15)).unwrap();
    assert!(empty.rotated.is_empty());
    assert_eq!(
        s.node(&node("n3")).unwrap().unwrap().status,
        NodeStatus::Revoked
    );
}

pub fn a_failed_revocation_writes_nothing(h: &dyn Harness) {
    let s = h.open("t_revoke_atomic");
    setup_two_groups(&*s);
    let events_before = s.events(0, 500).unwrap().len();
    let untouched = |s: &dyn KeyStore| {
        assert_eq!(
            s.node(&node("n1")).unwrap().unwrap().status,
            NodeStatus::Active
        );
        assert_eq!(s.held(&node("n1")).unwrap().len(), 2);
        assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(1)));
        assert_eq!(s.group_epoch(&group("pay")).unwrap(), Some(epoch(1)));
        assert_eq!(
            s.revocation_epoch(&node("n1"), &group("bank")).unwrap(),
            None
        );
        assert_eq!(s.events(0, 500).unwrap().len(), events_before);
    };
    // A held group not covered: a release raced the caller's read.
    let partial = vec![Rotation {
        from: epoch(1),
        next: key("bank", 2, 7),
    }];
    assert_eq!(
        code(s.revoke_node(&node("n1"), &partial, &audit(10))),
        "protected_keys_pg.epoch_conflict"
    );
    untouched(&*s);
    // A rotation of a group the node does not hold.
    let extra = vec![
        Rotation {
            from: epoch(1),
            next: key("bank", 2, 7),
        },
        Rotation {
            from: epoch(1),
            next: key("pay", 2, 8),
        },
        Rotation {
            from: epoch(1),
            next: key("ghost", 2, 9),
        },
    ];
    assert_eq!(
        code(s.revoke_node(&node("n1"), &extra, &audit(10))),
        "protected_keys_pg.bad_input"
    );
    untouched(&*s);
    // One group's head moved on: the whole revocation is refused, the other group is not rotated.
    s.rotate_group(epoch(1), &key("pay", 2, 3), &audit(9))
        .unwrap();
    let events_before = s.events(0, 500).unwrap().len();
    let stale = vec![
        Rotation {
            from: epoch(1),
            next: key("bank", 2, 7),
        },
        Rotation {
            from: epoch(1),
            next: key("pay", 2, 8),
        },
    ];
    assert_eq!(
        code(s.revoke_node(&node("n1"), &stale, &audit(10))),
        "protected_keys_pg.epoch_conflict"
    );
    assert_eq!(
        s.node(&node("n1")).unwrap().unwrap().status,
        NodeStatus::Active
    );
    assert_eq!(s.held(&node("n1")).unwrap().len(), 2);
    assert_eq!(
        s.group_epoch(&group("bank")).unwrap(),
        Some(epoch(1)),
        "bank was not rotated"
    );
    assert_eq!(
        s.group_key(&group("pay"), epoch(2)).unwrap(),
        key("pay", 2, 3)
    );
    assert_eq!(s.events(0, 500).unwrap().len(), events_before);
    // A duplicated group and an unusable next epoch are refused before anything runs.
    let twice = vec![
        Rotation {
            from: epoch(1),
            next: key("bank", 2, 7),
        },
        Rotation {
            from: epoch(1),
            next: key("bank", 2, 8),
        },
    ];
    assert_eq!(
        code(s.revoke_node(&node("n1"), &twice, &audit(10))),
        "protected_keys_pg.bad_input"
    );
    let skip = vec![Rotation {
        from: epoch(1),
        next: key("bank", 3, 7),
    }];
    assert_eq!(
        code(s.revoke_node(&node("n1"), &skip, &audit(10))),
        "protected_keys_pg.bad_input"
    );
}

pub fn revocation_races_release(h: &dyn Harness) {
    for round in 0..8u64 {
        let name = format!("t_revrace_{round}");
        let s = h.open(&name);
        s.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
        s.register_node(&node("n1"), &node_key(5), &audit(1))
            .unwrap();
        let (released, revoked) = std::thread::scope(|scope| {
            let a = Arc::clone(&s);
            let release =
                scope.spawn(move || a.record_release(&lease("n1", "bank", 1, 500), &audit(2)));
            let b = Arc::clone(&s);
            let revoke = scope.spawn(move || {
                // The honest caller: read what the node holds, rotate exactly that, retry on a
                // conflict (a release landed between the read and the revocation).
                for _ in 0..20 {
                    let rotations: Vec<Rotation> = b
                        .held(&node("n1"))
                        .unwrap()
                        .iter()
                        .map(|r| Rotation {
                            from: r.epoch,
                            next: key("bank", r.epoch.get() + 1, 7),
                        })
                        .collect();
                    match b.revoke_node(&node("n1"), &rotations, &audit(3)) {
                        Ok(report) => return report,
                        Err(e) => assert_eq!(e.code(), "protected_keys_pg.epoch_conflict"),
                    }
                }
                panic!("the revocation never settled");
            });
            (release.join().unwrap(), revoke.join().unwrap())
        });
        assert!(
            s.held(&node("n1")).unwrap().is_empty(),
            "a revoked node holds nothing"
        );
        assert_eq!(
            s.node(&node("n1")).unwrap().unwrap().status,
            NodeStatus::Revoked
        );
        match released {
            Ok(()) => {
                assert_eq!(
                    revoked.rotated.len(),
                    1,
                    "a release that won is rotated away"
                );
                assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
                assert_eq!(
                    s.revocation_epoch(&node("n1"), &group("bank")).unwrap(),
                    Some(epoch(1))
                );
            }
            Err(e) => {
                assert_eq!(
                    e.code(),
                    "protected_keys.node_revoked",
                    "a release that lost is refused"
                );
                assert!(revoked.rotated.is_empty());
                assert_eq!(s.group_epoch(&group("bank")).unwrap(), Some(epoch(1)));
            }
        }
    }
}

pub fn tenant_isolation(h: &dyn Harness) {
    let a = h.open("t_iso_a");
    let b = h.open("t_iso_b");
    assert_ne!(a.tenant(), b.tenant());
    a.create_group(&key("bank", 1, 1), &audit(1)).unwrap();
    a.register_node(&node("n1"), &node_key(5), &audit(1))
        .unwrap();
    a.record_release(&lease("n1", "bank", 1, 500), &audit(2))
        .unwrap();
    a.rotate_group(epoch(1), &key("bank", 2, 2), &audit(3))
        .unwrap();

    // B sees nothing of A, and a cross-tenant id answers exactly like a missing one.
    assert_eq!(b.group_epoch(&group("bank")).unwrap(), None);
    assert!(b.groups().unwrap().is_empty());
    assert_eq!(b.node(&node("n1")).unwrap(), None);
    assert!(b.held(&node("n1")).unwrap().is_empty());
    assert!(b.events(0, 100).unwrap().is_empty());
    assert_eq!(
        b.revocation_epoch(&node("n1"), &group("bank")).unwrap(),
        None
    );
    assert_eq!(b.backup(&root()).unwrap().groups.len(), 0);
    let missing = h.open("t_iso_missing");
    for store in [&b, &missing] {
        assert_eq!(
            code(store.group_key(&group("bank"), epoch(1))),
            "protected_keys.unknown_group"
        );
        assert_eq!(
            code(store.record_release(&lease("n1", "bank", 1, 9), &audit(4))),
            "protected_keys.unknown_node"
        );
        assert_eq!(
            code(store.set_node_status(&node("n1"), NodeStatus::Suspended, &audit(4))),
            "protected_keys.unknown_node"
        );
        assert_eq!(
            code(store.revoke_node(&node("n1"), &[], &audit(4))),
            "protected_keys.unknown_node"
        );
        assert_eq!(
            code(store.rotate_group(epoch(1), &key("bank", 2, 3), &audit(4))),
            "protected_keys.unknown_group"
        );
        assert_eq!(
            code(store.retire_epoch(&group("bank"), epoch(1), &audit(4))),
            "protected_keys.unknown_group"
        );
    }
    // The same names in B are independent rows.
    b.create_group(&key("bank", 1, 9), &audit(5)).unwrap();
    b.register_node(&node("n1"), &node_key(9), &audit(5))
        .unwrap();
    b.revoke_node(&node("n1"), &[], &audit(6)).unwrap();
    assert_eq!(a.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
    assert_eq!(
        a.group_key(&group("bank"), epoch(1)).unwrap(),
        key("bank", 1, 1)
    );
    assert_eq!(
        a.node(&node("n1")).unwrap().unwrap().status,
        NodeStatus::Active
    );
    assert_eq!(a.held(&node("n1")).unwrap().len(), 1);
    assert!(
        a.events(0, 100)
            .unwrap()
            .iter()
            .all(|e| e.actor == "hub-admin")
    );
    assert_eq!(a.events(0, 100).unwrap().len(), 4);
    assert_eq!(b.events(0, 100).unwrap().len(), 3);
}

pub fn history_is_ordered_and_complete(h: &dyn Harness) {
    let s = h.open("t_events");
    let by = |actor: &str, ms| Audit::new(actor, MonoTime::from_millis(ms)).unwrap();
    s.create_group(&key("bank", 1, 1), &by("alice", 1)).unwrap();
    s.register_node(&node("n1"), &node_key(5), &by("bob", 2))
        .unwrap();
    s.record_release(&lease("n1", "bank", 1, 500), &by("bob", 3))
        .unwrap();
    s.set_node_status(&node("n1"), NodeStatus::Suspended, &by("carol", 4))
        .unwrap();
    s.record_refusal(
        &node("n1"),
        &group("bank"),
        Some(epoch(1)),
        RefusalReason::NodeNotActive,
        &by("bob", 5),
    )
    .unwrap();
    s.set_node_status(&node("n1"), NodeStatus::Active, &by("carol", 6))
        .unwrap();
    s.rotate_group(epoch(1), &key("bank", 2, 2), &by("alice", 7))
        .unwrap();
    s.retire_epoch(&group("bank"), epoch(1), &by("alice", 8))
        .unwrap();
    // The node still holds bank@1 (never refreshed); the head is 2, so the revocation rotates 2 to 3.
    let rot = vec![Rotation {
        from: epoch(2),
        next: key("bank", 3, 4),
    }];
    s.revoke_node(&node("n1"), &rot, &by("dave", 9)).unwrap();

    let events = s.events(0, 100).unwrap();
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            KeyEventKind::Issue,
            KeyEventKind::Register,
            KeyEventKind::Release,
            KeyEventKind::Status,
            KeyEventKind::Refuse,
            KeyEventKind::Status,
            KeyEventKind::Rotate,
            KeyEventKind::Retire,
            KeyEventKind::Rotate,
            KeyEventKind::Revoke,
        ]
    );
    assert!(
        events.windows(2).all(|w| w[0].seq < w[1].seq),
        "strictly increasing"
    );
    let actors: Vec<_> = events.iter().map(|e| e.actor.as_str()).collect();
    assert_eq!(
        actors,
        [
            "alice", "bob", "bob", "carol", "bob", "carol", "alice", "alice", "dave", "dave"
        ]
    );
    let at: Vec<_> = events.iter().map(|e| e.at.millis()).collect();
    assert_eq!(at, [1, 2, 3, 4, 5, 6, 7, 8, 9, 9]);
    let rotate = &events[6];
    assert_eq!(
        (rotate.epoch, rotate.to_epoch),
        (Some(epoch(1)), Some(epoch(2)))
    );
    assert_eq!(events[3].detail.as_deref(), Some("suspended"));
    assert_eq!(
        events[4].detail.as_deref(),
        Some("protected_keys.node_not_active")
    );
    assert_eq!(events[4].node, Some(node("n1")));
    assert_eq!(events[9].node, Some(node("n1")));

    // Paging by cursor.
    let first = s.events(0, 4).unwrap();
    assert_eq!(first.len(), 4);
    let rest = s.events(first[3].seq, 100).unwrap();
    assert_eq!(rest.len(), 6);
    assert_eq!(rest[0], events[4]);
    assert!(s.events(events[9].seq, 10).unwrap().is_empty());
    assert_eq!(code(s.events(0, 0)), "protected_keys_pg.bad_input");
    assert_eq!(code(s.events(0, 501)), "protected_keys_pg.bad_input");
}

pub fn restart_loses_nothing(h: &dyn Harness) {
    let name = "t_restart";
    let secret = [7u8; 32];
    let provider = || {
        MemoryRootKeyProvider::from_key(tenant(name), &LockedKey::from_bytes(&secret).unwrap())
            .unwrap()
    };
    // A real hub makes real wrapped keys, bound to the tenant, group and epoch.
    let mut hub = Hub::new(provider());
    hub.create_group(&group("bank"), MonoTime::ZERO).unwrap();
    hub.create_group(&group("pay"), MonoTime::ZERO).unwrap();
    hub.rotate_group(&group("bank"), MonoTime::from_millis(1))
        .unwrap();
    let backup = hub.backup();
    let root_ref = RootRef {
        id: backup.root.id.clone(),
        version: backup.root.version,
    };

    let first = h.open(name);
    for k in backup.groups.iter().filter(|k| k.epoch == KeyEpoch::FIRST) {
        first.create_group(k, &audit(1)).unwrap();
    }
    for k in backup.groups.iter().filter(|k| k.epoch != KeyEpoch::FIRST) {
        first
            .rotate_group(KeyEpoch::new(k.epoch.get() - 1), k, &audit(2))
            .unwrap();
    }
    first
        .register_node(&node("n1"), &node_key(5), &audit(3))
        .unwrap();
    first
        .record_release(&lease("n1", "bank", 2, 777), &audit(4))
        .unwrap();
    first
        .register_node(&node("n2"), &node_key(6), &audit(5))
        .unwrap();
    first
        .record_release(&lease("n2", "pay", 1, 888), &audit(5))
        .unwrap();
    first
        .revoke_node(
            &node("n2"),
            &[Rotation {
                from: epoch(1),
                next: {
                    let mut k = key("pay", 2, 0);
                    k.bytes = vec![0x5a; 72];
                    k
                },
            }],
            &audit(6),
        )
        .unwrap();
    drop(first);

    // A new process over the same data.
    let second = h.open(name);
    assert_eq!(second.groups().unwrap(), vec![group("bank"), group("pay")]);
    assert_eq!(second.group_epoch(&group("bank")).unwrap(), Some(epoch(2)));
    assert_eq!(second.group_epoch(&group("pay")).unwrap(), Some(epoch(2)));
    assert_eq!(
        second.held(&node("n1")).unwrap(),
        vec![lease("n1", "bank", 2, 777)]
    );
    assert_eq!(
        second.node(&node("n1")).unwrap().unwrap().wrap,
        Some(node_key(5))
    );
    assert_eq!(
        second.node(&node("n2")).unwrap().unwrap().status,
        NodeStatus::Revoked
    );
    assert_eq!(
        second.revocation_epoch(&node("n2"), &group("pay")).unwrap(),
        Some(epoch(1))
    );
    assert_eq!(second.events(0, 100).unwrap().len(), 9);
    assert_eq!(
        code(second.record_release(&lease("n2", "pay", 2, 9), &audit(7))),
        "protected_keys.node_revoked"
    );
    // The stored keys, unwrapped by the root, rebuild the hub of the first process.
    let stored = second.backup(&root_ref).unwrap();
    assert_eq!(stored.tenant, tenant(name));
    let mut expected = backup.groups.clone();
    expected.retain(|k| !(k.group == group("pay") && k.epoch != KeyEpoch::FIRST));
    let mut got = stored.groups.clone();
    got.retain(|k| !(k.group == group("pay") && k.epoch != KeyEpoch::FIRST));
    assert_eq!(got, expected, "the wrapped keys come back byte for byte");
    let mut restorable = stored.clone();
    restorable
        .groups
        .retain(|k| !(k.group == group("pay") && k.epoch != KeyEpoch::FIRST));
    let restored = Hub::restore(provider(), restorable).unwrap();
    assert_eq!(
        restored.group_epoch(&group("bank")),
        hub.group_epoch(&group("bank"))
    );
    assert_eq!(restored.backup().groups, expected);
}

pub fn inputs_are_bounded(h: &dyn Harness) {
    let s = h.open("t_bounds");
    let too_short = WrappedGroupKey {
        bytes: vec![1; 32],
        ..key("bank", 1, 1)
    };
    assert_eq!(
        code(s.create_group(&too_short, &audit(1))),
        "protected_keys_pg.bad_input",
        "a bare 32-byte key is not a wrapped key"
    );
    let too_long = WrappedGroupKey {
        bytes: vec![1; 4097],
        ..key("bank", 1, 1)
    };
    assert_eq!(
        code(s.create_group(&too_long, &audit(1))),
        "protected_keys_pg.bad_input"
    );
    let no_root = WrappedGroupKey {
        root: RootRef {
            id: String::new(),
            version: 1,
        },
        ..key("bank", 1, 1)
    };
    assert_eq!(
        code(s.create_group(&no_root, &audit(1))),
        "protected_keys_pg.bad_input"
    );
    let zero = WrappedGroupKey {
        epoch: epoch(0),
        ..key("bank", 1, 1)
    };
    assert_eq!(
        code(s.create_group(&zero, &audit(1))),
        "protected_keys_pg.bad_input"
    );
    let short_node = WrappedNodeKey {
        bytes: vec![1; 32],
        ..node_key(1)
    };
    assert_eq!(
        code(s.register_node(&node("n1"), &short_node, &audit(1))),
        "protected_keys_pg.bad_input"
    );
    assert!(s.groups().unwrap().is_empty() && s.node(&node("n1")).unwrap().is_none());
    assert!(Audit::new("", MonoTime::ZERO).is_err());
    assert!(Audit::new(&"a".repeat(129), MonoTime::ZERO).is_err());
    assert!(Audit::new("bad\nactor", MonoTime::ZERO).is_err());
    assert!(Audit::new("ok", MonoTime::from_millis(u64::MAX)).is_err());
    assert!(s.events(0, 1).unwrap().is_empty());
    // Nothing a refused call touched left a trace.
    assert!(s.events(0, 500).unwrap().is_empty());
    // Debug output of the wrapped types does not print their bytes.
    let shown = format!("{:?} {:?}", key("bank", 1, 0xAB), node_key(0xCD));
    assert!(
        !shown.contains("171") && !shown.contains("205") && shown.contains("[wrapped]"),
        "{shown}"
    );
    let _ = ProtectedKeyError::Tampered;
}
