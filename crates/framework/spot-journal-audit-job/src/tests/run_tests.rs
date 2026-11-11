use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_conditions::{ConditionEngine, MemorySink};
use spot_journal_audit::Position;

use super::fixture::{Journal, engine, engine_without_catalogue, runtime, uuid};
use crate::memory::{FnInputs, MemoryBook, SharedClock};
use crate::{
    AuditInputs, AuditJobError, Cadence, JOURNAL_AUDIT_FAILED, JOURNAL_AUDIT_MISMATCH, RunSummary,
    TenantOutcome, run_due,
};

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = crate::DAY_MILLIS;

#[derive(Clone, Copy, Debug)]
enum Mode {
    Clean,
    Tamper,
    Truncate,
    Panic,
    OpenFails,
    /// The plan says this tenant; the options belong to `other`.
    OptionsOfOther,
    /// The options are right; the live side belongs to `other`.
    LiveOfOther,
}

struct Env {
    rt: tokio::runtime::Runtime,
    book: Arc<MemoryBook>,
    engine: ConditionEngine<MemorySink>,
    clock: SharedClock,
    modes: Arc<Mutex<BTreeMap<String, Mode>>>,
    inputs: Box<dyn AuditInputs>,
}

fn env(tenants: &[&str]) -> Env {
    let rt = runtime();
    let book = Arc::new(MemoryBook::new());
    let mut journals = BTreeMap::new();
    let mut modes = BTreeMap::new();
    for tenant in tenants {
        book.add(tenant, Cadence::Daily, None);
        journals.insert((*tenant).to_owned(), Arc::new(Journal::new(&rt, tenant, 5)));
        modes.insert((*tenant).to_owned(), Mode::Clean);
    }
    let modes = Arc::new(Mutex::new(modes));
    let seen = Arc::clone(&modes);
    let other = journals.values().last().cloned();
    let inputs = FnInputs::new(move |tenant| {
        let journal = journals.get(tenant.as_str()).unwrap();
        let mode = seen.lock().unwrap()[tenant.as_str()];
        match mode {
            Mode::Clean => Ok(journal.inputs(journal.live(), false)),
            Mode::Tamper => Ok(journal.inputs(journal.tampered_live(), false)),
            Mode::Truncate => Ok(journal.inputs(journal.live(), true)),
            Mode::Panic => panic!("provider blew up with SECRET-PANIC"),
            Mode::OpenFails => Err(AuditJobError::ProviderFailed { provider: "inputs" }),
            Mode::OptionsOfOther => {
                let other = other.as_ref().unwrap();
                Ok(other.inputs(other.live(), false))
            }
            Mode::LiveOfOther => {
                let other = other.as_ref().unwrap();
                Ok(journal.inputs(other.live(), false))
            }
        }
    });
    Env {
        rt,
        book,
        engine: engine(),
        clock: SharedClock::new(NOW, 5),
        modes,
        inputs: Box::new(inputs),
    }
}

impl Env {
    fn set(&self, tenant: &str, mode: Mode) {
        self.modes.lock().unwrap().insert(tenant.to_owned(), mode);
    }

    fn run(&mut self, now: i64) -> RunSummary {
        run_due(
            self.rt.handle(),
            now,
            self.book.as_ref(),
            self.inputs.as_ref(),
            &self.clock,
            &mut self.engine,
            self.book.as_ref(),
        )
        .unwrap()
    }

    fn open(&self) -> Vec<(String, String)> {
        self.engine
            .open()
            .iter()
            .map(|o| (o.id.as_str().to_owned(), o.subject.as_str().to_owned()))
            .collect()
    }
}

#[test]
fn a_clean_tenant_clears_open_conditions_and_records_the_position() {
    let mut env = env(&["acme"]);
    env.engine
        .raise(
            JOURNAL_AUDIT_MISMATCH,
            "acme",
            spot_conditions::ConditionParams::new(),
            spot_conditions::Millis::new(1),
        )
        .unwrap();
    env.engine
        .raise(
            JOURNAL_AUDIT_FAILED,
            "acme",
            spot_conditions::ConditionParams::new(),
            spot_conditions::Millis::new(1),
        )
        .unwrap();
    assert_eq!(env.open().len(), 2);
    let summary = env.run(NOW);
    let result = summary.result("acme").unwrap();
    assert_eq!(result.outcome, TenantOutcome::Clean);
    assert!(env.open().is_empty());
    assert_eq!(
        result.verified_position().unwrap().position,
        Position::new(1, 5)
    );
    assert_eq!(env.book.results().len(), 1);
}

#[test]
fn a_tampered_record_raises_the_mismatch_with_the_first_difference_and_no_content() {
    let mut env = env(&["acme"]);
    let summary = env.run(NOW);
    assert_eq!(
        summary.result("acme").unwrap().outcome,
        TenantOutcome::Clean
    );
    env.set("acme", Mode::Tamper);
    let summary = env.run(NOW + DAY);
    let result = summary.result("acme").unwrap();
    assert_eq!(result.outcome, TenantOutcome::Mismatch);
    assert!(result.verified_position().is_none());
    let open = env.engine.open();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), JOURNAL_AUDIT_MISMATCH);
    assert_eq!(open[0].subject.as_str(), "acme");
    assert_eq!(
        open[0].params.get("first_differing_record"),
        Some(format!("account/{}", uuid(2)).as_str())
    );
    assert_eq!(open[0].params.get("difference"), Some("content_differs"));
    // No record content anywhere: not in the condition, not in the result row, not in the
    // audit events.
    let shown = format!("{open:?} {result:?} {:?}", env.engine.sink().events());
    assert!(!shown.contains("SECRET-TAMPERED"), "{shown}");
}

#[test]
fn a_failed_rebuild_raises_failed_and_the_next_clean_run_clears_it() {
    let mut env = env(&["acme"]);
    env.set("acme", Mode::Truncate);
    let summary = env.run(NOW);
    let result = summary.result("acme").unwrap();
    let TenantOutcome::Failed(info) = result.outcome else {
        panic!("{:?}", result.outcome);
    };
    assert_eq!(info.stage, "plan");
    assert!(info.code.starts_with("journal_rebuild"), "{}", info.code);
    let open = env.engine.open();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id.as_str(), JOURNAL_AUDIT_FAILED);
    assert_eq!(open[0].params.get("stage"), Some("plan"));
    assert_eq!(open[0].params.get("code"), Some(info.code));
    assert!(result.verified_position().is_none());

    env.set("acme", Mode::Clean);
    let summary = env.run(NOW + DAY);
    assert_eq!(
        summary.result("acme").unwrap().outcome,
        TenantOutcome::Clean
    );
    assert!(env.open().is_empty());
}

#[test]
fn a_mismatch_after_a_failure_clears_the_failure() {
    let mut env = env(&["acme"]);
    env.set("acme", Mode::Truncate);
    env.run(NOW);
    env.set("acme", Mode::Tamper);
    env.run(NOW + DAY);
    assert_eq!(
        env.open(),
        vec![(JOURNAL_AUDIT_MISMATCH.to_owned(), "acme".to_owned())]
    );
}

#[test]
fn one_tenant_failing_does_not_stop_the_others() {
    let mut env = env(&["acme", "beta", "gamma"]);
    env.set("acme", Mode::Truncate);
    env.set("beta", Mode::OpenFails);
    let summary = env.run(NOW);
    assert_eq!(summary.results.len(), 3);
    assert!(matches!(
        summary.result("acme").unwrap().outcome,
        TenantOutcome::Failed(_)
    ));
    let TenantOutcome::Failed(info) = summary.result("beta").unwrap().outcome else {
        panic!("beta");
    };
    assert_eq!(
        (info.stage, info.code),
        ("inputs", "journal_audit_job.provider_failed")
    );
    assert_eq!(
        summary.result("gamma").unwrap().outcome,
        TenantOutcome::Clean
    );
    assert_eq!(env.book.results().len(), 3);
}

#[test]
fn a_panicking_provider_is_contained_at_the_tenant_boundary() {
    let mut env = env(&["acme", "beta"]);
    env.set("acme", Mode::Panic);
    let summary = env.run(NOW);
    let TenantOutcome::Failed(info) = summary.result("acme").unwrap().outcome else {
        panic!("acme");
    };
    assert_eq!(
        (info.stage, info.code),
        ("panic", "journal_audit_job.tenant_panicked")
    );
    assert_eq!(
        summary.result("beta").unwrap().outcome,
        TenantOutcome::Clean
    );
    // The panic text is not kept anywhere.
    let shown = format!("{summary:?} {:?}", env.engine.open());
    assert!(!shown.contains("SECRET-PANIC"));
    let open = env.open();
    assert_eq!(
        open,
        vec![(JOURNAL_AUDIT_FAILED.to_owned(), "acme".to_owned())]
    );
}

#[test]
fn a_second_pass_in_the_same_period_does_nothing() {
    let mut env = env(&["acme", "beta"]);
    let first = env.run(NOW);
    assert_eq!(first.results.len(), 2);
    // An hour later, and just before the day is over: nothing is due.
    for later in [NOW + 3_600_000, NOW + DAY - 1] {
        let again = env.run(later);
        assert!(again.results.is_empty());
        assert_eq!(again.not_due, 2);
    }
    assert_eq!(env.book.results().len(), 2);
    // A day after the first pass started it is due again.
    assert_eq!(env.run(NOW + DAY).results.len(), 2);
}

#[test]
fn a_tenant_listed_twice_is_audited_once() {
    let mut env = env(&["acme"]);
    env.book.add("acme", Cadence::Daily, None);
    assert_eq!(env.run(NOW).results.len(), 1);
}

#[test]
fn inputs_of_another_tenant_than_the_plan_are_refused_before_anything_runs() {
    let mut env = env(&["acme", "beta"]);
    env.set("acme", Mode::OptionsOfOther);
    let summary = env.run(NOW);
    let result = summary.result("acme").unwrap();
    let TenantOutcome::Failed(info) = result.outcome else {
        panic!("acme");
    };
    assert_eq!(
        (info.stage, info.code),
        ("refused", "journal_audit_job.tenant_mismatch")
    );
    assert!(result.run.is_none());
}

#[test]
fn a_live_side_of_another_tenant_is_refused_by_the_runner() {
    let mut env = env(&["acme", "beta"]);
    env.set("acme", Mode::LiveOfOther);
    let summary = env.run(NOW);
    let result = summary.result("acme").unwrap();
    let TenantOutcome::Failed(info) = result.outcome else {
        panic!("acme");
    };
    assert_eq!(
        (info.stage, info.code),
        ("refused", "journal_audit_run.tenant_mismatch")
    );
    assert!(result.run.is_none());
    assert_eq!(
        summary.result("beta").unwrap().outcome,
        TenantOutcome::Clean
    );
}

#[test]
fn an_unreadable_directory_fails_the_pass() {
    let mut env = env(&["acme"]);
    env.book.set_directory_failing(true);
    let outcome = run_due(
        env.rt.handle(),
        NOW,
        env.book.as_ref(),
        env.inputs.as_ref(),
        &env.clock,
        &mut env.engine,
        env.book.as_ref(),
    );
    assert_eq!(
        outcome.unwrap_err(),
        AuditJobError::ProviderFailed {
            provider: "directory"
        }
    );
}

#[test]
fn the_registered_conditions_are_red_tenant_conditions() {
    let env = env(&[]);
    for id in [JOURNAL_AUDIT_MISMATCH, JOURNAL_AUDIT_FAILED] {
        let def = env.engine.registry().get(id).unwrap();
        assert_eq!(def.severity, spot_conditions::Severity::Red);
        assert_eq!(def.subject_kind, "tenant");
    }
}

#[test]
fn the_host_registry_has_both_ids_without_a_duplicate() {
    let mut registry = spot_sync_conditions::catalogue().unwrap();
    crate::register_conditions(&mut registry).unwrap();
    assert!(registry.get(JOURNAL_AUDIT_MISMATCH).is_some());
    assert!(registry.get(JOURNAL_AUDIT_FAILED).is_some());
    // A second call is the duplicate the host must not make.
    assert!(crate::register_conditions(&mut registry).is_err());
}

#[test]
fn the_catalogue_and_the_job_agree_on_the_mismatch_shape() {
    let mut env = env(&["acme"]);
    env.set("acme", Mode::Tamper);
    env.run(NOW);
    let open = env.engine.open();
    assert_eq!(open.len(), 1);
    let raised = &open[0];
    let def = spot_sync_conditions::entry(JOURNAL_AUDIT_MISMATCH)
        .unwrap()
        .def;
    assert_eq!(def.subject_kind, crate::SUBJECT_KIND);
    // The raised params are complete against the catalogue's definition...
    assert!(def.missing_params(&raised.params).is_empty());
    // ...and carry no name the definition does not list.
    let names: Vec<&str> = raised.params.iter().map(|(name, _)| name).collect();
    assert_eq!(names.len(), 2);
    for name in names {
        assert!(
            def.params.iter().any(|p| p == name),
            "undeclared param {name}"
        );
    }
}

#[test]
fn a_registry_without_the_catalogue_fails_a_mismatch_loudly_and_the_others_still_run() {
    let mut env = env(&["acme", "beta", "gamma"]);
    env.engine = engine_without_catalogue();
    env.set("acme", Mode::Tamper);
    env.set("beta", Mode::Truncate);
    let summary = env.run(NOW);
    let acme = summary.result("acme").unwrap();
    assert_eq!(acme.outcome, TenantOutcome::Mismatch);
    assert_eq!(acme.condition_failure, Some("conditions.unknown_id"));
    // A failed rebuild raises JOURNAL_AUDIT_FAILED, which this crate registers: no failure.
    assert_eq!(summary.result("beta").unwrap().condition_failure, None);
    assert_eq!(
        env.open(),
        vec![(JOURNAL_AUDIT_FAILED.to_owned(), "beta".to_owned())]
    );
    // A clean tenant clears the mismatch id too, so it is counted as well.
    assert_eq!(
        summary.result("gamma").unwrap().outcome,
        TenantOutcome::Clean
    );
    assert_eq!(summary.condition_failures(), 2);
    assert_eq!(summary.results.len(), 3);
    assert_eq!(env.book.results().len(), 3);
}
