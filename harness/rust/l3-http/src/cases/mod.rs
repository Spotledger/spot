//! One behaviour per case, named like the L3 suite (`l3-documents`).
//!
//! Every case runs against the same [`Fixture`]: one database, one site, one host. A case is
//! independent of the others (it creates its own documents and restores anything it changes),
//! so a failure names one behaviour and leaves the rest of the run meaningful.
//!
//! A case that the stack cannot serve yet is **blocked**: it names the work package that owns
//! the missing surface and the observation that proves it is missing. Blocked is not a pass and
//! not a failure; it is the distance between the brief and the product, visible in every run.

mod auth;
mod documents;
mod edge;
mod lifecycle;
mod lists;
mod permissions;

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::fixture::{Fixture, INVITED};
use crate::report::{CaseResult, Filter, Outcome, Report, Status};
use crate::{ensure, failure};

pub use self::auth::*;
pub use self::documents::*;
pub use self::edge::*;
pub use self::lifecycle::*;
pub use self::lists::*;
pub use self::permissions::*;

/// Runs every admitted case, in order, and records one outcome per case.
///
/// The order is deliberate: the password and lockout cases finish before the permission cases
/// sign in, the metadata change comes before the cases that read metadata, and the shutdown case
/// is last because it stops the host.
pub async fn run_all(fixture: &Fixture, filter: Filter) -> Report {
    let mut report = Report::new();
    macro_rules! case {
        ($case:ident, $core:expr) => {{
            if filter.admits($core) {
                let started = Instant::now();
                let status = match $case(fixture).await {
                    Ok(()) => Status::Passed,
                    Err(failure) => Status::Failed(failure),
                };
                report.push(Outcome {
                    case: stringify!($case),
                    status,
                    duration: started.elapsed(),
                    core: $core,
                });
            }
        }};
    }
    // A blocked entry of the hand-written block: its owner and reason come from `LEGACY_BLOCKED`,
    // the same table `CASES.md` is generated from, so there is one source.
    macro_rules! blocked {
        ($name:literal) => {{
            if filter.admits(false) {
                let status = match LEGACY_BLOCKED.iter().find(|(name, ..)| *name == $name) {
                    Some((_, owner, reason)) => Status::Blocked { owner, reason },
                    None => Status::Failed(failure!(
                        "the blocked entry `{}` has no row in LEGACY_BLOCKED",
                        $name
                    )),
                };
                report.push(Outcome {
                    case: $name,
                    status,
                    duration: Duration::ZERO,
                    core: false,
                });
            }
        }};
    }

    // Authentication and sessions.
    case!(sign_in_returns_a_session_and_the_user, true);
    case!(sign_out_ends_the_session, true);
    case!(session_reports_the_signed_in_user, false);
    case!(the_first_sign_in_secret_needs_a_password_change, false);
    case!(an_unauthenticated_request_never_reaches_the_engine, true);
    case!(a_password_change_keeps_the_session, false);

    // Documents: CRUD, children, versions.
    case!(create_every_sample_doctype, true);
    case!(a_child_table_round_trips_with_its_rows, false);
    case!(a_table_multiselect_round_trips, false);
    case!(a_dynamic_link_names_its_target_per_document, false);
    blocked!("a_dynamic_link_refuses_a_target_of_another_type");
    blocked!("a_read_only_field_refuses_a_user_value");
    case!(an_unknown_field_is_refused, false);
    case!(a_unique_field_refuses_a_duplicate, false);
    case!(a_stale_version_is_a_conflict, true);
    case!(a_patch_without_if_match_is_refused, false);
    case!(a_delete_that_a_link_blocks_lists_its_referrers, false);

    // Lifecycle and the roll-up.
    blocked!("submit_a_time_entry_rolls_hours_up");
    blocked!("cancel_a_time_entry_rolls_hours_down");
    case!(correct_a_draft_amends_the_posted_entry, true);
    case!(a_posted_document_refuses_a_plain_edit, false);
    blocked!("an_hours_value_outside_the_range_is_refused");

    // Lists, filters, paging.
    blocked!("list_returns_the_default_columns_newest_first");
    case!(a_query_filters_by_equality_and_range, true);
    case!(a_query_sorts_by_two_keys_and_the_id, false);
    case!(a_query_pages_with_a_cursor, true);
    case!(a_query_reports_its_total_when_asked, false);
    case!(a_query_projects_only_the_requested_fields, false);
    case!(
        the_permission_filtered_list_equals_a_per_document_check,
        true
    );

    // Permissions and field security.
    case!(the_viewer_reads_but_cannot_create_or_write, true);
    case!(
        the_member_writes_tasks_and_only_their_own_time_entries,
        true
    );
    case!(the_manager_holds_every_right, false);
    case!(the_permission_summary_matches_enforcement, false);
    case!(field_masking_hides_the_confidential_details, false);
    blocked!("a_reveal_without_the_group_right_is_refused");

    // Metadata, idempotency, the HTTP edge.
    case!(meta_describes_every_field_and_its_etag, true);
    case!(a_metadata_change_is_visible_without_a_restart, false);
    case!(an_idempotency_key_returns_the_same_document, true);
    case!(the_request_id_round_trips, false);
    case!(the_health_endpoints_answer, true);
    case!(an_unknown_site_is_a_404_envelope, false);
    case!(a_body_over_the_limit_is_a_413_envelope, false);

    // The sign-in failures run last: the throttle counts per source, so an isolated loopback
    // source is used and no later case needs to sign in.
    case!(wrong_password_is_refused_without_saying_which_part, true);
    case!(a_throttled_sign_in_reports_retry_after, false);
    blocked!("a_throttled_sign_in_sets_the_retry_after_header");
    blocked!("repeated_wrong_passwords_lock_the_account");
    blocked!("a_locked_account_reports_its_retry_after");

    case!(a_shutdown_drains_the_requests_in_flight, false);

    // Surfaces the contract names but the host does not serve yet.
    blocked!("files: upload_then_download_with_a_range");
    blocked!("files: an_attached_file_is_not_an_orphan");
    blocked!("realtime: a_permitted_client_is_notified_and_another_is_not");
    blocked!("meta: a_runtime_created_doctype_is_usable_at_once");
    blocked!("naming: a_series_is_gapless_under_50_concurrent_creates");

    // The registered cases (`src/cases/<name>.rs`, found by `build.rs`), after the hand-written
    // block, ordered by `(ORDER, name)`.
    for def in registered() {
        match def.kind {
            CaseKind::Run(run) => {
                if filter.admits(def.core) {
                    let started = Instant::now();
                    let status = match run(fixture).await {
                        Ok(()) => Status::Passed,
                        Err(failure) => Status::Failed(failure),
                    };
                    report.push(Outcome {
                        case: def.name,
                        status,
                        duration: started.elapsed(),
                        core: def.core,
                    });
                }
            }
            CaseKind::Blocked { owner, reason } => {
                if filter.admits(false) {
                    report.push(Outcome {
                        case: def.name,
                        status: Status::Blocked { owner, reason },
                        duration: Duration::ZERO,
                        core: false,
                    });
                }
            }
        }
    }

    report
}

// ---------------------------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------------------------

/// The future a registered case's run function returns.
pub type CaseFuture<'a> = Pin<Box<dyn Future<Output = CaseResult> + Send + 'a>>;

/// A registered case's run function: an async fn over the fixture, boxed
/// (`|fixture| Box::pin(my_case(fixture))`).
pub type RunFn = for<'a> fn(&'a Fixture) -> CaseFuture<'a>;

/// What a registered case does.
#[derive(Clone, Copy)]
pub enum CaseKind {
    /// Runs against the fixture.
    Run(RunFn),
    /// The surface it needs is not served yet; `owner` is the task that adds it.
    Blocked {
        /// The task that owns the missing surface.
        owner: &'static str,
        /// What is missing, in one sentence.
        reason: &'static str,
    },
}

/// One registered case: a file `src/cases/<name>.rs` that declares `pub const ORDER: u32` and
/// `pub fn cases() -> Vec<CaseDef>` is found by `build.rs` and needs no edit here.
#[derive(Clone, Copy)]
pub struct CaseDef {
    /// The case's name, as the report and `CASES.md` show it.
    pub name: &'static str,
    /// A heading of `CASES.md`; the case is listed under it.
    pub section: &'static str,
    /// What the case pins, on one line.
    pub pins: &'static str,
    /// Whether the case is in the quick (pull request) subset.
    pub core: bool,
    /// What the case does.
    pub kind: CaseKind,
}

impl CaseDef {
    /// A case that runs.
    pub const fn run(
        name: &'static str,
        section: &'static str,
        pins: &'static str,
        core: bool,
        run: RunFn,
    ) -> Self {
        Self {
            name,
            section,
            pins,
            core,
            kind: CaseKind::Run(run),
        }
    }

    /// A case the stack cannot serve yet. It is never in the quick subset.
    pub const fn blocked(
        name: &'static str,
        section: &'static str,
        pins: &'static str,
        owner: &'static str,
        reason: &'static str,
    ) -> Self {
        Self {
            name,
            section,
            pins,
            core: false,
            kind: CaseKind::Blocked { owner, reason },
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/cases_registry.rs"));

/// One row of the index for a case that predates the registry.
#[derive(Debug, Clone, Copy)]
pub struct LegacyRow {
    /// The case's name.
    pub name: &'static str,
    /// A heading of `CASES.md`.
    pub section: &'static str,
    /// What the case pins, on one line.
    pub pins: &'static str,
}

/// The 53 cases that predate the registry, in `CASES.md` order: name, section, pins. A case that is
/// blocked has its owner and reason in [`LEGACY_BLOCKED`] (one source for the run and the file).
/// New cases do not add rows here: they live in their own `src/cases/<name>.rs` (see `CaseDef`).
pub const LEGACY: &[LegacyRow] = &[
    LegacyRow {
        name: "sign_in_returns_a_session_and_the_user",
        section: "Authentication and sessions",
        pins: "`POST /auth/login` → token, expiry, identity",
    },
    LegacyRow {
        name: "sign_out_ends_the_session",
        section: "Authentication and sessions",
        pins: "`POST /auth/logout` → the token stops working (`auth.session_expired`)",
    },
    LegacyRow {
        name: "session_reports_the_signed_in_user",
        section: "Authentication and sessions",
        pins: "`GET /auth/session` → the same identity and site",
    },
    LegacyRow {
        name: "wrong_password_is_refused_without_saying_which_part",
        section: "Authentication and sessions",
        pins: "a wrong password and an unknown user answer the same 401 `auth.invalid_credentials`",
    },
    LegacyRow {
        name: "an_unauthenticated_request_never_reaches_the_engine",
        section: "Authentication and sessions",
        pins: "401 `auth.required`, `Running::engine_ops()` does not move",
    },
    LegacyRow {
        name: "a_password_change_keeps_the_session",
        section: "Authentication and sessions",
        pins: "the token that changed the password still answers afterwards",
    },
    LegacyRow {
        name: "the_first_sign_in_secret_needs_a_password_change",
        section: "Authentication and sessions",
        pins: "`spot add-user`'s printed secret signs in (and is refused as a bearer key); until the password is chosen only read-session and sign-out answer besides the change, everything else is 403 `cli.password_change_required`; the change replaces the session and spends the secret",
    },
    LegacyRow {
        name: "repeated_wrong_passwords_lock_the_account",
        section: "Authentication and sessions",
        pins: "the throttle locks after `lockout_after` failures",
    },
    LegacyRow {
        name: "a_locked_account_reports_its_retry_after",
        section: "Authentication and sessions",
        pins: "the 429 carries `details.retry_after`",
    },
    LegacyRow {
        name: "a_throttled_sign_in_reports_retry_after",
        section: "Authentication and sessions",
        pins: "two failures from one source → 429 `api.rate_limited` with a positive `details.retry_after`",
    },
    LegacyRow {
        name: "a_throttled_sign_in_sets_the_retry_after_header",
        section: "Authentication and sessions",
        pins: "the same 429 carries the registry's promised `Retry-After` header",
    },
    LegacyRow {
        name: "create_every_sample_doctype",
        section: "Documents: CRUD, children, versions",
        pins: "project, task, time_entry, label, child rows, `Location`",
    },
    LegacyRow {
        name: "a_child_table_round_trips_with_its_rows",
        section: "Documents: CRUD, children, versions",
        pins: "`task.checklist`: insert, edit by row id, delete, order",
    },
    LegacyRow {
        name: "a_table_multiselect_round_trips",
        section: "Documents: CRUD, children, versions",
        pins: "`project.labels` picks, including a reorder",
    },
    LegacyRow {
        name: "a_dynamic_link_names_its_target_per_document",
        section: "Documents: CRUD, children, versions",
        pins: "`task.related_type` + `related_to` over project and task",
    },
    LegacyRow {
        name: "a_dynamic_link_refuses_a_target_of_another_type",
        section: "Documents: CRUD, children, versions",
        pins: "`related_type = project` with a Task id in `related_to` is refused",
    },
    LegacyRow {
        name: "an_unknown_field_is_refused",
        section: "Documents: CRUD, children, versions",
        pins: "`document.unknown_field` names the field",
    },
    LegacyRow {
        name: "a_unique_field_refuses_a_duplicate",
        section: "Documents: CRUD, children, versions",
        pins: "two projects with one `code`; the first survives",
    },
    LegacyRow {
        name: "a_stale_version_is_a_conflict",
        section: "Documents: CRUD, children, versions",
        pins: "`PATCH` with the previous `If-Match` → 412 `document.stale_version` + `current_version`",
    },
    LegacyRow {
        name: "a_patch_without_if_match_is_refused",
        section: "Documents: CRUD, children, versions",
        pins: "428 `api.precondition_required`",
    },
    LegacyRow {
        name: "a_delete_that_a_link_blocks_lists_its_referrers",
        section: "Documents: CRUD, children, versions",
        pins: "`DELETE` a project with tasks → 409 `document.linked`",
    },
    LegacyRow {
        name: "a_read_only_field_refuses_a_user_value",
        section: "Documents: CRUD, children, versions",
        pins: "`task.logged_hours` from a client is refused",
    },
    LegacyRow {
        name: "correct_a_draft_amends_the_posted_entry",
        section: "Lifecycle",
        pins: "`POST .../correct` → a new draft linked by `amended_from`, the original cancelled",
    },
    LegacyRow {
        name: "a_posted_document_refuses_a_plain_edit",
        section: "Lifecycle",
        pins: "`PATCH` on a posted entry → 422, field code `document.read_only_field`",
    },
    LegacyRow {
        name: "submit_a_time_entry_rolls_hours_up",
        section: "Lifecycle",
        pins: "`POST .../post` grows `task.logged_hours`",
    },
    LegacyRow {
        name: "cancel_a_time_entry_rolls_hours_down",
        section: "Lifecycle",
        pins: "`POST .../cancel` subtracts the hours",
    },
    LegacyRow {
        name: "an_hours_value_outside_the_range_is_refused",
        section: "Lifecycle",
        pins: "the sample controller's `validate_save` (0.25–24)",
    },
    LegacyRow {
        name: "a_query_filters_by_equality_and_range",
        section: "Lists, filters, paging",
        pins: "`prefix` + `eq` + `ge` in one `and`",
    },
    LegacyRow {
        name: "a_query_sorts_by_two_keys_and_the_id",
        section: "Lists, filters, paging",
        pins: "status ascending, title descending, stable",
    },
    LegacyRow {
        name: "a_query_pages_with_a_cursor",
        section: "Lists, filters, paging",
        pins: "`limit` + `cursor` walks every row exactly once, `next_cursor: null` at the end",
    },
    LegacyRow {
        name: "a_query_reports_its_total_when_asked",
        section: "Lists, filters, paging",
        pins: "`with_total` reports matches, not page size",
    },
    LegacyRow {
        name: "a_query_projects_only_the_requested_fields",
        section: "Lists, filters, paging",
        pins: "`fields` plus `id` and `version`",
    },
    LegacyRow {
        name: "the_permission_filtered_list_equals_a_per_document_check",
        section: "Lists, filters, paging",
        pins: "every listed row is readable; a denied DocType is absent",
    },
    LegacyRow {
        name: "list_returns_the_default_columns_newest_first",
        section: "Lists, filters, paging",
        pins: "`GET /resource/{dt}` is newest first (the contract)",
    },
    LegacyRow {
        name: "the_viewer_reads_but_cannot_create_or_write",
        section: "Permissions and field security",
        pins: "Viewer: `permission.denied` on create/write/delete",
    },
    LegacyRow {
        name: "the_member_writes_tasks_and_only_their_own_time_entries",
        section: "Permissions and field security",
        pins: "`if_owner` on Time Entry write",
    },
    LegacyRow {
        name: "the_manager_holds_every_right",
        section: "Permissions and field security",
        pins: "Manager matrix over all five DocTypes",
    },
    LegacyRow {
        name: "the_permission_summary_matches_enforcement",
        section: "Permissions and field security",
        pins: "`GET /auth/permissions` says no create → the request is denied",
    },
    LegacyRow {
        name: "field_masking_hides_the_confidential_details",
        section: "Permissions and field security",
        pins: "`task.details` is hidden from a Viewer, readable by a Manager; the summary names it",
    },
    LegacyRow {
        name: "a_reveal_without_the_group_right_is_refused",
        section: "Permissions and field security",
        pins: "`POST .../reveal` → 403 `permission.reveal_denied`",
    },
    LegacyRow {
        name: "meta_describes_every_field_and_its_etag",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "`GET /meta/{dt}`, `If-None-Match` → 304",
    },
    LegacyRow {
        name: "a_metadata_change_is_visible_without_a_restart",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "add a field to the site's copy, `spot reload`, the field is in `meta` and writable",
    },
    LegacyRow {
        name: "an_idempotency_key_returns_the_same_document",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "the same key and body → one document, `Idempotency-Replayed`, the same `Location`",
    },
    LegacyRow {
        name: "the_request_id_round_trips",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "a valid `X-Request-Id` is echoed, an invalid one replaced, errors carry it",
    },
    LegacyRow {
        name: "the_health_endpoints_answer",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "live 200, ready 200 with the storage check",
    },
    LegacyRow {
        name: "an_unknown_site_is_a_404_envelope",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "`Host: nowhere` on a site route → `http.unknown_site`",
    },
    LegacyRow {
        name: "a_body_over_the_limit_is_a_413_envelope",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "a declared body over 10 MiB → `http.body_too_large` before the body is read",
    },
    LegacyRow {
        name: "a_shutdown_drains_the_requests_in_flight",
        section: "Metadata, idempotency, the HTTP edge",
        pins: "a half-sent request completes during shutdown, the port then closes",
    },
    LegacyRow {
        name: "files: upload_then_download_with_a_range",
        section: "Blocked by a surface the host does not serve yet",
        pins: "upload, download and a ranged download over `/files`",
    },
    LegacyRow {
        name: "files: an_attached_file_is_not_an_orphan",
        section: "Blocked by a surface the host does not serve yet",
        pins: "an `attach` field names an uploaded file and is not an orphan",
    },
    LegacyRow {
        name: "realtime: a_permitted_client_is_notified_and_another_is_not",
        section: "Blocked by a surface the host does not serve yet",
        pins: "`GET /api/v1/realtime` notifies a permitted client and not another",
    },
    LegacyRow {
        name: "meta: a_runtime_created_doctype_is_usable_at_once",
        section: "Blocked by a surface the host does not serve yet",
        pins: "a DocType created at run time is usable at once",
    },
    LegacyRow {
        name: "naming: a_series_is_gapless_under_50_concurrent_creates",
        section: "Blocked by a surface the host does not serve yet",
        pins: "a naming series is gapless under 50 concurrent creates",
    },
];

/// The blocked cases that remain in the hand-written block: name, owner, reason.
pub const LEGACY_BLOCKED: &[(&str, &str, &str)] = &[
    (
        "a_dynamic_link_refuses_a_target_of_another_type",
        "WP-DOC-04.T1",
        "the engine never consults the plan's `dynamic_links` map (WP-MET-02.T1 compiles it): a Task accepts `related_type = project` with a Task id in `related_to` and answers 201 instead of a link error",
    ),
    (
        "a_read_only_field_refuses_a_user_value",
        "WP-DOC-04.T1",
        "the engine treats `read_only` as a form hint: a create carrying `task.logged_hours` answers 201 and stores the value",
    ),
    (
        "submit_a_time_entry_rolls_hours_up",
        "WP-PRD-01.T4",
        "`spot serve` does not register the installed apps' controllers, so the sample roll-up never runs and `task.logged_hours` stays null after a post",
    ),
    (
        "cancel_a_time_entry_rolls_hours_down",
        "WP-PRD-01.T4",
        "the same missing controller registration: a cancel cannot subtract hours it never added",
    ),
    (
        "an_hours_value_outside_the_range_is_refused",
        "WP-PRD-01.T4",
        "the sample controller's `validate_save` step is not registered by the host: a Time Entry with 30 hours answers 201",
    ),
    (
        "list_returns_the_default_columns_newest_first",
        "WP-PLT-05.T2",
        "the contract says `resource_list` answers newest first, but the list passes no order and the query compiler appends `id` ascending, so the answer is oldest first",
    ),
    (
        "a_reveal_without_the_group_right_is_refused",
        "WP-PRD-01.T4",
        "`spot serve` does not mount `field_reveal` (no `with_reveal`), so the route answers an empty 404 instead of `403 permission.reveal_denied`",
    ),
    (
        "a_throttled_sign_in_sets_the_retry_after_header",
        "WP-PLT-02.T2",
        "the throttle's 429 carries `details.retry_after` (the case above pins it) but no layer sets the `Retry-After` header the registry promises; the delay path is WP-PLT-02.T2 and the HTTP error-header mapping, if it is where this belongs, is WP-PLT-05.T2",
    ),
    (
        "repeated_wrong_passwords_lock_the_account",
        "WP-PLT-02.T2",
        "once `backoff_after` failures are in the window the throttle answers `Delay` on every attempt (it never checks whether the delay elapsed), so no further failure is recorded and the `lockout_after`-th failure is unreachable",
    ),
    (
        "a_locked_account_reports_its_retry_after",
        "WP-PLT-02.T2",
        "the account lockout is unreachable for the same reason, so no refusal can carry the lockout's `retry_after`",
    ),
    (
        "files: upload_then_download_with_a_range",
        "WP-PLT-10",
        "the host mounts no `/files` routes; the contract's upload, download and range operations are not served",
    ),
    (
        "files: an_attached_file_is_not_an_orphan",
        "WP-PLT-10",
        "no file service is mounted, so an `attach` field cannot name an uploaded file or prove an orphan",
    ),
    (
        "realtime: a_permitted_client_is_notified_and_another_is_not",
        "WP-PLT-08.T2",
        "`GET /api/v1/realtime` is not mounted (the router's own docs name WP-PLT-08.T2)",
    ),
    (
        "meta: a_runtime_created_doctype_is_usable_at_once",
        "WP-PLT-16.T2",
        "`spot serve` builds metadata from the app directories only and never reads the customisation records",
    ),
    (
        "naming: a_series_is_gapless_under_50_concurrent_creates",
        "WP-DOC-05",
        "the engine renders no name yet; the sample clients supply `code` themselves",
    ),
];

// ---------------------------------------------------------------------------------------------
// The first sign-in secret
// ---------------------------------------------------------------------------------------------

/// The secret `spot add-user` prints is a **first sign-in credential**, not a password and not an
/// API key: it signs in once, and until the user chooses a password its session may do three
/// things only — read its own session, sign out, change the password. Everything else is refused
/// with `403 cli.password_change_required` before the request reaches the router. The password
/// change replaces the session and spends the secret.
///
/// The body lives here rather than in `auth.rs` because the registry is the file this task owns.
/// It consumes the fixture's [`INVITED`] user, which is the only user that still holds a one-time
/// secret; no other case signs in as that user.
pub async fn the_first_sign_in_secret_needs_a_password_change(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(INVITED);
    let Some(secret) = user.secret.clone() else {
        return Err(failure!(
            "the invited user holds no one-time secret, so there is no first sign-in to pin"
        ));
    };

    // The printed secret is the credential: it signs in, and the answer says a password is due.
    let first = fixture.client().login(&user.email, &secret).await?;
    ensure!(
        first.status == 200,
        "the printed secret does not sign in (HTTP {}): {}",
        first.status,
        first.redacted_text()
    );
    ensure!(
        first
            .json()
            .pointer("/messages/0/code")
            .and_then(|code| code.as_str())
            == Some(concat!("auth", ".password_change_required")),
        "the sign-in with the printed secret does not ask for a password: {}",
        first.redacted_text()
    );
    let Some(restricted_token) = first
        .data()
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::to_owned)
    else {
        return Err(failure!(
            "the sign-in answered no session token: {}",
            first.redacted_text()
        ));
    };
    let restricted = fixture.client().with_token(&restricted_token);

    // The secret is not an API key: sent as a bearer credential it is refused outright.
    let bearer = fixture.client().with_token(&secret).meta("task").await?;
    ensure!(
        bearer.status == 401,
        "the printed secret also works as a bearer credential (HTTP {}): {}",
        bearer.status,
        bearer.redacted_text()
    );

    // The one allowed read: the session names the user it belongs to.
    let session = restricted.session().await?;
    session.expect(200)?;
    ensure!(
        session
            .data()
            .pointer("/user/id")
            .and_then(|id| id.as_str())
            == Some(user.id.as_str()),
        "the restricted session names another user: {}",
        session.text()
    );

    // Everything but the password change, the sign-out and that read is refused with the stable
    // code, before the router — so no route can answer data or a different error first.
    for (method, path) in [
        ("GET", "/api/v1/meta/task"),
        ("GET", "/api/v1/resource/task"),
        ("POST", "/api/v1/resource/task"),
        ("GET", "/api/v1/auth/permissions"),
    ] {
        let refused = restricted.send(method, path, &[], None).await?;
        ensure!(
            refused.status == 403,
            "{method} {path} answers HTTP {} for a restricted session: {}",
            refused.status,
            refused.redacted_text()
        );
        ensure!(
            refused.error_code().as_deref() == Some(concat!("cli", ".password_change_required")),
            "{method} {path} answers `{:?}`, not the refusal code: {}",
            refused.error_code(),
            refused.redacted_text()
        );
        ensure!(
            refused.json().get("data").is_none(),
            "{method} {path} answered a body for a refused request: {}",
            refused.redacted_text()
        );
    }

    // Signing out is allowed while restricted: a second restricted session ends with 204 and
    // stops working.
    let second = fixture.client().login(&user.email, &secret).await?;
    second.expect(200)?;
    let Some(second_token) = second
        .data()
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::to_owned)
    else {
        return Err(failure!(
            "the second sign-in answered no session token: {}",
            second.redacted_text()
        ));
    };
    let leaving = fixture.client().with_token(&second_token);
    leaving.logout().await?.expect(204)?;
    let after_sign_out = leaving.session().await?;
    ensure!(
        after_sign_out.status == 401,
        "the signed-out restricted session still answers HTTP {}: {}",
        after_sign_out.status,
        after_sign_out.text()
    );

    // The password change is the third allowed operation: it replaces the session, and the
    // restricted one is revoked.
    let chosen = "tundra-willow-basalt-64-quill";
    let changed = restricted
        .send_json(
            "POST",
            "/api/v1/auth/password",
            &[],
            &json!({ "current": secret, "new": chosen }),
        )
        .await?;
    ensure!(
        changed.status == 200,
        "the password change answers HTTP {} for a restricted session: {}",
        changed.status,
        changed.redacted_text()
    );
    let Some(full_token) = changed
        .data()
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::to_owned)
    else {
        return Err(failure!(
            "the password change answered no replacement session: {}",
            changed.redacted_text()
        ));
    };
    ensure!(
        full_token != restricted_token,
        "the restricted session was not replaced: the token is unchanged"
    );
    let revoked = restricted.session().await?;
    ensure!(
        revoked.status == 401,
        "the replaced restricted session still answers HTTP {}: {}",
        revoked.status,
        revoked.text()
    );

    // The secret is spent: it is neither a password nor a key any more.
    let spent = fixture.client().login(&user.email, &secret).await?;
    ensure!(
        spent.status == 401,
        "the spent secret still signs in (HTTP {}): {}",
        spent.status,
        spent.redacted_text()
    );
    let spent_bearer = fixture.client().with_token(&secret).meta("task").await?;
    ensure!(
        spent_bearer.status == 401,
        "the spent secret still works as a bearer credential (HTTP {}): {}",
        spent_bearer.status,
        spent_bearer.redacted_text()
    );

    // The replacement session is not restricted: the same requests that were refused answer.
    let full = fixture.client().with_token(&full_token);
    full.session().await?.expect(200)?;
    full.meta("task").await?.expect(200)?;

    // And the chosen password is the credential from now on.
    let via_password = fixture.client().sign_in(&user.email, chosen).await?;
    via_password.meta("task").await?.expect(200)?;
    Ok(())
}
