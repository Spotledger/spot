//! The search surface (`POST /api/v1/resource/{doctype}/query` with `search`, WP-STO-13.T6,
//! ADR-087 section 7, decision G2).
//!
//! **Every case here is blocked, on purpose.** No DocType of the shipped apps declares a
//! `[search]` projection (`apps/sample` is not owned by this task), and the host does not yet call
//! `services::search::install_hook` where it builds the site's backend (`host.rs`/`schema.rs` are
//! not owned either), so a site answers `501 search.unsupported` for every DocType. The cases name
//! the owner and the observation that proves the gap, and carry the budgets of decision G2 so the
//! work that unblocks them has the acceptance in front of it. A blocked case is not a pass.

use super::CaseDef;

/// Runs after the import cases.
pub const ORDER: u32 = 17;

const SECTION: &str = "Blocked by a surface the host does not serve yet";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "search: english_and_urdu_recall_and_p95_meet_the_g2_budgets",
            SECTION,
            "on 10,000 listings per tenant a search has recall at 10 of at least 0.90 for English queries and 0.70 for Urdu, and p95 of at most 50 ms (decision G2; the large-runner case, figures from WP-STO-15.T4)",
            "WP-STO-13.T6 (sample DocType with `[search]` and `install_hook` in the host)",
            "`POST /resource/{doctype}/query` with a `search` clause answers `501 search.unsupported` for every shipped DocType",
        ),
        CaseDef::blocked(
            "search: a_protected_field_never_matches_and_a_search_for_it_is_no_oracle",
            SECTION,
            "a value of a protected field never matches, searching for it returns what searching for a string that exists nowhere returns, and naming the field in `search.fields`, `facets` or `snippets` is `403 permission.field_denied`",
            "WP-STO-13.T6 (sample DocType with `[search]` and `install_hook` in the host)",
            "no shipped DocType declares `[search]`, so the `search` clause is `501 search.unsupported`",
        ),
        CaseDef::blocked(
            "search: tenant_b_never_sees_tenant_a_rows_and_a_user_never_sees_an_unreadable_row_in_results_facets_or_snippets",
            SECTION,
            "tenant B's search never returns tenant A's rows even for an identical term (the projection tables are per tenant database), and a user who cannot read a row never sees it in results, facet counts or snippets",
            "WP-STO-13.T6 (sample DocType with `[search]` and `install_hook` in the host)",
            "no shipped DocType declares `[search]`, so there is no projection to isolate",
        ),
        CaseDef::blocked(
            "search: a_missing_extension_or_an_unusable_ctype_reports_degraded_with_the_exact_fix",
            SECTION,
            "`/api/health/ready` names the `search` service degraded with the missing prerequisite and the exact fix, and search answers `501 search.unsupported`",
            "WP-STO-13.T6 (sample DocType with `[search]` and `install_hook` in the host)",
            "the `search` service is not mounted for a site whose backend has no search hook",
        ),
        CaseDef::blocked(
            "search: a_write_is_searchable_in_the_same_request_sequence_and_a_rolled_back_write_leaves_no_projection_row",
            SECTION,
            "create, update and delete a document and the next search sees it; a write that rolls back leaves no projection row; a rebuild after dropping the projection restores identical results",
            "WP-STO-13.T6 (sample DocType with `[search]` and `install_hook` in the host)",
            "no projection is installed in the site's storage",
        ),
    ]
}
