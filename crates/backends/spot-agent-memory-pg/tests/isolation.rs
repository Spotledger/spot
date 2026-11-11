//! The isolation tests: a tenant's memory lives in a database of its own, and neither side of the
//! wall can reach the other.
//!
//! Each case connects as the role in question and tries — the assertions are made by PostgreSQL,
//! not by this crate's own guard clauses.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;

use common::{Business, Provisioned, ephemeral_tenant, runtime, tenant};
use spot_agent_memory::testing::episode_saying;
use spot_agent_memory::{Entry, MemoryStore};
use spot_agent_memory_pg::{PgMemoryProvisioner, PgMemoryStore, pool_for};
use spot_postgres::testing::TestDb;

/// The password the control plane hands the memory role in these tests.
const MEMORY_SECRET: &str = "memory-role-secret-0001";

fn create_db() -> Option<(TestDb, Arc<tokio::runtime::Runtime>)> {
    let rt = runtime();
    let db = rt.block_on(TestDb::create())?;
    Some((db, rt))
}

#[test]
fn the_memory_role_cannot_read_a_business_table_even_when_it_connects() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let business = Business::create(&rt, &admin, &db);

    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");
    let memory = PgMemoryStore::connect(
        &placement.config(&admin, MEMORY_SECRET),
        owner.clone(),
        Arc::clone(&rt),
    )
    .expect("the memory store builds");
    memory
        .prepare()
        .expect("the memory role may create the tables it owns");
    memory
        .put(Entry::Episode(episode_saying(
            owner.as_str(),
            1,
            "the job stayed stuck",
            1_000,
        )))
        .expect("the memory role may write its own memory");
    assert_eq!(
        memory
            .count(spot_agent_memory::MemoryKind::Episode)
            .unwrap(),
        1
    );

    // The memory database holds no business table: the name is not even visible from it.
    let client = rt.block_on(async {
        let pool = pool_for(&placement.config(&admin, MEMORY_SECRET)).expect("a pool");
        pool.get().await.expect("the memory connection")
    });
    let visible: i64 = rt.block_on(async {
        client
            .query_one(
                "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
                &[&Business::TABLE],
            )
            .await
            .expect("the catalogue is readable")
            .get(0)
    });
    assert_eq!(
        visible, 0,
        "the business table is not in the memory database"
    );

    // Connecting to the business database as the memory role, the table is refused.
    let intruder = placement
        .config(&admin, MEMORY_SECRET)
        .with_dbname(&business.database);
    let pool = pool_for(&intruder).expect("a pool");
    let client = rt
        .block_on(pool.get())
        .expect("the business database still lets the role connect");
    let refused = rt.block_on(client.query("SELECT * FROM business_job", &[]));
    let error = refused.expect_err("the memory role may not read a business table");
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("42501"),
        "permission denied: {error}"
    );

    business.drop_all(&rt, &db);
    drop(provisioned);
    drop(memory);
}

#[test]
fn the_business_role_cannot_connect_to_the_memory_database() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let business = Business::create(&rt, &admin, &db);
    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");

    let mut config = business.config(&admin);
    config.dbname.clone_from(&placement.database);
    let pool = pool_for(&config).expect("a pool is built without connecting");
    let refused = rt.block_on(pool.get());
    let error = refused.expect_err("the business role may not connect");
    // `CONNECT` was revoked from `PUBLIC` on the memory database, so even the name of the
    // database is not enough for a role that was never granted it.
    assert!(
        format!("{error:?}").contains("permission denied")
            || format!("{error:?}").contains("no pg_hba")
            || format!("{error:?}").contains("Connection refused"),
        "the business role is refused: {error:?}"
    );

    business.drop_all(&rt, &db);
    drop(provisioned);
}

#[test]
fn the_memory_database_is_the_only_place_the_vector_extension_and_the_memory_tables_live() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let business = Business::create(&rt, &admin, &db);
    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");
    let memory = PgMemoryStore::connect(
        &placement.config(&admin, MEMORY_SECRET),
        owner.clone(),
        Arc::clone(&rt),
    )
    .expect("the memory store builds");
    memory.prepare().expect("the memory database is prepared");

    // The business database has no `vector` extension and no memory table: memory never lives
    // beside business data, not even as an empty schema.
    let pool = pool_for(&business.config(&admin)).expect("a pool");
    let client = rt.block_on(pool.get()).expect("the business connection");
    let vector: i64 = rt.block_on(async {
        client
            .query_one(
                "SELECT count(*) FROM pg_extension WHERE extname = 'vector'",
                &[],
            )
            .await
            .expect("the extensions are readable")
            .get(0)
    });
    assert_eq!(vector, 0, "the business database has no pgvector");
    let memory_tables: i64 = rt.block_on(async {
        client
            .query_one(
                "SELECT count(*) FROM information_schema.tables \
                 WHERE table_name LIKE 'spot\\_memory\\_%'",
                &[],
            )
            .await
            .expect("the catalogue is readable")
            .get(0)
    });
    assert_eq!(
        memory_tables, 0,
        "no memory table is in the business database"
    );

    business.drop_all(&rt, &db);
    drop(memory);
    drop(provisioned);
}

#[test]
fn the_memory_location_is_recorded_in_the_control_store_and_nowhere_in_the_business_schema() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let business = Business::create(&rt, &admin, &db);
    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");
    assert_eq!(placement.tenant, owner);
    assert!(placement.database.starts_with("spot_memory_db_"));
    assert!(placement.role.starts_with("spot_memory_role_"));

    // Provisioning is idempotent: a second run keeps the names and the creation instant.
    provisioner
        .provision(&owner, MEMORY_SECRET, 9_000)
        .expect("provisioning is idempotent");
    let again = provisioner
        .placement(&owner)
        .expect("the placement is read")
        .expect("the placement exists");
    assert_eq!(again.database, placement.database);
    assert_eq!(again.role, placement.role);
    let control = common::control_client(&rt, &db);
    let created: i64 = rt.block_on(async {
        control
            .query_one(
                "SELECT created_ms FROM spot_memory_database WHERE tenant = $1",
                &[&owner.as_str()],
            )
            .await
            .expect("the record is readable")
            .get(0)
    });
    assert_eq!(created, 1_000, "the original creation instant is kept");

    // The connection string (and its password) appears nowhere in the control store's record and
    // nowhere in the business database's own catalogue.
    let stored: (String, String) = rt.block_on(async {
        let row = control
            .query_one(
                "SELECT database, role FROM spot_memory_database WHERE tenant = $1",
                &[&owner.as_str()],
            )
            .await
            .expect("the record is readable");
        (row.get(0), row.get(1))
    });
    assert_eq!(stored, (placement.database.clone(), placement.role.clone()));
    assert!(!stored.0.contains(MEMORY_SECRET));
    assert!(!stored.1.contains(MEMORY_SECRET));

    let pool = pool_for(&business.config(&admin)).expect("a pool");
    let client = rt.block_on(pool.get()).expect("the business connection");
    let mentions: i64 = rt.block_on(async {
        client
            .query_one(
                "SELECT (SELECT count(*) FROM pg_class c JOIN pg_namespace n \
                     ON n.oid = c.relnamespace \
                  WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
                    AND (c.relname LIKE '%memory%' OR c.relname LIKE '%agent_memory%')) \
                 + (SELECT count(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
                    JOIN pg_namespace n ON n.oid = c.relnamespace \
                    WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') \
                      AND a.attname LIKE '%memory%')",
                &[],
            )
            .await
            .expect("the catalogue is readable")
            .get(0)
    });
    assert_eq!(mentions, 0, "the business schema names no memory database");

    // Nothing this crate hands out in a `Debug` line carries the password.
    let memory = PgMemoryStore::connect(
        &placement.config(&admin, MEMORY_SECRET),
        owner.clone(),
        Arc::clone(&rt),
    )
    .expect("the memory store builds");
    for rendered in [
        format!("{memory:?}"),
        format!("{placement:?}"),
        format!("{provisioner:?}"),
    ] {
        assert!(!rendered.contains(MEMORY_SECRET), "{rendered}");
        assert!(!rendered.contains("postgres://"), "{rendered}");
    }

    business.drop_all(&rt, &db);
    drop(memory);
    drop(provisioned);
}

#[test]
fn deleting_a_tenant_removes_its_memory_database_its_role_and_its_record() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");
    let memory = PgMemoryStore::connect(
        &placement.config(&admin, MEMORY_SECRET),
        owner.clone(),
        Arc::clone(&rt),
    )
    .expect("the memory store builds");
    memory.prepare().expect("the memory database is prepared");
    memory
        .put(Entry::Episode(episode_saying(
            owner.as_str(),
            1,
            "stuck job",
            1_000,
        )))
        .expect("the write");
    let exported = memory.export().expect("the export survives the deletion");
    assert_eq!(exported.len(), 1);
    drop(memory);

    assert!(
        provisioner
            .deprovision(&owner)
            .expect("the deprovision succeeds")
    );
    let admin_client = rt.block_on(async { db.admin_client().await });
    let control = common::control_client(&rt, &db);
    let (databases, roles, records): (i64, i64, i64) = rt.block_on(async {
        let databases = admin_client
            .query_one(
                "SELECT count(*) FROM pg_database WHERE datname = $1",
                &[&placement.database],
            )
            .await
            .expect("pg_database is readable")
            .get(0);
        let roles = admin_client
            .query_one(
                "SELECT count(*) FROM pg_roles WHERE rolname = $1",
                &[&placement.role],
            )
            .await
            .expect("pg_roles is readable")
            .get(0);
        let records = control
            .query_one(
                "SELECT count(*) FROM spot_memory_database WHERE tenant = $1",
                &[&owner.as_str()],
            )
            .await
            .expect("the record table is readable")
            .get(0);
        (databases, roles, records)
    });
    assert_eq!(databases, 0, "the memory database is gone");
    assert_eq!(roles, 0, "the memory role is gone");
    assert_eq!(records, 0, "the record is gone");
    assert!(
        !provisioner
            .deprovision(&owner)
            .expect("a second deprovision is harmless"),
        "there is nothing left to remove"
    );
    drop(provisioned);
}

#[test]
fn the_memory_role_is_not_a_superuser_and_cannot_create_databases() {
    let Some((db, rt)) = create_db() else {
        return;
    };
    let admin = db.config();
    let owner = ephemeral_tenant(&db);
    let provisioner = Arc::new(PgMemoryProvisioner::new(
        admin.clone(),
        "und",
        Arc::clone(&rt),
    ));
    let provisioned = Provisioned::new(Arc::clone(&provisioner), owner.clone(), MEMORY_SECRET);
    let placement = provisioner
        .placement(&owner)
        .expect("the placement is recorded")
        .expect("the placement exists");

    let admin_client = rt.block_on(async { db.admin_client().await });
    // A role name has to be quoted here: it is data in the query, not an identifier this test
    // derived, so it is bound rather than interpolated.
    assert!(
        placement
            .role
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "the role name is a plain identifier"
    );
    let row = rt.block_on(async {
        admin_client
            .query_one(
                "SELECT rolsuper, rolcreatedb, rolcreaterole, rolbypassrls FROM pg_roles \
                 WHERE rolname = $1",
                &[&placement.role],
            )
            .await
            .expect("pg_roles is readable")
    });
    let (superuser, createdb, createrole, bypassrls): (bool, bool, bool, bool) =
        (row.get(0), row.get(1), row.get(2), row.get(3));
    assert!(!superuser, "the memory role is not a superuser");
    assert!(!createdb, "the memory role cannot create databases");
    assert!(!createrole, "the memory role cannot create roles");
    assert!(!bypassrls, "the memory role cannot bypass row security");

    // It also has no privilege on the business database at all.
    let granted: i64 = rt.block_on(async {
        admin_client
            .query_one(
                "SELECT count(*) FROM pg_database WHERE datname = $1 AND datacl IS NOT NULL \
                 AND array_to_string(datacl, ',') LIKE '%' || $2 || '%'",
                &[&tenant("postgres").as_str(), &placement.role],
            )
            .await
            .expect("pg_database is readable")
            .get(0)
    });
    assert_eq!(
        granted, 0,
        "the memory role holds nothing on another database"
    );
    drop(provisioned);
}
