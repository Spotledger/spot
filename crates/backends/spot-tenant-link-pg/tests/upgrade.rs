//! The tables carry the `spot_` prefix `spot_pg_migrate` requires, a schema that still holds the
//! old names is upgraded without losing a row, and `prepare` is safe to run again and from
//! several nodes at once.
//!
//! Every test here needs a server and skips cleanly without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{Env, env, link_id, linked, offer, runtime, tenant, tenant_id};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Collation};
use spot_tenant_link::{Direction, Error, LinkStore, MemoryPeerKeyDirectory, Trust};
use spot_tenant_link_pg::PgLinkStore;
use tokio_postgres::Client;

/// The tables as the crate created them before the prefix: the same shape, the old names.
const OLD_DDL: &str = "\
CREATE TABLE link (\
    id uuid PRIMARY KEY, \
    party_a text NOT NULL, \
    party_b text NOT NULL, \
    local text NOT NULL, \
    carrier text NOT NULL CHECK (carrier IN ('direct', 'pull', 'external')), \
    state text NOT NULL \
        CHECK (state IN ('proposed', 'accepted_by_peer', 'active', 'revoked', 'expired')), \
    created bigint NOT NULL CHECK (created >= 0), \
    expires bigint CHECK (expires IS NULL OR expires > created), \
    revoked_by text, \
    revoke_reason text, \
    revoked_at bigint, \
    version bigint NOT NULL CHECK (version >= 1), \
    registry_pending boolean NOT NULL DEFAULT true, \
    CHECK (party_a <> party_b), \
    CHECK (local = party_a OR local = party_b)); \
CREATE UNIQUE INDEX link_live_pair \
    ON link (party_a, party_b) WHERE state NOT IN ('revoked', 'expired'); \
CREATE TABLE link_peer_key (\
    link uuid NOT NULL REFERENCES link (id), \
    party text NOT NULL, \
    public_key bytea NOT NULL CHECK (octet_length(public_key) = 32), \
    PRIMARY KEY (link, party)); \
CREATE TABLE link_consent (\
    link uuid NOT NULL REFERENCES link (id), \
    ord integer NOT NULL CHECK (ord >= 0), \
    party text NOT NULL, \
    actor text NOT NULL, \
    at bigint NOT NULL CHECK (at >= 0), \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, ord), \
    UNIQUE (link, party), \
    UNIQUE (link, actor)); \
CREATE TABLE link_grant (\
    link uuid NOT NULL REFERENCES link (id), \
    ord integer NOT NULL CHECK (ord >= 0), \
    topic text NOT NULL, \
    direction text NOT NULL, \
    expires bigint NOT NULL CHECK (expires >= 0), \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, ord), \
    UNIQUE (link, topic)); \
CREATE TABLE link_offer (\
    link uuid NOT NULL REFERENCES link (id), \
    side text NOT NULL, \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, side)); \
CREATE TABLE link_audit_ref (\
    link uuid NOT NULL, \
    seq bigint GENERATED ALWAYS AS IDENTITY, \
    kind text NOT NULL, \
    actor text NOT NULL, \
    at bigint NOT NULL, \
    revision bigint NOT NULL CHECK (revision >= 0), \
    chain_seq bigint NOT NULL CHECK (chain_seq >= 0), \
    PRIMARY KEY (link, seq)); \
CREATE TABLE link_nonce (\
    link uuid NOT NULL, \
    nonce bytea NOT NULL CHECK (octet_length(nonce) = 16), \
    seen_at bigint NOT NULL CHECK (seen_at >= 0), \
    PRIMARY KEY (link, nonce));";

/// The (old, new) name of every table.
const PAIRS: [(&str, &str); 7] = [
    ("link", "spot_tenant_link"),
    ("link_peer_key", "spot_tenant_link_peer_key"),
    ("link_consent", "spot_tenant_link_consent"),
    ("link_grant", "spot_tenant_link_grant"),
    ("link_offer", "spot_tenant_link_offer"),
    ("link_audit_ref", "spot_tenant_link_audit_ref"),
    ("link_nonce", "spot_tenant_link_nonce"),
];

fn schema(n: u8) -> String {
    tenant_id(n).schema_name()
}

/// A raw client whose search path is tenant `n`'s schema.
async fn client(e: &Env, n: u8) -> Client {
    let client = e.db.raw_client().await;
    client
        .batch_execute(&format!("SET search_path TO \"{}\"", schema(n)))
        .await
        .unwrap();
    client
}

/// Every relation, index, sequence and constraint name of tenant `n`'s schema, sorted.
fn names(e: &Env, n: u8) -> Vec<String> {
    let schema = schema(n);
    e.rt.block_on(async {
        let mut out: Vec<String> =
            e.db.raw_client()
                .await
                .query(
                    "SELECT 'rel:' || c.relkind::text || ':' || c.relname::text FROM pg_class c \
                 JOIN pg_namespace s ON s.oid = c.relnamespace WHERE s.nspname = $1 \
                 UNION SELECT 'con:' || k.conname::text FROM pg_constraint k \
                 JOIN pg_namespace s ON s.oid = k.connamespace WHERE s.nspname = $1 \
                 UNION SELECT 'idx:' || ic.relname::text FROM pg_index i \
                 JOIN pg_class ic ON ic.oid = i.indexrelid \
                 JOIN pg_namespace s ON s.oid = ic.relnamespace WHERE s.nspname = $1",
                    &[&schema],
                )
                .await
                .unwrap()
                .iter()
                .map(|r| r.get(0))
                .collect();
        out.sort();
        out
    })
}

fn table_names(e: &Env, n: u8) -> Vec<String> {
    let schema = schema(n);
    e.rt.block_on(async {
        e.db.raw_client()
            .await
            .query(
                "SELECT table_name::text FROM information_schema.tables \
                 WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name::text COLLATE \"C\"",
                &[&schema],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    })
}

/// Every row of every table as text under the given table names, in a fixed order: compares the
/// stored data exactly.
fn dump(e: &Env, n: u8, new_names: bool) -> Vec<Vec<String>> {
    e.rt.block_on(async {
        let client = client(e, n).await;
        let mut all = Vec::new();
        for (old, new) in PAIRS {
            let table = if new_names { new } else { old };
            let rows = client
                .query(
                    &format!("SELECT t::text FROM {table} t ORDER BY t::text COLLATE \"C\""),
                    &[],
                )
                .await
                .unwrap();
            all.push(rows.iter().map(|r| r.get(0)).collect());
        }
        all
    })
}

/// The peer's signed acceptance of the link every test builds (deterministic, so the same bytes
/// each call).
fn accept_message() -> spot_tenant_link::HandshakeMessage {
    linked().messages[1].clone()
}

/// A source database with one finished link in every table, written through the crate's own API
/// (so the rows are real), and the link.
fn populated_source() -> (Env, spot_tenant_link::Link) {
    let rt = runtime();
    let e = rt.block_on(env(rt.clone())).expect("a server");
    let store = e.store(1);
    let link = linked().a;
    e.rt.block_on(store.insert_with_offer(link.clone(), &offer(&tenant(1), Direction::Send)))
        .unwrap();
    e.rt.block_on(store.put_offer(&link.id, &offer(&tenant(2), Direction::Receive)))
        .unwrap();
    e.rt.block_on(store.claim_inbound(
        &accept_message(),
        &MemoryPeerKeyDirectory::new(),
        &Trust::PinnedOnly,
        2_000,
    ))
    .unwrap();
    e.rt.block_on(async {
        client(&e, 1)
            .await
            .batch_execute(&format!(
                "INSERT INTO spot_tenant_link_audit_ref (link, kind, actor, at, revision, chain_seq) \
                 VALUES ('{id}', 'proposed', 'user:a', 1100, 1, 4), \
                        ('{id}', 'activated', 'user:a', 1700, 2, 5)",
                id = link.id.as_uuid()
            ))
            .await
            .unwrap();
    });
    (e, link)
}

/// Creates tenant 1's schema in `target` with the OLD names and copies every row of `source` in.
fn make_old(target: &Env, source: &Env) {
    target.provision_schema(1);
    target.rt.block_on(async {
        let dst = client(target, 1).await;
        dst.batch_execute(OLD_DDL).await.unwrap();
        let src = client(source, 1).await;
        for (old, new) in PAIRS {
            let rows = src
                .query(&format!("SELECT to_jsonb(t)::text FROM {new} t"), &[])
                .await
                .unwrap();
            for row in rows {
                let json: String = row.get(0);
                dst.execute(
                    &format!(
                        "INSERT INTO {old} OVERRIDING SYSTEM VALUE \
                         SELECT * FROM jsonb_populate_record(NULL::{old}, $1::text::jsonb)"
                    ),
                    &[&json],
                )
                .await
                .unwrap();
            }
        }
        dst.batch_execute("SELECT setval(pg_get_serial_sequence('link_audit_ref', 'seq'), 2)")
            .await
            .unwrap();
    });
}

fn fresh_store(e: &Env, n: u8) -> PgLinkStore {
    PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(n))
}

#[test]
fn every_object_the_crate_creates_is_spot_prefixed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    e.store(1);
    assert_eq!(
        table_names(&e, 1),
        PAIRS
            .iter()
            .map(|p| p.1.to_owned())
            .collect::<Vec<_>>()
            .tap_sorted()
    );
    for name in names(&e, 1) {
        let bare = name.rsplit(':').next().unwrap();
        assert!(bare.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }
}

/// The migrator refuses every table that is not `spot_` and not in the catalogue. It reads the
/// schema of its connection, so the scratch database's default search path is the tenant's.
async fn migrator_refusals(e: &Env, n: u8) -> usize {
    e.db.admin_client()
        .await
        .batch_execute(&format!(
            "ALTER DATABASE \"{}\" SET search_path TO \"{}\"",
            e.db.name(),
            schema(n)
        ))
        .await
        .unwrap();
    let backend = PgBackend::connect(e.db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    assert!(live.tables.contains_key("spot_tenant_link"), "{live:?}");
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    plan(&live, &wanted).refused.len()
}

#[test]
fn a_prepared_schema_passes_the_migrator() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    e.store(1);
    assert_eq!(e.rt.block_on(migrator_refusals(&e, 1)), 0);
}

#[test]
fn an_upgraded_schema_passes_the_migrator() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    old.rt.block_on(fresh_store(&old, 1).prepare()).unwrap();
    assert_eq!(old.rt.block_on(migrator_refusals(&old, 1)), 0);
}

trait TapSorted {
    fn tap_sorted(self) -> Self;
}

impl TapSorted for Vec<String> {
    fn tap_sorted(mut self) -> Self {
        self.sort();
        self
    }
}

#[test]
fn old_named_tables_are_renamed_with_every_row_kept() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, link) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    let before = dump(&old, 1, false);
    assert!(before.iter().all(|rows| !rows.is_empty()), "{before:?}");
    assert_eq!(table_names(&old, 1).len(), 7);

    let store = fresh_store(&old, 1);
    old.rt.block_on(store.prepare()).unwrap();

    // Byte for byte the same rows under the new names, the old names gone.
    assert_eq!(dump(&old, 1, true), before);
    let mut expected: Vec<String> = PAIRS.iter().map(|p| p.1.to_owned()).collect();
    expected.sort();
    assert_eq!(table_names(&old, 1), expected);
    // The crate's own API reads them, and the replay guard and the references survived.
    let reader = fresh_store(&source, 1);
    assert_eq!(store.get(&link.id).unwrap(), reader.get(&link.id).unwrap());
    let (mine, theirs) = old.rt.block_on(store.offers(&link.id)).unwrap();
    assert!(mine.is_some() && theirs.is_some());
    assert_eq!(old.rt.block_on(store.nonce_count(&link.id)).unwrap(), 1);
    let replay = old.rt.block_on(store.claim_inbound(
        &accept_message(),
        &MemoryPeerKeyDirectory::new(),
        &Trust::PinnedOnly,
        3_000,
    ));
    assert!(matches!(replay, Err(Error::Replayed { .. })), "{replay:?}");
    // The identity sequence kept its position under its new name.
    let next: i64 = old.rt.block_on(async {
        client(&old, 1)
            .await
            .query_one(
                "INSERT INTO spot_tenant_link_audit_ref (link, kind, actor, at, revision, chain_seq) \
                 SELECT id, 'revoked', 'user:a', 1800, 3, 6 FROM spot_tenant_link LIMIT 1 \
                 RETURNING seq",
                &[],
            )
            .await
            .unwrap()
            .get(0)
    });
    assert_eq!(next, 3);
    // One live link per pair is still enforced by the renamed unique index.
    let mut rival = link.clone();
    rival.id = link_id(9);
    let refused = store.insert(rival);
    assert!(
        matches!(refused, Err(Error::DuplicateLink { .. })),
        "{refused:?}"
    );
}

#[test]
fn an_upgraded_schema_has_the_catalogue_of_a_fresh_one() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    old.rt.block_on(fresh_store(&old, 1).prepare()).unwrap();
    let rt = runtime();
    let fresh = rt.block_on(env(rt.clone())).unwrap();
    fresh.store(1);

    let upgraded = names(&old, 1);
    assert_eq!(upgraded, names(&fresh, 1));
    for expected in [
        "idx:spot_tenant_link_live_pair",
        "idx:spot_tenant_link_pkey",
        "idx:spot_tenant_link_peer_key_pkey",
        "idx:spot_tenant_link_consent_link_actor_key",
        "idx:spot_tenant_link_grant_link_topic_key",
        "con:spot_tenant_link_pkey",
        "con:spot_tenant_link_peer_key_link_fkey",
        "con:spot_tenant_link_audit_ref_chain_seq_check",
        "con:spot_tenant_link_nonce_nonce_check",
        "rel:S:spot_tenant_link_audit_ref_seq_seq",
    ] {
        assert!(upgraded.iter().any(|n| n == expected), "missing {expected}");
    }
    assert!(
        !upgraded
            .iter()
            .any(|n| n.rsplit(':').next().unwrap().starts_with("link")),
        "an old name is left: {upgraded:?}"
    );
}

#[test]
fn preparing_again_changes_nothing() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    let store = fresh_store(&old, 1);
    old.rt.block_on(store.prepare()).unwrap();
    let before_names = names(&old, 1);
    let before_rows = dump(&old, 1, true);
    for _ in 0..3 {
        old.rt.block_on(store.prepare()).unwrap();
    }
    assert_eq!(names(&old, 1), before_names);
    assert_eq!(dump(&old, 1, true), before_rows);
}

#[test]
fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    let before = dump(&old, 1, false);
    let fresh = {
        let rt = runtime();
        let fresh = rt.block_on(env(rt.clone())).unwrap();
        fresh.store(1);
        fresh
    };
    old.rt.block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let store = fresh_store(&old, 1);
            tasks.push(tokio::spawn(async move { store.prepare().await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
    });
    assert_eq!(dump(&old, 1, true), before);
    assert_eq!(names(&old, 1), names(&fresh, 1));
}

#[test]
fn both_names_present_is_refused_and_nothing_is_touched() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    old.rt.block_on(async {
        client(&old, 1)
            .await
            .batch_execute(
                "CREATE TABLE spot_tenant_link_grant (LIKE link_grant INCLUDING ALL); \
                 INSERT INTO spot_tenant_link_grant SELECT * FROM link_grant",
            )
            .await
            .unwrap();
    });
    let names_before = names(&old, 1);
    let old_rows = dump(&old, 1, false);

    let error = old
        .rt
        .block_on(fresh_store(&old, 1).prepare())
        .expect_err("no merge by guess");
    let text = error.to_string();
    assert!(
        text.contains("`link_grant`") && text.contains("`spot_tenant_link_grant`"),
        "{text}"
    );
    assert_eq!(names(&old, 1), names_before);
    assert_eq!(dump(&old, 1, false), old_rows);
}

#[test]
fn a_foreign_table_with_an_old_name_is_not_renamed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    e.provision_schema(1);
    e.rt.block_on(async {
        client(&e, 1)
            .await
            .batch_execute(
                "CREATE TABLE link (id text PRIMARY KEY, secret text); \
                 INSERT INTO link VALUES ('x', 'do-not-print')",
            )
            .await
            .unwrap();
    });
    let error =
        e.rt.block_on(fresh_store(&e, 1).prepare())
            .expect_err("a foreign table is left alone");
    let text = error.to_string();
    assert!(text.contains("`link`"), "{text}");
    assert!(!text.contains("do-not-print"), "{text}");
    // The refusal rolled everything back: no new table was created either.
    assert_eq!(table_names(&e, 1), vec!["link".to_owned()]);
}

#[test]
fn a_taken_target_name_is_refused_and_nothing_is_renamed() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    old.rt.block_on(async {
        client(&old, 1)
            .await
            .batch_execute("CREATE SEQUENCE spot_tenant_link_live_pair")
            .await
            .unwrap();
    });
    let names_before = names(&old, 1);
    let error = old
        .rt
        .block_on(fresh_store(&old, 1).prepare())
        .expect_err("a taken name is never reused");
    assert!(
        error.to_string().contains("`spot_tenant_link_live_pair`"),
        "{error}"
    );
    assert_eq!(names(&old, 1), names_before);
}

#[test]
fn an_old_table_of_another_schema_is_never_touched() {
    let rt = runtime();
    if rt.block_on(env(rt.clone())).is_none() {
        return;
    }
    let (source, _) = populated_source();
    let rt = runtime();
    let old = rt.block_on(env(rt.clone())).unwrap();
    make_old(&old, &source);
    // Another tenant's schema, prepared on its own: tenant 1's old tables stay as they are.
    let other = fresh_store(&old, 2);
    old.provision_schema(2);
    old.rt.block_on(other.prepare()).unwrap();
    assert_eq!(table_names(&old, 2).len(), 7);
    let mut still_old: Vec<String> = PAIRS.iter().map(|p| p.0.to_owned()).collect();
    still_old.sort();
    assert_eq!(table_names(&old, 1), still_old);
}
