//! Seeded-corruption proofs: break the store on purpose and show the invariants notice.
//!
//! A [`Corruptor`] damages a PostgreSQL scratch database behind the backend's back (through the
//! raw client), in exactly one way. [`prove`] runs a clean workload, checks that **every**
//! invariant holds, applies the corruption, and checks that **exactly** the expected invariants
//! fail and name the entity and id involved.
//!
//! The corruptors are PostgreSQL-only: the in-memory fake keeps its state private (and the
//! contract offers no way to write a version or an outbox row directly), so on the fake the same
//! invariants are proven against the weakened backends of [`crate::faulty`] and by
//! [`burn_sequence_number`], which uses only the contract.

use std::sync::Arc;

use spot_postgres::testing::TestDb;
use spot_storage_contract::{
    Isolation, Name, Sequences, SeriesKey, StorageBackend, StorageResult, Transaction, TxOptions,
};

use crate::builtin::framework_registry;
use crate::driver::DriverConfig;
use crate::invariant::{BoxFuture, Violation};
use crate::model::Model;
use crate::pg::pg_env;
use crate::runner::{check, run_workload};
use crate::schedule::Profile;

/// One way to break a PostgreSQL store.
pub trait Corruptor: Send + Sync {
    /// A short stable name.
    fn name(&self) -> &'static str;
    /// The ids of the invariants that must fail afterwards, and no others. A corruption that
    /// removes a document is also seen by the oracle-based count; the doc lists both.
    fn expected(&self) -> &'static [&'static str];
    /// The entity the first expected invariant names in its violations.
    fn entity(&self) -> &'static str;
    /// Applies the corruption to the scratch database. An error means the corruption did not
    /// apply (for example no suitable victim row), which makes the proof fail.
    fn apply<'a>(&'a self, db: &'a TestDb) -> BoxFuture<'a, Result<(), String>>;
}

struct Sql {
    name: &'static str,
    expected: &'static [&'static str],
    entity: &'static str,
    sql: &'static str,
}

impl Corruptor for Sql {
    fn name(&self) -> &'static str {
        self.name
    }
    fn expected(&self) -> &'static [&'static str] {
        self.expected
    }
    fn entity(&self) -> &'static str {
        self.entity
    }
    fn apply<'a>(&'a self, db: &'a TestDb) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let client = db.raw_client().await;
            client
                .batch_execute(self.sql)
                .await
                .map_err(|e| format!("{}: {e}", self.name))
        })
    }
}

/// Every corruption, one per kind of promise the framework invariants make.
///
/// The SQL changes data only. Where a trigger of the backend would record the change in the change
/// feed, or a foreign key would refuse it, the statement turns that off for the table and puts it
/// back, as a bug or an operator working behind the backend's back would.
pub fn pg_corruptors() -> Vec<Box<dyn Corruptor>> {
    let all: Vec<Sql> = vec![
        Sql {
            name: "skip_a_sequence_value",
            expected: &["seq-gapless"],
            entity: "sequence",
            sql: "DO $$ DECLARE n bigint; BEGIN
                    UPDATE spot_sequence SET next_value = next_value + 1 WHERE series = 'l7-series-0';
                    GET DIAGNOSTICS n = ROW_COUNT;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "duplicate_a_sequence_value",
            expected: &["seq-gapless"],
            entity: "sequence",
            sql: "DO $$ DECLARE n bigint; BEGIN
                    UPDATE spot_sequence SET next_value = next_value - 1 WHERE series = 'l7-series-0';
                    GET DIAGNOSTICS n = ROW_COUNT;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "decrement_a_document_version",
            expected: &["doc-version_continuity"],
            entity: "l7_order",
            sql: "DO $$ DECLARE n bigint; BEGIN
                    ALTER TABLE l7_order DISABLE TRIGGER spot_capture;
                    UPDATE l7_order SET version = version - 1
                      WHERE id = (SELECT id FROM l7_order WHERE version > 1 ORDER BY id LIMIT 1);
                    GET DIAGNOSTICS n = ROW_COUNT;
                    ALTER TABLE l7_order ENABLE TRIGGER spot_capture;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "delete_one_outbox_row",
            expected: &["outbox-complete"],
            entity: "outbox",
            sql: "DO $$ DECLARE victim uuid; BEGIN
                    PERFORM set_config('spot.feed_maintenance', 'on', true);
                    SELECT id INTO victim FROM spot_outbox ORDER BY id OFFSET 5 LIMIT 1;
                    IF victim IS NULL THEN RAISE EXCEPTION 'no victim'; END IF;
                    DELETE FROM spot_outbox_pub WHERE event_id = victim;
                    DELETE FROM spot_outbox WHERE id = victim;
                  END $$",
        },
        Sql {
            name: "insert_an_orphan_outbox_row",
            expected: &["outbox-complete"],
            entity: "outbox",
            sql: "INSERT INTO spot_outbox
                    (id, tx, ordinal, topic, subject_entity, subject_id, subject_version,
                     schema_version, payload, captured_at, xid)
                  VALUES (uuidv7(), uuidv7(), 0, 'l7_commit', 'l7_party', uuidv7(), NULL, 1,
                          '{}', clock_timestamp(), pg_current_xact_id())",
        },
        Sql {
            name: "delete_a_parent_leaving_children",
            expected: &["child-integrity", "doc-count_matches_oracle"],
            entity: "l7_order_line",
            sql: "DO $$ DECLARE c text; n bigint; BEGIN
                    FOR c IN SELECT conname FROM pg_constraint
                             WHERE conrelid = 'l7_order_line'::regclass AND contype = 'f' LOOP
                      EXECUTE format('ALTER TABLE l7_order_line DROP CONSTRAINT %I', c);
                    END LOOP;
                    ALTER TABLE l7_order DISABLE TRIGGER spot_capture;
                    DELETE FROM l7_order
                      WHERE id = (SELECT parent FROM l7_order_line ORDER BY parent LIMIT 1);
                    GET DIAGNOSTICS n = ROW_COUNT;
                    ALTER TABLE l7_order ENABLE TRIGGER spot_capture;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "set_a_link_to_a_missing_id",
            expected: &["link-integrity"],
            entity: "l7_order",
            sql: "DO $$ DECLARE c text; n bigint; BEGIN
                    FOR c IN SELECT conname FROM pg_constraint
                             WHERE conrelid = 'l7_order'::regclass AND contype = 'f' LOOP
                      EXECUTE format('ALTER TABLE l7_order DROP CONSTRAINT %I', c);
                    END LOOP;
                    ALTER TABLE l7_order DISABLE TRIGGER spot_capture;
                    UPDATE l7_order SET party = uuidv7()
                      WHERE id = (SELECT id FROM l7_order WHERE party IS NOT NULL ORDER BY id LIMIT 1);
                    GET DIAGNOSTICS n = ROW_COUNT;
                    ALTER TABLE l7_order ENABLE TRIGGER spot_capture;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "drop_a_unique_index_and_insert_a_duplicate",
            expected: &["unique-keys"],
            entity: "l7_party",
            sql: "DO $$ DECLARE n bigint; BEGIN
                    DROP INDEX l7_party_name;
                    ALTER TABLE l7_party DISABLE TRIGGER spot_capture;
                    UPDATE l7_party SET name = (SELECT name FROM l7_party ORDER BY id LIMIT 1)
                      WHERE id = (SELECT id FROM l7_party ORDER BY id DESC LIMIT 1);
                    GET DIAGNOSTICS n = ROW_COUNT;
                    ALTER TABLE l7_party ENABLE TRIGGER spot_capture;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
        Sql {
            name: "subtract_one_from_a_counter",
            expected: &["lost_update-free"],
            entity: "l7_counter",
            sql: "DO $$ DECLARE n bigint; BEGIN
                    ALTER TABLE l7_counter DISABLE TRIGGER spot_capture;
                    UPDATE l7_counter SET value = value - 1
                      WHERE id = (SELECT id FROM l7_counter ORDER BY id LIMIT 1);
                    GET DIAGNOSTICS n = ROW_COUNT;
                    ALTER TABLE l7_counter ENABLE TRIGGER spot_capture;
                    IF n <> 1 THEN RAISE EXCEPTION 'no victim'; END IF;
                  END $$",
        },
    ];
    all.into_iter()
        .map(|c| Box::new(c) as Box<dyn Corruptor>)
        .collect()
}

/// What a proof found.
#[derive(Clone, Debug)]
pub struct Proof {
    /// The corruptor's name.
    pub corruption: &'static str,
    /// Invariants that failed before the corruption (must be none).
    pub failed_before: Vec<String>,
    /// Invariants that failed after it.
    pub failed_after: Vec<String>,
    /// The violations of the expected invariants.
    pub violations: Vec<Violation>,
    /// The seed, to replay the schedule.
    pub seed: u64,
}

impl Proof {
    /// Whether the proof holds: clean before, exactly the expected invariants failing after, and
    /// every violation of the first expected invariant naming `entity` and a record id.
    pub fn holds(&self, c: &dyn Corruptor) -> bool {
        let mut want: Vec<&str> = c.expected().to_vec();
        want.sort_unstable();
        let mut got: Vec<&str> = self.failed_after.iter().map(String::as_str).collect();
        got.sort_unstable();
        let named = self.violations.iter().any(|v| {
            v.invariant == c.expected()[0]
                && v.entity == c.entity()
                && v.id != "-"
                && !v.id.is_empty()
        });
        self.failed_before.is_empty() && want == got && named
    }
}

/// Runs a clean workload on a fresh scratch database, checks everything holds, applies
/// `corruptor`, and checks again. `None` when no PostgreSQL server is configured.
pub async fn prove(
    corruptor: &dyn Corruptor,
    workers: u32,
    operations: u64,
    seed: u64,
) -> Option<Result<Proof, String>> {
    let env = pg_env(workers).await?;
    Some(
        prove_on(
            &env.db,
            env.backend.clone(),
            corruptor,
            workers,
            operations,
            seed,
        )
        .await,
    )
}

async fn prove_on<B: StorageBackend>(
    db: &TestDb,
    backend: Arc<B>,
    corruptor: &dyn Corruptor,
    workers: u32,
    operations: u64,
    seed: u64,
) -> Result<Proof, String> {
    let model = Arc::new(Model::new().map_err(|e| e.to_string())?);
    let mut config = DriverConfig::for_profile(Profile::Smoke, seed);
    config.workers = workers;
    config.operations = operations;
    let done = run_workload(backend.clone(), model.clone(), config, "pg")
        .await
        .map_err(|e| e.to_string())?;
    let registry = framework_registry(&model);
    let before = check(backend.clone(), &model, &done, &registry).await;
    corruptor.apply(db).await?;
    let after = check(backend, &model, &done, &registry).await;
    let mut violations = Vec::new();
    for id in corruptor.expected() {
        violations.extend(after.violations_of(id).iter().cloned());
    }
    Ok(Proof {
        corruption: corruptor.name(),
        failed_before: before
            .failed_ids()
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        failed_after: after.failed_ids().iter().map(|s| (*s).to_owned()).collect(),
        violations,
        seed,
    })
}

/// A corruption that needs only the contract: allocates and commits one number of `series`
/// without telling the driver, leaving a gap in the series. Works on any backend, so the fake is
/// covered too. The expected failing invariant is `seq-gapless`.
pub async fn burn_sequence_number<B: StorageBackend>(
    backend: &B,
    model: &Model,
    series: &str,
) -> StorageResult<()> {
    let options = TxOptions::new(Name::new("l7_burn")?, Isolation::ReadCommitted);
    let mut tx = backend.begin(model.catalog.clone(), options).await?;
    tx.allocate(&SeriesKey::new(series)?, 1).await?;
    tx.commit().await
}
