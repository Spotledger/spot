# spot-l3-documents (WP-TST-10)

L3 **document behaviour suite**: 30 cases run over the [`spot-app-api`](../../../contracts/app-api)
document contract (ADR-057). Where `spot_app_api::contract_tests` (WP-CON-05) pins every rule in
isolation, this suite pins the **combinations** business code builds on — nested lifecycles,
amend chains, links seen through renames, child-table merges, posted documents, version
arithmetic, event emission and message collection.

The suite compiles against the host's **public API only** (`spot-app-api`; `serde_json` and
`tokio` are dev-dependencies), so it can never touch engine internals. Each case runs on a fresh
host and reports a pass/fail with a short message; `run_all` aggregates them into the same
`Report` shape as the contract suite.

## How a host runs the suite

1. Implement `spot_app_api::contract_tests::Fixture` — a host factory returning a fresh
   `Box<dyn TestHost>` (see `tests/stub.rs`, which wraps `spot_app_api::stub::StubHost`).
2. Call `spot_l3_documents::run_all(&fixture).await` and assert
   `report.assert_all_passed()`.
3. The host must already support everything ADR-057 fixes: documents with drafts and versions,
   post/cancel/correct, child rows, link fields, whitelisted methods, hook steps with
   overrides, seams, outbox events and messages.

`cargo test -p spot-l3-documents` runs the suite on the stub host.

## The cases

Every case cites the ADRs that fix the behaviour it checks; where ADR-51 (nested lifecycles) or
the amend chain (ADR-42) leave an ordering open, the case asserts only what is fixed.

### Nested lifecycles, amend chains, messages (ADR-51, ADR-42, ADR-055)

| Case | Pins |
| --- | --- |
| `nested_created_children_commit_together` | ADR-51: an `after_create` step creates a second document whose own `after_create` creates a third; all three commit with the outer transaction. |
| `nested_rollback_cancels_every_child_and_its_events_and_jobs` | ADR-51: a failure in the third document's `validate_save` rolls back all three documents and their events and jobs. |
| `committed_spawns_run_in_new_transactions_after_the_outer_commit` | ADR-51: a committed step that creates a document runs that document's full lifecycle (including its own committed steps) in its own transaction; the outer document's steps never re-run. |
| `nested_create_events_carry_the_outer_command_id` | ADR-51/ADR-056: events emitted inside a nested transaction carry the outer invocation's `command_id`. |
| `messages_from_nested_hooks_arrive_in_raised_order` | ADR-055: messages raised by nested hooks are returned by the outer operation, in the order they were raised. |
| `messages_from_hooks_do_not_block_commit` | ADR-055: raising a message does not fail or block the operation; the document commits. |
| `amend_correct_cancels_original_and_opens_draft` | ADR-42: correcting a posted document cancels it and opens a draft with `amended_from` pointing at it. |
| `amend_chain_each_amendment_cancels_the_posted_document` | ADR-42: original → A1 → A2; the original and A1 end cancelled, A2 is a draft pointing at A1. |
| `amend_correct_requires_a_posted_document` | ADR-42: correcting a draft or a cancelled document fails with `document.invalid_state`. |
| `amend_correct_with_stale_version_is_refused_and_nothing_cancelled` | ADR-42/ADR-38: correcting with a stale version fails with `document.stale_version` and cancels nothing. |### Links, child tables, posted documents (ADR-33, ADR-34, ADR-57)

| Case | Pins |
| --- | --- |
| `links_rename_target_keeps_draft_links_valid` | ADR-33/ADR-34: links are ids; renaming a customer leaves every link from draft invoices valid. |
| `links_renaming_a_posted_document_emits_doc_renamed` | ADR-056: renaming emits `doc_renamed` naming the document. |
| `links_delete_of_referenced_document_unblocks_after_references_gone` | ADR-33/ADR-34: deleting a customer referenced by a draft invoice fails with `document.linked`; after the invoice is deleted it succeeds. |
| `links_link_created_in_the_same_transaction_and_kept` | ADR-33/ADR-51: a link to a document created earlier in the same transaction is valid. |
| `tables_child_table_reorder_keeps_row_ids_and_values` | ADR-34: a patch that reorders rows changes row order; ids stay on the rows that kept their values. |
| `tables_child_table_insert_and_delete_rows` | ADR-34: a patch that inserts and deletes rows lands rows in patch order and drops the deleted rows. |
| `tables_row_errors_name_rows_by_their_new_position` | ADR-34: a row validation error names the row by its **new** position (`items[2].qty`). |
| `tables_table_only_changes_list_the_table_in_changed_fields` | ADR-056: when only child rows changed, `doc_updated`'s changed list names the table and nothing else. |
| `posted_updates_change_only_fields_allowed_on_posted_documents` | ADR-11/ADR-57: an update of a posted document mixing an allowed and a read-only field is rejected whole; only allowed fields change otherwise. |
| `posted_updates_emit_doc_updated_posted` | ADR-056: a posted-document update emits `doc_updated_posted` (not `doc_updated`), and the state stays posted. |
| `posted_renaming_a_posted_document_keeps_it_posted` | ADR-57: renaming a posted document keeps it posted. |
| `posted_and_cancelled_documents_cannot_be_deleted` | ADR-57: posted and cancelled documents refuse `delete` with `document.invalid_state`. |

### Versions, events, methods, overrides, seams (ADR-38, ADR-056, ADR-16, ADR-08)

| Case | Pins |
| --- | --- |
| `versions_increment_by_one_per_write` | ADR-38: the version increases by exactly 1 per successful write, including `post`, `cancel` and `rename`. |
| `versions_second_update_stale` | ADR-38: two sequential updates from the same starting version — the second fails with `document.stale_version` and `current_version` equal to the first's result. |
| `events_every_successful_operation_emits_exactly_one_lifecycle_event` | ADR-056: every successful operation emits exactly one lifecycle event whose `subject_version` equals the version returned. |
| `events_failed_operations_emit_no_events` | ADR-056: failed operations emit no events and change nothing. |
| `events_lifecycle_data_matches_the_topic_schema` | ADR-056: every event's data validates against the topic's `Operation::schema()`. |
| `methods_not_on_the_whitelist_fail` | ADR-16: unknown methods, methods on the wrong doctype and doctypes without a controller fail with `method.not_found`. |
| `methods_whitelisted_method_runs_and_returns_json` | ADR-16: a whitelisted method runs on the loaded document and returns its JSON, with messages on the invocation. |
| `overrides_change_steps_and_seams_round_trip_json` | ADR-08: overrides replace, disable and wrap scheduled steps exactly as declared, seams round-trip JSON, and a missing seam fails with `method.not_found`. |