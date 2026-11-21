//! The schema and the security properties: the migrator accepts the tables, no plaintext key
//! reaches the database, the history and the keys are immutable (by trigger and by grant),
//! statements take input as data, a backend failure or a corrupt row is an error (never an empty
//! result), and errors carry no content.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_protected_keys::store::{BAD_INPUT, CORRUPT, EPOCH_CONFLICT, STORE_FAILED};
use spot_protected_keys::{
    Audit, GroupName, KeyEpoch, KeyEventKind, KeyStore, KeyStoreError, LockedKey,
    MemoryRootKeyProvider, MonoTime, NodeId, NodeWrapKey, ReleaseRecord, RootKeyProvider, RootRef,
    TenantId, WrappedGroupKey, WrappedNodeKey,
};
use spot_protected_keys_pg::{BAD_SETUP, MIGRATION_FAILED, PgKeyStore, sql};
use spot_storage_contract::{Catalog, Collation};

/// A known test key: the dump test looks for these bytes.
const KNOWN_GROUP_KEY: &[u8; 32] = b"KNOWN-GROUP-KEY-0123456789abcdef";
const KNOWN_NODE_KEY: &[u8; 32] = b"KNOWN-NODE--KEY-0123456789abcdef";

fn audit(ms: u64) -> Audit {
    Audit::new("hub-admin", MonoTime::from_millis(ms)).unwrap()
}
fn group(name: &str) -> GroupName {
    GroupName::new(name).unwrap()
}
fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}
fn wrapped(name: &str, epoch: u64, fill: u8) -> WrappedGroupKey {
    WrappedGroupKey {
        group: group(name),
        epoch: KeyEpoch::new(epoch),
        root: RootRef {
            id: "memory-root".to_owned(),
            version: 1,
        },
        bytes: vec![fill; 72],
    }
}
fn node_key(fill: u8) -> WrappedNodeKey {
    WrappedNodeKey {
        root: RootRef {
            id: "memory-root".to_owned(),
            version: 1,
        },
        bytes: vec![fill; 72],
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n =
            chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

#[test]
fn codes_are_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    for code in [
        BAD_SETUP,
        MIGRATION_FAILED,
        EPOCH_CONFLICT,
        BAD_INPUT,
        STORE_FAILED,
        CORRUPT,
    ] {
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
}

#[test]
fn every_statement_is_fixed_text_scoped_by_the_tenant_parameter() {
    let tenant_scoped = [
        sql::INSERT_GROUP,
        sql::INSERT_EPOCH,
        sql::BUMP_HEAD,
        sql::HEAD,
        sql::GROUPS,
        sql::GROUP_KEY,
        sql::RETIRE,
        sql::BACKUP,
        sql::UPSERT_NODE,
        sql::NODE,
        sql::SET_STATUS,
        sql::RECORD_RELEASE,
        sql::HOLDS,
        sql::REVOKE_NODE,
        sql::HOLD_GROUPS,
        sql::DELETE_HOLDS,
        sql::INSERT_REVOCATION,
        sql::REVOCATION_EPOCH,
        sql::INSERT_EVENT,
        sql::EVENTS,
    ];
    for statement in tenant_scoped {
        assert!(
            statement.contains("tenant = $1") || statement.contains("VALUES ($1"),
            "a statement is not scoped by $1: {statement}"
        );
    }
    // The statements that read or change existing rows filter on the tenant, not just insert it.
    for statement in [
        sql::BUMP_HEAD,
        sql::HEAD,
        sql::GROUP_KEY,
        sql::RETIRE,
        sql::NODE,
        sql::SET_STATUS,
        sql::RECORD_RELEASE,
        sql::HOLDS,
        sql::REVOKE_NODE,
        sql::DELETE_HOLDS,
        sql::EVENTS,
    ] {
        assert!(statement.contains("tenant = $1"), "{statement}");
    }
}

#[test]
fn the_store_refuses_an_unusable_tenant() {
    let Some(fx) = fixture() else { return };
    let (bridge, pool) = open_bridge(fx.db.config());
    for bad in ["", &"x".repeat(129), "bad\ntenant"] {
        let err = PgKeyStore::new(std::sync::Arc::clone(&bridge), &pool, bad).unwrap_err();
        assert_eq!(err.code(), BAD_SETUP);
    }
    assert!(PgKeyStore::new(bridge, &pool, "acme").is_ok());
}

#[test]
fn prepare_is_idempotent_and_the_migrator_accepts_the_tables() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store.prepare().unwrap();
    store.prepare().unwrap();
    let refused = fx.rt.block_on(async {
        let backend = PgBackend::connect(fx.db.config()).await.unwrap();
        let live = LiveSchema::read(&backend).await.unwrap();
        let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
        plan(&live, &wanted).refused.len()
    });
    assert_eq!(
        refused, 0,
        "the migrator refuses nothing: every table carries the spot_ prefix"
    );
}

#[test]
fn no_plaintext_key_byte_reaches_the_database() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    let tenant = TenantId::new("acme").unwrap();
    let provider = MemoryRootKeyProvider::from_key(
        tenant.clone(),
        &LockedKey::from_bytes(b"KNOWN-ROOT-KEY--0123456789abcdef").unwrap(),
    )
    .unwrap();

    // A group key with known plaintext, wrapped by the root exactly as the hub wraps it.
    let sealed = provider
        .wrap(&LockedKey::from_bytes(KNOWN_GROUP_KEY).unwrap(), b"aad")
        .unwrap();
    let key = WrappedGroupKey {
        group: group("bank"),
        epoch: KeyEpoch::FIRST,
        root: RootRef {
            id: sealed.key.id.clone(),
            version: sealed.key.version,
        },
        bytes: sealed.bytes.clone(),
    };
    store.create_group(&key, &audit(1)).unwrap();
    // A node wrapping key with known plaintext, wrapped under the root by the crate.
    let n = node("branch-1");
    let node_wrap =
        NodeWrapKey::from_key(n.clone(), LockedKey::from_bytes(KNOWN_NODE_KEY).unwrap());
    let stored_node = node_wrap.wrap_under_root(&tenant, &provider).unwrap();
    store.register_node(&n, &stored_node, &audit(2)).unwrap();
    store
        .record_release(
            &ReleaseRecord {
                node: n.clone(),
                group: group("bank"),
                epoch: KeyEpoch::FIRST,
                lease_until: MonoTime::from_millis(100),
            },
            &audit(3),
        )
        .unwrap();
    // The stored node key opens again under the root.
    let read = store.node(&n).unwrap().unwrap().wrap.unwrap();
    NodeWrapKey::unwrap_from_root(&n, &tenant, &read, &provider).unwrap();
    // ... and not for another node or another tenant.
    assert!(NodeWrapKey::unwrap_from_root(&node("branch-2"), &tenant, &read, &provider).is_err());

    let dump: String = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        let tables: Vec<String> = client
            .query(
                "SELECT table_name::text FROM information_schema.tables \
                 WHERE table_name LIKE 'spot_key_%' AND table_type = 'BASE TABLE' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert!(tables.len() >= 6, "{tables:?}");
        let mut dump = String::new();
        for table in tables {
            // The row as text: bytea comes out as hex, every other column as written.
            let rows = client
                .query(&format!("SELECT t::text FROM {table} t"), &[])
                .await
                .unwrap();
            for row in rows {
                let line: String = row.get(0);
                dump.push_str(&line);
                dump.push('\n');
            }
        }
        dump
    });
    // The positive control: the dump does show the wrapped bytes.
    assert!(
        dump.contains(&hex(&sealed.bytes)),
        "the dump must show the wrapped group key"
    );
    assert!(dump.contains(&hex(&stored_node.bytes)));
    for (name, plain) in [("group", KNOWN_GROUP_KEY), ("node", KNOWN_NODE_KEY)] {
        let lower = dump.to_lowercase();
        assert!(!lower.contains(&hex(plain)), "{name} key in hex");
        assert!(!dump.contains(&base64(plain)), "{name} key in base64");
        assert!(
            !dump.contains(std::str::from_utf8(plain).unwrap()),
            "{name} key as text"
        );
    }
    // The wrapped form does not contain the plaintext either (a wrap that stored the key as
    // is would fail here).
    for (wrapped, plain) in [
        (&sealed.bytes, KNOWN_GROUP_KEY.as_slice()),
        (&stored_node.bytes, KNOWN_NODE_KEY.as_slice()),
    ] {
        assert!(!wrapped.windows(plain.len()).any(|w| w == plain));
    }

    // A bare key cannot be put in a key column even by hand.
    let refused = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .execute(
                "INSERT INTO spot_key_group_epoch (tenant, group_name, epoch, root_id, root_version, wrapped) \
                 VALUES ('acme', 'bank', 2, 'r', 1, $1)",
                &[&KNOWN_GROUP_KEY.to_vec()],
            )
            .await
            .unwrap_err()
    });
    assert_eq!(refused.as_db_error().unwrap().code().code(), "23514");
    // And the API refuses it before the statement runs.
    let bare = WrappedGroupKey {
        bytes: KNOWN_GROUP_KEY.to_vec(),
        ..wrapped("pay", 1, 0)
    };
    assert_eq!(
        store.create_group(&bare, &audit(4)).unwrap_err().code(),
        BAD_INPUT
    );
}

#[test]
fn every_row_carries_the_stores_tenant() {
    let Some(fx) = fixture() else { return };
    let (bridge, pool, a) = open_store(fx.db.config(), "tenant_a");
    let b = PgKeyStore::new(bridge, &pool, "tenant_b").unwrap();
    for (store, fill) in [(&*a, 1u8), (&b, 2u8)] {
        store
            .create_group(&wrapped("bank", 1, fill), &audit(1))
            .unwrap();
        store
            .register_node(&node("n1"), &node_key(fill), &audit(1))
            .unwrap();
        store
            .record_release(
                &ReleaseRecord {
                    node: node("n1"),
                    group: group("bank"),
                    epoch: KeyEpoch::FIRST,
                    lease_until: MonoTime::from_millis(9),
                },
                &audit(2),
            )
            .unwrap();
    }
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        for table in [
            "spot_key_group",
            "spot_key_group_epoch",
            "spot_key_node",
            "spot_key_hold",
            "spot_key_event",
        ] {
            let rows = client
                .query(
                    &format!("SELECT tenant, count(*) FROM {table} GROUP BY tenant ORDER BY 1"),
                    &[],
                )
                .await
                .unwrap();
            let seen: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
            assert_eq!(seen.len(), 2, "{table}: {seen:?}");
            assert_eq!(
                seen[0].1, seen[1].1,
                "{table}: each tenant wrote its own rows"
            );
        }
    });
    assert_eq!(
        a.group_key(&group("bank"), KeyEpoch::FIRST).unwrap().bytes[0],
        1
    );
    assert_eq!(
        b.group_key(&group("bank"), KeyEpoch::FIRST).unwrap().bytes[0],
        2
    );
}

#[test]
fn the_history_and_the_keys_are_immutable() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .create_group(&wrapped("bank", 1, 1), &audit(1))
        .unwrap();
    store
        .rotate_group(KeyEpoch::FIRST, &wrapped("bank", 2, 2), &audit(2))
        .unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        for statement in [
            "UPDATE spot_key_event SET actor = 'mallory'",
            "DELETE FROM spot_key_event",
            "TRUNCATE spot_key_event",
            "DELETE FROM spot_key_group_epoch",
            "TRUNCATE spot_key_group_epoch CASCADE",
            "UPDATE spot_key_group_epoch SET root_id = 'other'",
            "UPDATE spot_key_group_epoch SET wrapped = '\\x00' || wrapped WHERE epoch = 1",
            "UPDATE spot_key_group_epoch SET wrapped = NULL, retired_at_ms = 1, epoch = 9 WHERE epoch = 1",
        ] {
            let err = client.execute(statement, &[]).await.unwrap_err();
            let db = err.as_db_error().unwrap_or_else(|| panic!("{statement}: {err:?}"));
            assert!(
                db.message().contains("append-only"),
                "{statement} must be refused by the guard, got {db:?}"
            );
        }
        let events: i64 = client
            .query_one("SELECT count(*) FROM spot_key_event", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(events, 2);
    });
    // The one allowed edit, retirement, goes through the API.
    store
        .retire_epoch(&group("bank"), KeyEpoch::FIRST, &audit(3))
        .unwrap();
    assert!(
        store
            .events(0, 10)
            .unwrap()
            .iter()
            .any(|e| e.kind == KeyEventKind::Retire)
    );
}

#[test]
fn the_application_role_cannot_update_or_delete_history() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .create_group(&wrapped("bank", 1, 1), &audit(1))
        .unwrap();
    let role = format!("spot_key_app_{}", std::process::id());
    assert_eq!(
        store.restrict_role("").unwrap_err().code(),
        BAD_INPUT,
        "an unusable role name is refused before a statement runs"
    );
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
            .await
            .ok();
        client
            .batch_execute(&format!("CREATE ROLE {role} NOLOGIN"))
            .await
            .unwrap();
    });
    store.restrict_role(&role).unwrap();
    store.restrict_role(&role).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client.batch_execute(&format!("SET ROLE {role}")).await.unwrap();
        let denied = |statement: &'static str| {
            let client = &client;
            async move {
                let err = client.execute(statement, &[]).await.unwrap_err();
                assert_eq!(
                    err.as_db_error().map(|d| d.code().code().to_owned()).as_deref(),
                    Some("42501"),
                    "{statement} must be a permission error"
                );
            }
        };
        denied("UPDATE spot_key_event SET actor = 'mallory'").await;
        denied("DELETE FROM spot_key_event").await;
        denied("TRUNCATE spot_key_event").await;
        denied("DELETE FROM spot_key_group_epoch").await;
        denied("TRUNCATE spot_key_group_epoch").await;
        denied("DELETE FROM spot_key_revocation").await;
        // What the store needs still works.
        client.query("SELECT * FROM spot_key_event", &[]).await.unwrap();
        client
            .execute(
                "INSERT INTO spot_key_event (tenant, kind, actor, at_ms) VALUES ('acme', 'issue', 'a', 1)",
                &[],
            )
            .await
            .unwrap();
        client.batch_execute("RESET ROLE").await.unwrap();
        client
            .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role}"))
            .await
            .unwrap();
    });
}

#[test]
fn a_hostile_name_is_data_not_sql() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    let hostile = "x'; DROP TABLE spot_key_group; --";
    store
        .create_group(&wrapped(hostile, 1, 1), &audit(1))
        .unwrap();
    assert_eq!(store.groups().unwrap(), vec![group(hostile)]);
    assert_eq!(
        store.group_epoch(&group(hostile)).unwrap(),
        Some(KeyEpoch::FIRST)
    );
    store
        .register_node(&node(hostile), &node_key(1), &audit(2))
        .unwrap();
    assert!(store.node(&node(hostile)).unwrap().is_some());
    // The same for a tenant name.
    let (bridge, pool) = open_bridge(fx.db.config());
    let odd = PgKeyStore::new(bridge, &pool, "t'; DROP TABLE spot_key_node; --").unwrap();
    odd.create_group(&wrapped("bank", 1, 1), &audit(3)).unwrap();
    assert_eq!(odd.groups().unwrap(), vec![group("bank")]);
    assert!(
        store.node(&node(hostile)).unwrap().is_some(),
        "the table is still there"
    );
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_and_nothing_runs() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    let refused = fx
        .rt
        .block_on(async { store.create_group(&wrapped("bank", 1, 1), &audit(1)) });
    let err = refused.unwrap_err();
    assert_eq!(err.code(), STORE_FAILED);
    assert!(err.to_string().contains("pg_bridge.wrong_thread"), "{err}");
    assert!(store.groups().unwrap().is_empty(), "nothing ran");
}

#[test]
fn a_backend_failure_is_an_error_and_says_nothing_about_the_data() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .create_group(&wrapped("super_secret_group", 1, 1), &audit(1))
        .unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute("DROP TABLE spot_key_hold")
            .await
            .unwrap();
    });
    store
        .register_node(&node("branch-1"), &node_key(1), &audit(2))
        .unwrap();
    let err = store
        .record_release(
            &ReleaseRecord {
                node: node("branch-1"),
                group: group("super_secret_group"),
                epoch: KeyEpoch::FIRST,
                lease_until: MonoTime::from_millis(5),
            },
            &audit(3),
        )
        .unwrap_err();
    assert!(matches!(err, KeyStoreError::Failed(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("sqlstate class 42"), "{text}");
    for leak in ["super_secret_group", "branch-1", "spot_key_hold", "acme"] {
        assert!(!text.contains(leak), "the message leaks {leak}: {text}");
    }
    // The failed release left no event.
    assert!(
        store
            .events(0, 100)
            .unwrap()
            .iter()
            .all(|e| e.kind != KeyEventKind::Release)
    );
}

#[test]
fn a_corrupt_row_is_an_error_never_skipped() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .create_group(&wrapped("bank", 1, 1), &audit(1))
        .unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .execute(
                "INSERT INTO spot_key_group (tenant, group_name, current_epoch) VALUES ('acme', $1, 1)",
                &[&"bad\u{1}name"],
            )
            .await
            .unwrap();
    });
    assert_eq!(store.groups().unwrap_err().code(), CORRUPT);
}

#[test]
fn a_revoked_nodes_wrapped_key_is_erased_in_the_row() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .register_node(&node("n1"), &node_key(7), &audit(1))
        .unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        let present: bool = client
            .query_one("SELECT wrapped IS NOT NULL FROM spot_key_node", &[])
            .await
            .unwrap()
            .get(0);
        assert!(present);
    });
    store.revoke_node(&node("n1"), &[], &audit(2)).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        let row = client
            .query_one(
                "SELECT wrapped IS NULL, root_id IS NULL, status FROM spot_key_node",
                &[],
            )
            .await
            .unwrap();
        assert!(row.get::<_, bool>(0) && row.get::<_, bool>(1));
        assert_eq!(row.get::<_, String>(2), "revoked");
    });
}

#[test]
fn a_timed_out_database_is_an_error() {
    // The bridge deadline cancels the statement; here a lock held by a raw session makes the
    // write wait past the deadline.
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    store
        .create_group(&wrapped("bank", 1, 1), &audit(1))
        .unwrap();
    let locker = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute("BEGIN; LOCK TABLE spot_key_group IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        client
    });
    let err = store.group_epoch(&group("bank")).unwrap_err();
    assert_eq!(err.code(), STORE_FAILED);
    assert!(err.to_string().contains("pg_bridge.timeout"), "{err}");
    fx.rt
        .block_on(async { locker.batch_execute("ROLLBACK").await.unwrap() });
    assert_eq!(
        store.group_epoch(&group("bank")).unwrap(),
        Some(KeyEpoch::FIRST)
    );
}

#[test]
fn the_root_ref_round_trips_through_the_backup() {
    let Some(fx) = fixture() else { return };
    let (_bridge, _pool, store) = open_store(fx.db.config(), "acme");
    let key = WrappedGroupKey {
        root: RootRef {
            id: "kms/key-1".to_owned(),
            version: 4_000_000_000,
        },
        ..wrapped("bank", 1, 3)
    };
    store.create_group(&key, &audit(1)).unwrap();
    let backup = store.backup(&key.root).unwrap();
    assert_eq!(backup.groups, vec![key]);
}
