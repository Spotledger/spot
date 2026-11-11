//! Migrations against a real PostgreSQL (skipped, with a SKIPPED line, when
//! `SPOT_TEST_PG_URL` is unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_pg_migrate::{
    ApplyOptions, MigrateError, PlanOptions, Risk, Run, apply, codes, plan, plan_with,
};
use spot_storage_contract::{FieldType, StorageBackend};
use support::*;

fn every_type() -> Vec<spot_storage_contract::FieldDef> {
    vec![
        field("flag", FieldType::Bool, true),
        field("qty", FieldType::Int, true),
        field("amount", dec(18, 2), true),
        field("title", text(Some(40)), true),
        field("notes", text(None), true),
        field("due", FieldType::Date, true),
        field("at", FieldType::Time, true),
        field("created", FieldType::DateTime, true),
        field("party", link("party"), true),
        field("extra", FieldType::Json, true),
        field("blob", FieldType::Bytes, true),
    ]
}

fn base(version: u64) -> spot_storage_contract::Catalog {
    catalog(
        version,
        vec![
            doc("party", vec![field("name", text(Some(140)), false)]),
            with_child(
                with_index(
                    doc("invoice", every_type()),
                    "invoice_title_idx",
                    &["title"],
                    false,
                ),
                "lines",
                "invoice_line",
            ),
            child(
                "invoice_line",
                "invoice",
                vec![field("qty", FieldType::Int, false)],
            ),
        ],
    )
}

#[tokio::test]
async fn a_new_database_gets_every_table_field_type_index_and_child() {
    let Some(rig) = rig().await else { return };
    let wanted = base(1);
    let first = rig.plan(&wanted).await;
    assert!(first.refused.is_empty());
    assert!(first.steps.iter().all(|s| s.risk == Risk::Additive));
    let report = rig.migrate(&wanted).await;
    assert!(report.executed.iter().any(|k| k == "create_table:invoice"));

    // Nothing left to do, and the backend's own `prepare` agrees with what was built.
    assert!(rig.plan(&wanted).await.is_empty());
    rig.backend.prepare(&wanted).await.unwrap();
    let client = rig.db.raw_client().await;
    let triggers: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'spot_capture' AND NOT tgisinternal",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(triggers, 2, "the change feed captures both document tables");
}

#[tokio::test]
async fn a_schema_built_by_prepare_plans_as_unchanged() {
    let Some(rig) = rig().await else { return };
    let wanted = base(1);
    rig.backend.prepare(&wanted).await.unwrap();
    let p = rig.plan(&wanted).await;
    assert!(p.is_empty(), "{:?} {:?}", p.steps, p.refused);
}

#[tokio::test]
async fn plan_of_an_unchanged_catalogue_is_empty_and_the_hash_is_stable() {
    let Some(rig) = rig().await else { return };
    let wanted = base(1);
    let (a, b) = (rig.plan(&wanted).await, rig.plan(&wanted).await);
    assert_eq!(a.plan_hash, b.plan_hash);
    assert_eq!(a.plan_hash.len(), 64);
    rig.migrate(&wanted).await;
    let again = rig.plan(&wanted).await;
    assert!(again.is_empty());
    assert_ne!(again.plan_hash, a.plan_hash);
    let report = rig.migrate(&wanted).await;
    assert!(report.executed.is_empty(), "a second run does nothing");
}

#[tokio::test]
async fn safe_changes_are_applied_online_and_keep_the_data() {
    let Some(rig) = rig().await else { return };
    let v1 = catalog(
        1,
        vec![doc(
            "party",
            vec![
                field("name", text(Some(20)), false),
                field("limit", dec(10, 2), false),
                field("nick", text(None), false),
            ],
        )],
    );
    rig.migrate(&v1).await;
    let client = rig.db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO party (id, version, name, \"limit\", nick) \
             VALUES (gen_random_uuid(), 1, 'Acme', 12345678.99, 'a')",
        )
        .await
        .unwrap();

    // Widen the limit and the numeric, relax a NOT NULL, add an optional column and an index.
    let v2 = catalog(
        2,
        vec![with_index(
            doc(
                "party",
                vec![
                    field("name", text(Some(60)), false),
                    field("limit", dec(14, 4), false),
                    field("nick", text(None), true),
                    field("note", text(Some(10)), true),
                ],
            ),
            "party_name_idx",
            &["name"],
            true,
        )],
    );
    let p = rig.plan(&v2).await;
    assert!(p.refused.is_empty(), "{:?}", p.refused);
    assert!(
        p.steps
            .iter()
            .any(|s| matches!(&s.run, Run::Outside(sql) if sql.contains("CONCURRENTLY"))),
        "the index is built concurrently"
    );
    apply(&rig.backend, &p, ApplyOptions::default())
        .await
        .unwrap();
    assert!(rig.plan(&v2).await.is_empty());
    rig.backend.prepare(&v2).await.unwrap();
    let row = client
        .query_one("SELECT \"limit\"::text, name FROM party", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "12345678.9900");
    assert_eq!(row.get::<_, String>(1), "Acme");
}

#[tokio::test]
async fn a_required_field_on_a_populated_table_is_backfilled_in_batches() {
    let Some(rig) = rig().await else { return };
    let v1 = catalog(
        1,
        vec![doc("party", vec![field("name", text(None), false)])],
    );
    rig.migrate(&v1).await;
    let client = rig.db.raw_client().await;
    client
        .batch_execute(
            "INSERT INTO party (id, version, name) \
             SELECT gen_random_uuid(), 1, 'n' || g FROM generate_series(1, 50000) g",
        )
        .await
        .unwrap();

    let v2 = catalog(
        2,
        vec![doc(
            "party",
            vec![
                field("name", text(None), false),
                field("tier", FieldType::Int, false),
                field("code", text(None), false),
            ],
        )],
    );
    let mut options = PlanOptions::default();
    options
        .defaults
        .insert(("party".into(), "tier".into()), "3".into());
    let p = plan_with(&rig.live().await, &v2, &options);
    assert!(p.steps.iter().any(|s| s.risk == Risk::NeedsBackfill));

    // A writer keeps inserting while the backfill runs; none of its inserts may fail.
    let writer_client = rig.db.raw_client().await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let writer = tokio::spawn(async move {
        let mut inserted = 0u32;
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            // Before the column exists the insert has no tier; after, it supplies one.
            let with_tier = writer_client
                .execute(
                    "INSERT INTO party (id, version, name, tier, code) \
                     VALUES (gen_random_uuid(), 1, 'live', 9, 'c')",
                    &[],
                )
                .await;
            match with_tier {
                Ok(_) => inserted += 1,
                Err(e) if e.code().map(|c| c.code()) == Some("42703") => {
                    writer_client
                        .execute(
                            "INSERT INTO party (id, version, name) \
                             VALUES (gen_random_uuid(), 1, 'live')",
                            &[],
                        )
                        .await
                        .unwrap();
                    inserted += 1;
                }
                Err(e) => panic!("a concurrent insert failed: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        inserted
    });
    let report = apply(
        &rig.backend,
        &p,
        ApplyOptions {
            batch_size: 5_000,
            ..ApplyOptions::default()
        },
    )
    .await
    .unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let live_rows = writer.await.unwrap();

    assert!(
        report.backfilled_rows >= 50_000,
        "{}",
        report.backfilled_rows
    );
    let total: i64 = client
        .query_one("SELECT count(*) FROM party", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(total, 50_000 + i64::from(live_rows));
    let nulls: i64 = client
        .query_one(
            "SELECT count(*) FROM party WHERE tier IS NULL OR code IS NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(nulls, 0);
    let (filled, sum): (i64, i64) = {
        let r = client
            .query_one(
                "SELECT count(*), sum(tier)::bigint FROM party WHERE name <> 'live'",
                &[],
            )
            .await
            .unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!(filled, 50_000);
    assert_eq!(
        sum,
        3 * 50_000,
        "every old row got the default, exactly once"
    );
    assert!(rig.plan_with_defaults(&v2, &options).await.is_empty());
}

#[tokio::test]
async fn every_refused_class_is_refused_and_changes_nothing() {
    let Some(rig) = rig().await else { return };
    let v1 = catalog(
        1,
        vec![
            doc(
                "party",
                vec![
                    field("name", text(Some(50)), false),
                    field("limit", dec(10, 4), true),
                    field("qty", FieldType::Int, true),
                    field("nick", text(None), true),
                    field("old", FieldType::Bool, true),
                ],
            ),
            doc("legacy", vec![]),
        ],
    );
    rig.migrate(&v1).await;
    let before = rig.live().await;

    let party = |fields: Vec<_>| doc("party", fields);
    let cases: Vec<(&str, spot_storage_contract::Catalog)> = vec![
        (
            "drop column",
            catalog(
                2,
                vec![
                    party(vec![field("name", text(Some(50)), false)]),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        ("drop table", catalog(2, vec![party(everything_v1())])),
        (
            "narrow numeric",
            catalog(
                2,
                vec![
                    party(replace(everything_v1(), field("limit", dec(10, 2), true))),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        (
            "change type family",
            catalog(
                2,
                vec![
                    party(replace(everything_v1(), field("qty", text(None), true))),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        (
            "tighten to required",
            catalog(
                2,
                vec![
                    party(replace(everything_v1(), field("nick", text(None), false))),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        (
            "lower limit",
            catalog(
                2,
                vec![
                    party(replace(
                        everything_v1(),
                        field("name", text(Some(10)), false),
                    )),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        (
            "new limit",
            catalog(
                2,
                vec![
                    party(replace(everything_v1(), field("nick", text(Some(5)), true))),
                    doc("legacy", vec![]),
                ],
            ),
        ),
        (
            "rename",
            catalog(
                2,
                vec![
                    party(replace_name(everything_v1(), "old", "older")),
                    doc("legacy", vec![]),
                ],
            ),
        ),
    ];
    for (label, wanted) in cases {
        let p = rig.plan(&wanted).await;
        assert!(!p.refused.is_empty(), "{label} must be refused");
        assert!(p.refused.iter().all(|r| !r.reason.is_empty()));
        let err = apply(&rig.backend, &p, ApplyOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, MigrateError::Refused(_)), "{label}: {err:?}");
        assert_eq!(err.code(), codes::REFUSED);
        assert_eq!(rig.live().await, before, "{label} changed the schema");
    }
    // A different collation is refused too.
    let other = spot_storage_contract::Catalog::new(
        2,
        spot_storage_contract::Collation::new("ur-PK").unwrap(),
        v1_entities(),
    )
    .unwrap();
    assert!(
        rig.plan(&other)
            .await
            .refused
            .iter()
            .any(|r| r.reason.contains("collation"))
    );
}

fn everything_v1() -> Vec<spot_storage_contract::FieldDef> {
    vec![
        field("name", text(Some(50)), false),
        field("limit", dec(10, 4), true),
        field("qty", FieldType::Int, true),
        field("nick", text(None), true),
        field("old", FieldType::Bool, true),
    ]
}

fn v1_entities() -> Vec<spot_storage_contract::EntityDef> {
    vec![doc("party", everything_v1()), doc("legacy", vec![])]
}

fn replace(
    mut fields: Vec<spot_storage_contract::FieldDef>,
    new: spot_storage_contract::FieldDef,
) -> Vec<spot_storage_contract::FieldDef> {
    for f in &mut fields {
        if f.name == new.name {
            *f = new.clone();
        }
    }
    fields
}

fn replace_name(
    mut fields: Vec<spot_storage_contract::FieldDef>,
    from: &str,
    to: &str,
) -> Vec<spot_storage_contract::FieldDef> {
    for f in &mut fields {
        if f.name.as_str() == from {
            f.name = n(to);
        }
    }
    fields
}

#[tokio::test]
async fn a_crash_after_any_step_is_finished_by_the_next_run() {
    let Some(first) = rig().await else { return };
    // Count the steps of the migration once, then crash after each in turn on a fresh database.
    let v1 = catalog(
        1,
        vec![doc("party", vec![field("name", text(Some(40)), false)])],
    );
    let v2 = catalog(
        2,
        vec![
            with_index(
                doc(
                    "party",
                    vec![
                        field("name", text(Some(80)), false),
                        field("tier", FieldType::Int, false),
                        field("boss", link("party"), true),
                    ],
                ),
                "party_tier_idx",
                &["tier"],
                false,
            ),
            child("party_note", "party", vec![field("body", text(None), true)]),
        ]
        .into_iter()
        .map(|e| {
            if e.name.as_str() == "party" {
                with_child(e, "notes", "party_note")
            } else {
                e
            }
        })
        .collect(),
    );
    let steps = {
        first.migrate(&v1).await;
        first.backend.prepare(&v1).await.unwrap();
        let client = first.db.raw_client().await;
        client
            .batch_execute("INSERT INTO party (id, version, name) SELECT gen_random_uuid(), 1, 'x' || g FROM generate_series(1, 100) g")
            .await
            .unwrap();
        first.plan(&v2).await.steps.len()
    };
    assert!(steps >= 6, "the migration has several steps: {steps}");
    drop(first);

    for stop in 0..steps {
        let Some(rig) = rig().await else { return };
        rig.migrate(&v1).await;
        rig.db
            .raw_client()
            .await
            .batch_execute("INSERT INTO party (id, version, name) SELECT gen_random_uuid(), 1, 'x' || g FROM generate_series(1, 100) g")
            .await
            .unwrap();
        let p = rig.plan(&v2).await;
        let options = ApplyOptions {
            stop_after_step: Some(stop),
            ..ApplyOptions::default()
        };
        match apply(&rig.backend, &p, options).await {
            Err(MigrateError::Interrupted { step }) => assert_eq!(step, stop),
            other => panic!("step {stop}: expected an interruption, got {other:?}"),
        }
        // The half-migrated schema plans cleanly (nothing is refused) and the rerun finishes.
        let rest = rig.plan(&v2).await;
        assert!(
            rest.refused.is_empty(),
            "after step {stop}: {:?}",
            rest.refused
        );
        rig.migrate(&v2).await;
        assert!(rig.plan(&v2).await.is_empty(), "after step {stop}");
        rig.backend.prepare(&v2).await.unwrap();
    }
}

#[tokio::test]
async fn two_concurrent_applies_on_one_site_run_once() {
    let Some(rig) = rig().await else { return };
    let wanted = base(1);
    let p = rig.plan(&wanted).await;
    let (a, b) = tokio::join!(
        apply(&rig.backend, &p, ApplyOptions::default()),
        apply(&rig.backend, &p, ApplyOptions::default())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let ran = [a.executed.len(), b.executed.len()];
    assert!(
        ran.contains(&0) && ran.iter().any(|n| *n > 0),
        "exactly one of the two did the work: {ran:?}"
    );
    assert!(rig.plan(&wanted).await.is_empty());
}

#[tokio::test]
async fn a_required_link_without_a_default_is_refused_by_name() {
    let Some(rig) = rig().await else { return };
    let v1 = catalog(
        1,
        vec![doc("party", vec![field("name", text(None), false)])],
    );
    rig.migrate(&v1).await;
    rig.db
        .raw_client()
        .await
        .batch_execute("INSERT INTO party (id, version, name) VALUES (gen_random_uuid(), 1, 'a')")
        .await
        .unwrap();
    let v2 = catalog(
        2,
        vec![doc(
            "party",
            vec![
                field("name", text(None), false),
                field("boss", link("party"), false),
            ],
        )],
    );
    let p = plan(&rig.live().await, &v2);
    assert_eq!(p.refused.len(), 1);
    assert_eq!(p.refused[0].code, codes::NO_BACKFILL_DEFAULT);
}
