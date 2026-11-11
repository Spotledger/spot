// Tests for the review findings: keyed all-or-nothing validates before writing,
// idempotency keys cannot collide across imports, and a dry run reports permission refusals.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::testkit::*;
use super::*;
use crate::error::KEY_NOT_FOUND;
use spot_app_api::context::Actor;
use spot_app_api::prelude::{CommandId, Name, SiteId};
use spot_app_api::stub::StubHost;

/// A permission probe that refuses creating one DocType and updating one record id.
struct Deny {
    create: bool,
    update: Option<RecordId>,
}

impl PermissionProbe for Deny {
    fn check_create(&self, _inv: &Invocation, doctype: &str) -> Result<(), AppError> {
        if self.create {
            Err(AppError::new(
                spot_app_api::error::ErrorClass::PermissionDenied,
                "permission.denied",
                format!("you may not create {doctype}"),
            ))
        } else {
            Ok(())
        }
    }

    fn check_update(&self, _inv: &Invocation, doctype: &str, id: RecordId) -> Result<(), AppError> {
        if self.update == Some(id) {
            Err(AppError::new(
                spot_app_api::error::ErrorClass::PermissionDenied,
                "permission.denied",
                format!("you may not update {doctype}"),
            ))
        } else {
            Ok(())
        }
    }
}

fn run(host: &dyn DocumentService, items: Vec<ImportItem>, options: &ApplyOptions) -> ApplyOutcome {
    let mut progress = Recorder::default();
    block_on(apply_import(
        host,
        &invocation(),
        items.into_iter(),
        options,
        &mut progress,
    ))
}

fn rating_of(host: &StubHost, n: u8) -> Value {
    host.stored("customer", rid(n))
        .unwrap()
        .get("rating")
        .clone()
}

/// Blocker 2: an all-or-nothing update with a key that names nothing in the LAST row writes
/// nothing, not even the updates of the rows before it.
#[test]
fn all_or_nothing_update_with_a_bad_last_key_writes_nothing() {
    let host = host();
    let first = seed_customer(&host, 41, "Acme", "A");
    let second = seed_customer(&host, 42, "Beta", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), first.id());
    index.add("customer", "name", Value::Text("Beta".into()), second.id());
    let mut options = options(ApplyMode::Update {
        key_field: "name".to_owned(),
    });
    options.policy = ImportPolicy::AllOrNothing;
    options.index = Some(Arc::new(index));

    let outcome = run(
        &host,
        vec![
            customer_item(2, "Acme", Some("B")),
            customer_item(3, "Beta", Some("B")),
            customer_item(4, "Nobody", Some("B")),
        ],
        &options,
    );
    assert!(matches!(outcome, ApplyOutcome::NothingWritten(_)));
    let summary = outcome.summary();
    assert_eq!(summary.stopped, Some(StopReason::PolicyRefused));
    assert_eq!(summary.updated, 0);
    assert_eq!(entry(summary, KEY_NOT_FOUND).unwrap().row, 4);
    assert_eq!(
        rating_of(&host, 41),
        Value::Text("A".into()),
        "nothing updated"
    );
    assert_eq!(
        rating_of(&host, 42),
        Value::Text("A".into()),
        "nothing updated"
    );
}

/// Blocker 2: the same key twice in one file is refused before any write.
#[test]
fn all_or_nothing_update_with_a_repeated_key_writes_nothing() {
    let host = host();
    let seeded = seed_customer(&host, 41, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());
    let mut options = options(ApplyMode::Upsert {
        key_field: "name".to_owned(),
    });
    options.policy = ImportPolicy::AllOrNothing;
    options.index = Some(Arc::new(index));

    let outcome = run(
        &host,
        vec![
            customer_item(2, "Acme", Some("B")),
            customer_item(3, "Acme", Some("C")),
        ],
        &options,
    );
    let summary = outcome.summary();
    assert!(matches!(outcome, ApplyOutcome::NothingWritten(_)));
    assert_eq!(summary.updated, 0);
    assert_eq!(entry(summary, crate::error::DUPLICATE_KEY).unwrap().row, 3);
    assert_eq!(rating_of(&host, 41), Value::Text("A".into()));
}

/// Blocker 2: a permission refusal on a later row's update is found in the validation pass.
#[test]
fn all_or_nothing_update_refused_by_permission_writes_nothing() {
    let host = host();
    let first = seed_customer(&host, 41, "Acme", "A");
    let second = seed_customer(&host, 42, "Beta", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), first.id());
    index.add("customer", "name", Value::Text("Beta".into()), second.id());
    let mut options = options(ApplyMode::Update {
        key_field: "name".to_owned(),
    });
    options.policy = ImportPolicy::AllOrNothing;
    options.index = Some(Arc::new(index));
    options.permissions = Arc::new(Deny {
        create: false,
        update: Some(second.id()),
    });

    let outcome = run(
        &host,
        vec![
            customer_item(2, "Acme", Some("B")),
            customer_item(3, "Beta", Some("B")),
        ],
        &options,
    );
    let summary = outcome.summary();
    assert!(matches!(outcome, ApplyOutcome::NothingWritten(_)));
    assert_eq!(entry(summary, "permission.denied").unwrap().row, 3);
    assert_eq!(rating_of(&host, 41), Value::Text("A".into()));
}

fn user_invocation(site: &str, user: u8) -> Invocation {
    let mut inv = invocation();
    inv.site = SiteId::new(site).unwrap();
    inv.actor = Actor::User {
        id: rid(user),
        via: None,
    };
    inv
}

/// Blocker 3: the same label and row from a different site, actor, DocType or label never
/// share a command id; the same inputs always do.
#[test]
fn command_ids_differ_across_sites_actors_doctypes_and_labels() {
    use super::write::command_text;
    let base = user_invocation("site-a", 1);
    let id = |inv: &Invocation, doctype: &str, label: &str, row: u64| {
        command_text(inv, 'c', doctype, label, row)
    };
    let reference = id(&base, "customer", "job-1", 2);
    assert_eq!(reference, id(&base, "customer", "job-1", 2), "stable");
    assert_ne!(
        reference,
        id(&user_invocation("site-b", 1), "customer", "job-1", 2)
    );
    assert_ne!(
        reference,
        id(&user_invocation("site-a", 2), "customer", "job-1", 2)
    );
    assert_ne!(
        reference,
        id(&invocation(), "customer", "job-1", 2),
        "system actor"
    );
    assert_ne!(reference, id(&base, "supplier", "job-1", 2));
    assert_ne!(reference, id(&base, "customer", "job-2", 2));
    assert_ne!(reference, id(&base, "customer", "job-1", 3));
    assert_ne!(
        reference,
        command_text(&base, 'u', "customer", "job-1", 2),
        "operations differ"
    );
    assert!(reference.len() < 128 && reference.starts_with("impc-"));
    // Length prefixes: moving a character across fields changes the key.
    assert_ne!(id(&base, "ab", "c", 2), id(&base, "a", "bc", 2));
}

/// Blocker 3: two imports with the same label from different actors on a deduping host each
/// create their own document.
#[test]
fn two_actors_with_one_label_do_not_collide() {
    let host = host();
    let wrapped = Wrapped::deduping(&host);
    let options = options(ApplyMode::Insert);
    let mut progress = Recorder::default();
    let first = block_on(apply_import(
        &wrapped,
        &user_invocation("test", 1),
        vec![customer_item(2, "Acme", Some("A"))].into_iter(),
        &options,
        &mut progress,
    ))
    .into_summary();
    let second = block_on(apply_import(
        &wrapped,
        &user_invocation("test", 2),
        vec![customer_item(2, "Beta", Some("B"))].into_iter(),
        &options,
        &mut progress,
    ))
    .into_summary();
    assert_eq!((first.created, second.created), (1, 1));
    assert_ne!(first.created_drafts[0].id, second.created_drafts[0].id);
    assert_eq!(first.stopped, None);
    assert_eq!(second.stopped, None);
}

/// Blocker 3: if an earlier, different document answers a create (a colliding command id),
/// it is NOT counted as created.
#[test]
fn a_create_answered_with_another_document_is_not_counted_as_created() {
    let host = host();
    let wrapped = Wrapped::deduping(&host);
    let options = options(ApplyMode::Insert);
    let inv = invocation();
    // An earlier import stored "Acme" under the very key row 2 of this run derives.
    let key = super::write::command_text(&inv, 'c', "customer", &options.batch_label, 2);
    let mut earlier = inv.clone();
    earlier.command_id = Some(CommandId::new(&key).unwrap());
    let mut doc = Document::new(Name::new("customer").unwrap(), rid(70));
    doc.set(Name::new("name").unwrap(), Value::Text("Acme".into()));
    doc.set(Name::new("rating").unwrap(), Value::Text("A".into()));
    block_on(wrapped.create(&earlier, doc)).unwrap();

    let outcome = run(
        &wrapped,
        vec![customer_item(2, "Different", Some("B"))],
        &options,
    );
    let summary = outcome.summary();
    assert_eq!(summary.created, 0, "nothing was created");
    assert!(summary.created_drafts.is_empty());
    assert_eq!(
        entry(summary, crate::error::IDEMPOTENCY_CONFLICT)
            .unwrap()
            .row,
        2
    );
    assert_eq!(summary.stopped, Some(StopReason::WriteFailed));
}

/// Blocker 3: a real run refuses the empty label and the old default; a dry run does not
/// need one.
#[test]
fn a_real_run_needs_a_unique_label() {
    let host = host();
    for label in ["", "  ", "import"] {
        let mut options = options(ApplyMode::Insert);
        options.batch_label = label.to_owned();
        let outcome = run(&host, vec![customer_item(2, "Acme", Some("A"))], &options);
        assert!(
            matches!(outcome, ApplyOutcome::NothingWritten(_)),
            "label {label:?}"
        );
        let summary = outcome.summary();
        assert_eq!(summary.stopped, Some(StopReason::Configuration));
        assert!(entry(summary, crate::error::CONFIG_BATCH_LABEL).is_some());
        assert!(host.outbox().is_empty(), "nothing written");
    }
    let mut dry = options(ApplyMode::DryRun { key_field: None });
    dry.batch_label = String::new();
    let outcome = run(&host, vec![customer_item(2, "Acme", Some("A"))], &dry);
    assert_eq!(outcome.summary().created, 1);
    assert_eq!(outcome.summary().stopped, None);
}

/// A command id the engine would refuse is an error, never the caller's own id.
#[test]
fn a_refused_command_id_is_an_error() {
    let inv = invocation();
    let too_long = "x".repeat(200);
    assert!(super::write::invocation_with_id(&inv, &too_long).is_err());
    let ok = super::write::invocation_with_id(&inv, "impc-ok-1").unwrap();
    assert_eq!(ok.command_id.unwrap().as_str(), "impc-ok-1");
}

/// Blocker 4: a dry run reports create refusals as row errors and writes nothing.
#[test]
fn dry_run_reports_a_create_refusal() {
    let host = host();
    let mut options = options(ApplyMode::DryRun { key_field: None });
    options.permissions = Arc::new(Deny {
        create: true,
        update: None,
    });
    let outcome = run(
        &host,
        vec![
            customer_item(2, "One", Some("A")),
            customer_item(3, "Two", Some("B")),
        ],
        &options,
    );
    let summary = outcome.summary();
    assert!(summary.dry_run && summary.permissions_checked);
    assert_eq!(summary.created, 0, "refused rows would not be created");
    assert_eq!(summary.failed, 2);
    assert_eq!(
        summary
            .errors
            .iter()
            .filter(|error| error.code == "permission.denied")
            .count(),
        2
    );
    assert!(host.outbox().is_empty());
}

/// Blocker 4: a keyed dry run reports an update refusal too, and a dry run without a probe
/// says it could not check.
#[test]
fn dry_run_reports_an_update_refusal_and_admits_when_unchecked() {
    let host = host();
    let seeded = seed_customer(&host, 41, "Acme", "A");
    let index = MapIndex::default();
    index.add("customer", "name", Value::Text("Acme".into()), seeded.id());
    let index: Arc<dyn KeyIndex> = Arc::new(index);
    let mode = ApplyMode::DryRun {
        key_field: Some("name".to_owned()),
    };

    let mut options = options(mode.clone());
    options.index = Some(index.clone());
    options.permissions = Arc::new(Deny {
        create: false,
        update: Some(seeded.id()),
    });
    let outcome = run(&host, vec![customer_item(2, "Acme", Some("B"))], &options);
    let summary = outcome.summary();
    assert_eq!((summary.updated, summary.failed), (0, 1));
    assert_eq!(entry(summary, "permission.denied").unwrap().row, 2);

    let mut allowed = self::options(mode);
    allowed.index = Some(index);
    allowed.permissions = Arc::new(crate::apply::testing::AllowAll);
    let outcome = run(&host, vec![customer_item(2, "Acme", Some("B"))], &allowed);
    assert!(outcome.summary().permissions_checked);
    assert_eq!(outcome.summary().updated, 1);
}

/// Hardening: an index that names a document whose stored key has since changed must not make
/// the importer patch that document.
#[test]
fn a_stale_index_entry_never_patches_another_document() {
    let host = host();
    let other = seed_customer(&host, 51, "Zed", "A");
    let index = MapIndex::default();
    // The index still says "Acme" lives at the document that now holds "Zed".
    index.add("customer", "name", Value::Text("Acme".into()), other.id());
    let mut options = options(ApplyMode::Update {
        key_field: "name".to_owned(),
    });
    options.policy = ImportPolicy::AllOrNothing;
    options.batch_label = "stale-index".to_owned();
    options.index = Some(Arc::new(index));

    let outcome = run(&host, vec![customer_item(2, "Acme", Some("B"))], &options);
    assert!(matches!(outcome, ApplyOutcome::NothingWritten(_)));
    assert_eq!(entry(outcome.summary(), KEY_NOT_FOUND).unwrap().row, 2);
    assert_eq!(rating_of(&host, 51), Value::Text("A".into()), "untouched");
}

/// Recorded security finding: a real (not dry-run) insert with a refusing probe writes nothing.
#[test]
fn a_denying_probe_prevents_every_write() {
    let host = host();
    let mut options = options(ApplyMode::Insert);
    options.permissions = Arc::new(crate::apply::testing::DenyAll);
    let outcome = run(
        &host,
        vec![
            customer_item(2, "One", Some("A")),
            customer_item(3, "Two", Some("B")),
        ],
        &options,
    );
    let summary = outcome.summary();
    assert_eq!((summary.created, summary.failed), (0, 2));
    assert!(host.outbox().is_empty());
}

/// Recorded security finding: a probe that errors (it does not say "denied") fails closed with a stable code, and
/// nothing from the probe's error text reaches the report.
struct BrokenProbe;

impl PermissionProbe for BrokenProbe {
    fn check_create(&self, _inv: &Invocation, _doctype: &str) -> Result<(), AppError> {
        Err(AppError::new(
            spot_app_api::error::ErrorClass::Internal,
            "http.internal",
            "database password hunter2 unreachable",
        ))
    }

    fn check_update(
        &self,
        _inv: &Invocation,
        _doctype: &str,
        _id: RecordId,
    ) -> Result<(), AppError> {
        Err(AppError::new(
            spot_app_api::error::ErrorClass::Unavailable,
            "server.unavailable",
            "permission service down at 10.0.0.9",
        ))
    }
}

#[test]
fn a_probe_that_errors_fails_closed_with_a_stable_code() {
    let host = host();
    let mut options = options(ApplyMode::Insert);
    options.permissions = Arc::new(BrokenProbe);
    let outcome = run(&host, vec![customer_item(2, "One", Some("A"))], &options);
    let summary = outcome.summary();
    assert_eq!((summary.created, summary.failed), (0, 1));
    assert!(host.outbox().is_empty());
    let found = entry(summary, "permission.denied").unwrap();
    assert!(!found.message.contains("hunter2") && !found.message.contains("10.0.0.9"));
    assert!(summary.errors.iter().all(|e| e.code == "permission.denied"));
}
