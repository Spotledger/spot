//! L3 document behaviour suite (WP-TST-10.T1).
//!
//! A contract-level Rust test suite over the `spot_app_api` document contract (ADR-057). Where
//! the WP-CON-05 suite in `spot_app_api::contract_tests` pins single operations in isolation,
//! this crate pins behaviour across combinations of operations on the same documents: nested
//! lifecycles started from in-transaction and committed hooks (ADR-51), multi-level amend
//! chains (ADR-42), link targets seen through renames, child-table merges, posted-document
//! rules, version arithmetic, event emission (ADR-056) and message collection.
//!
//! Every case is a [`CaseResult`] function over a [`Fixture`] — a host factory, as defined by
//! `spot_app_api::contract_tests` — so [`run_all`] works with the stub host (`tests/stub.rs`)
//! or with any later backend host. Each case starts on a fresh host and records what it
//! observes; each case cites the decision records it enforces in its doc comment, in the case
//! list below and in `README.md`.

use spot_app_api::BoxFuture;
use spot_app_api::contract_tests::{CaseOutcome, CaseResult, Fixture, Report};

mod cases_doc;
mod cases_flow;
mod cases_ops;

/// Helpers shared by the case modules: invocation and value builders, a step log,
/// pre-minted record ids, hook closures and a small controller with hand-written hook
/// steps. The `contract_tests` module keeps its own copies of most of these private; nothing
/// here is public API.
pub(crate) mod shared {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Value as Json;

    use spot_app_api::context::{Actor, Invocation, MethodContext};
    use spot_app_api::contract_tests::{CaseResult, Failure, TestHost, catalog};
    use spot_app_api::controller::{Controller, MethodDecl, StepDecl, StepName};
    use spot_app_api::error::{AppError, AppResult};
    use spot_app_api::prelude::{
        CommandId, Decimal, Name, OffsetDateTime, RecordId, SiteId, Value, Version,
    };
    use spot_app_api::{BoxFuture, ChildRow, Document, ErrorClass, HookContext, HookPoint};

    /// A hook step's closure, as a controller keeps it.
    pub(crate) type Step =
        Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

    /// A whitelisted method's closure.
    pub(crate) type Call = Arc<
        dyn for<'a> Fn(&'a str, Json, &'a mut dyn MethodContext) -> BoxFuture<'a, AppResult<Json>>
            + Send
            + Sync,
    >;

    /// Parses a storage-style name (doctype, field), failing the process on a typo (tests
    /// only).
    pub(crate) fn n(name: &str) -> Name {
        Name::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    /// A parsed step name (test-only).
    pub(crate) fn step(name: &str) -> StepName {
        StepName::new(name).unwrap_or_else(|e| panic!("invalid step name {name:?}: {e}"))
    }

    /// An invocation (`stub.test`, fixed command id), one per case.
    pub(crate) fn inv(command_id: &str) -> Invocation {
        Invocation {
            site: SiteId::new("stub.test").unwrap_or_else(|e| panic!("{e}")),
            actor: Actor::System {
                component: n("l3_documents"),
            },
            command_id: CommandId::new(command_id).ok(),
            language: None,
            now: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// A decimal value (test-only).
    pub(crate) fn dec(text: &str) -> Value {
        Value::Decimal(text.parse::<Decimal>().unwrap_or_else(|e| panic!("{e}")))
    }

    /// A text value (test-only).
    pub(crate) fn text(text: &str) -> Value {
        Value::Text(text.to_owned())
    }

    /// A stored document's version; the first version when it has none yet.
    pub(crate) fn cur(document: &Document) -> Version {
        document.version().unwrap_or(Version::FIRST)
    }

    /// A case assertion that does not panic mid-case (the runner keeps going).
    pub(crate) fn check(condition: bool, what: impl Into<String>) -> CaseResult {
        if condition {
            Ok(())
        } else {
            Err(Failure(what.into()))
        }
    }

    /// Expects `result` to fail with exactly `code`, back for further assertions.
    pub(crate) fn expect_err<T>(result: AppResult<T>, code: &str) -> Result<AppError, Failure> {
        match result {
            Ok(_) => Err(Failure(format!("expected error {code}, got a result"))),
            Err(e) => {
                let matches = e.code == code || e.fields.iter().any(|f| f.code == code);
                if matches {
                    Ok(e)
                } else {
                    let fields: Vec<String> = e
                        .fields
                        .iter()
                        .map(|f| format!("{}={}", f.field.as_str(), f.code))
                        .collect();
                    Err(Failure(format!(
                        "expected error {code}, got {} with fields {fields:?}",
                        e.code
                    )))
                }
            }
        }
    }

    /// Step labels a case has seen so far, in order.
    pub(crate) type Log = Arc<Mutex<Vec<String>>>;

    /// Adds a label to the log.
    pub(crate) fn push(log: &Log, label: impl Into<String>) {
        log.lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(label.into());
    }

    /// The log's labels.
    pub(crate) fn read(log: &Log) -> Vec<String> {
        log.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// How many log entries contain `label`.
    pub(crate) fn occurrences(log: &Log, label: &str) -> usize {
        read(log).iter().filter(|e| e.as_str() == label).count()
    }

    /// Record ids pre-minted with [`ids`], handed to nested steps.
    pub(crate) type Ids = Arc<Mutex<Vec<RecordId>>>;

    /// Pre-mints `count` fresh ids for steps that create documents later.
    pub(crate) fn ids(host: &dyn TestHost, count: usize) -> Ids {
        Arc::new(Mutex::new((0..count).map(|_| host.fresh_id()).collect()))
    }

    /// Takes the next pre-minted id, if any remain.
    pub(crate) fn take(ids: &Ids) -> AppResult<RecordId> {
        ids.lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop()
            .ok_or_else(|| {
                AppError::new(
                    ErrorClass::Internal,
                    "l3.out_of_ids",
                    "a nested step ran more often than expected",
                )
            })
    }

    /// Document ids created by hook steps, with a case-local label.
    pub(crate) type Created = Arc<Mutex<Vec<(String, RecordId)>>>;

    /// Records that a hook created a document under label.
    pub(crate) fn note_created(created: &Created, label: &str, id: RecordId) {
        created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((label.to_owned(), id));
    }

    /// Every id recorded under `label`.
    pub(crate) fn created_of(created: &Created, label: &str) -> Vec<RecordId> {
        created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(l, _)| l.as_str() == label)
            .map(|(_, id)| *id)
            .collect()
    }

    /// A hook step that records `<prefix>:<point name>` and does nothing else.
    pub(crate) fn recorder(log: Log, prefix: &'static str) -> Step {
        Arc::new(move |ctx: &mut dyn HookContext| {
            let log = log.clone();
            Box::pin(async move {
                push(&log, format!("{prefix}:{}", ctx.point().name()));
                Ok(())
            })
        })
    }

    /// A hook step from a hand-written closure.
    pub(crate) fn hook_fn<F>(f: F) -> Step
    where
        F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>>
            + Send
            + Sync
            + 'static,
    {
        Arc::new(f)
    }

    /// A stored draft customer.
    pub(crate) async fn new_customer(host: &dyn TestHost, name: &str) -> Result<Document, Failure> {
        let mut d = Document::new(n("customer"), host.fresh_id());
        d.set(n("customer_name"), text(name));
        Ok(host.create(&inv("l3:new"), d).await?.value)
    }

    /// A stored draft invoice linked to `customer`, with `items` rows `(item, qty)`.
    pub(crate) async fn new_invoice(
        host: &dyn TestHost,
        customer: RecordId,
        items: &[(&str, &str)],
    ) -> Result<Document, Failure> {
        let mut d = Document::new(n("invoice"), host.fresh_id());
        d.set(n("customer"), Value::Id(customer));
        d.set(n("number"), text("INV-1"));
        for (item, qty) in items {
            d.rows_mut(n("items")).push(ChildRow {
                id: host.fresh_id(),
                fields: BTreeMap::from([(n("item"), text(item)), (n("qty"), dec(qty))]),
            });
        }
        Ok(host.create(&inv("l3:new"), d).await?.value)
    }

    /// A controller whose hook steps are hand-written closures, for tests. Steps run through
    /// [`Controller::run_step`], which dispatches on the recorded name.
    pub(crate) struct Hooked {
        doctype: String,
        steps: Vec<(StepName, HookPoint, Step)>,
        methods: Option<Vec<(Name, bool)>>,
        call: Option<Call>,
    }

    impl Hooked {
        pub(crate) fn new(doctype: &str) -> Self {
            Self {
                doctype: doctype.to_owned(),
                steps: Vec::new(),
                methods: None,
                call: None,
            }
        }

        /// Adds a hook step.
        pub(crate) fn hook(self, name: StepName, point: HookPoint, f: Step) -> Self {
            let mut this = self;
            this.steps.push((name, point, f));
            this
        }

        /// Declares whitelisted methods with one handler for all of them.
        pub(crate) fn method(mut self, name: &str, mutates: bool, call: Call) -> Self {
            self.methods
                .get_or_insert_with(Vec::new)
                .push((n(name), mutates));
            self.call = Some(call);
            self
        }
    }

    impl Controller for Hooked {
        fn doctype(&self) -> &str {
            &self.doctype
        }

        fn steps(&self) -> Vec<StepDecl> {
            self.steps
                .iter()
                .map(|(name, point, _)| StepDecl {
                    name: name.clone(),
                    point: point.clone(),
                    priority: 0,
                    budget: Duration::from_secs(60),
                })
                .collect()
        }

        fn run_step<'a>(
            &'a self,
            step: &'a StepName,
            ctx: &'a mut dyn HookContext,
        ) -> BoxFuture<'a, AppResult<()>> {
            let f = self
                .steps
                .iter()
                .find(|(name, _, _)| name == step)
                .map(|(_, _, f)| f.clone());
            Box::pin(async move {
                match f {
                    Some(f) => f(ctx).await,
                    None => Err(AppError::new(
                        ErrorClass::Internal,
                        "platform.invalid_step_name",
                        format!("no step {}", step.as_str()),
                    )),
                }
            })
        }

        fn methods(&self) -> Vec<MethodDecl> {
            self.methods
                .iter()
                .flatten()
                .map(|(name, mutates)| MethodDecl::new(name.clone(), *mutates))
                .collect()
        }

        fn call_method<'a>(
            &'a self,
            method: &'a str,
            args: Json,
            ctx: &'a mut dyn MethodContext,
        ) -> BoxFuture<'a, AppResult<Json>> {
            match &self.call {
                Some(call) => {
                    let call = call.clone();
                    Box::pin(async move { call(method, args, ctx).await })
                }
                None => {
                    let _ = ctx;
                    Box::pin(async move {
                        Err(AppError::new(
                            ErrorClass::MethodNotFound,
                            "method.not_found",
                            format!("no method {method}"),
                        ))
                    })
                }
            }
        }
    }

    /// Defines the shared doctypes from the WP-CON-05 catalog on `host`.
    pub(crate) fn define_catalog(host: &dyn TestHost) {
        for definition in catalog() {
            host.define(definition);
        }
    }
}

use cases_doc as doc;
use cases_flow as flow;
use cases_ops as ops;

/// A case: its name as shown by the runner, and its runner.
type CaseEntry = (&'static str, fn(&dyn Fixture) -> BoxFuture<'_, CaseResult>);

/// The suite: 30 cases in three groups. Nested lifecycle and amend cases (ADR-51, ADR-42,
/// ADR-055); document data cases for links, child tables and posted documents (ADR-33,
/// ADR-34); and version, event, method, override and seam cases (ADR-38, ADR-056, ADR-16,
/// ADR-08). Every case is documented beside its implementation in the `cases_*` modules and
/// in `README.md`.
static CASES: [CaseEntry; 30] = [
    // Nested lifecycles (ADR-51), amend chains (ADR-42), messages.
    (
        "nested_created_children_commit_together",
        flow::nested_created_children_commit_together,
    ),
    (
        "nested_rollback_cancels_every_child_and_its_events_and_jobs",
        flow::nested_rollback_cancels_every_child_and_its_events_and_jobs,
    ),
    (
        "committed_spawns_run_in_new_transactions_after_the_outer_commit",
        flow::committed_spawns_run_in_new_transactions_after_the_outer_commit,
    ),
    (
        "nested_create_events_carry_the_outer_command_id",
        flow::nested_create_events_carry_the_outer_command_id,
    ),
    (
        "messages_from_nested_hooks_arrive_in_raised_order",
        flow::messages_from_nested_hooks_arrive_in_raised_order,
    ),
    (
        "amend_correct_cancels_original_and_opens_draft",
        flow::amend_correct_cancels_original_and_opens_draft,
    ),
    (
        "amend_chain_each_amendment_cancels_the_posted_document",
        flow::amend_chain_each_amendment_cancels_the_posted_document,
    ),
    (
        "amend_correct_requires_a_posted_document",
        flow::amend_correct_requires_a_posted_document,
    ),
    (
        "amend_correct_with_stale_version_is_refused_and_nothing_cancelled",
        flow::amend_correct_with_stale_version_is_refused_and_nothing_cancelled,
    ),
    (
        "messages_from_hooks_do_not_block_commit",
        flow::messages_from_hooks_do_not_block_commit,
    ),
    // Links, child tables, posted documents (ADR-33, ADR-34, ADR-57).
    (
        "links_rename_target_keeps_draft_links_valid",
        doc::links_rename_target_keeps_draft_links_valid,
    ),
    (
        "links_renaming_a_posted_document_emits_doc_renamed",
        doc::links_renaming_a_posted_document_emits_doc_renamed,
    ),
    (
        "links_delete_of_referenced_document_unblocks_after_references_gone",
        doc::links_delete_of_referenced_document_unblocks_after_references_gone,
    ),
    (
        "links_link_created_in_the_same_transaction_and_kept",
        doc::links_link_created_in_the_same_transaction_and_kept,
    ),
    (
        "tables_child_table_reorder_keeps_row_ids_and_values",
        doc::tables_child_table_reorder_keeps_row_ids_and_values,
    ),
    (
        "tables_child_table_insert_and_delete_rows",
        doc::tables_child_table_insert_and_delete_rows,
    ),
    (
        "tables_row_errors_name_rows_by_their_new_position",
        doc::tables_row_errors_name_rows_by_their_new_position,
    ),
    (
        "tables_table_only_changes_list_the_table_in_changed_fields",
        doc::tables_table_only_changes_list_the_table_in_changed_fields,
    ),
    (
        "posted_updates_change_only_fields_allowed_on_posted_documents",
        doc::posted_updates_change_only_fields_allowed_on_posted_documents,
    ),
    (
        "posted_updates_emit_doc_updated_posted",
        doc::posted_updates_emit_doc_updated_posted,
    ),
    (
        "posted_renaming_a_posted_document_keeps_it_posted",
        doc::posted_renaming_a_posted_document_keeps_it_posted,
    ),
    (
        "posted_and_cancelled_documents_cannot_be_deleted",
        doc::posted_and_cancelled_documents_cannot_be_deleted,
    ),
    // Versions, events, methods, overrides, seams (ADR-38, ADR-056, ADR-16, ADR-08).
    (
        "versions_increment_by_one_per_write",
        ops::versions_increment_by_one_per_write,
    ),
    (
        "versions_second_update_stale",
        ops::versions_second_update_stale,
    ),
    (
        "events_every_successful_operation_emits_exactly_one_lifecycle_event",
        ops::events_every_successful_operation_emits_exactly_one_lifecycle_event,
    ),
    (
        "events_failed_operations_emit_no_events",
        ops::events_failed_operations_emit_no_events,
    ),
    (
        "events_lifecycle_data_matches_the_topic_schema",
        ops::events_lifecycle_data_matches_the_topic_schema,
    ),
    (
        "methods_not_on_the_whitelist_fail",
        ops::methods_not_on_the_whitelist_fail,
    ),
    (
        "methods_whitelisted_method_runs_and_returns_json",
        ops::methods_whitelisted_method_runs_and_returns_json,
    ),
    (
        "overrides_change_steps_and_seams_round_trip_json",
        ops::overrides_change_steps_and_seams_round_trip_json,
    ),
];

/// Runs every case on fresh hosts from `fixture` and reports each result.
pub async fn run_all(fixture: &dyn Fixture) -> Report {
    let mut outcomes = Vec::new();
    for (name, case) in CASES {
        outcomes.push(CaseOutcome {
            case: name,
            result: case(fixture).await,
        });
    }
    Report { outcomes }
}
