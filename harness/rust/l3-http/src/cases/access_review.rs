//! SoD analysis and access reviews run on a server (WP-SEC-02.T4). The end-to-end cases need the
//! host's `ReviewEnv` adapter and the PostgreSQL stores wired into the site and a two-node
//! fixture, so each is **blocked** with its owner; the host rules (pending removals confirmed by
//! read-back, retry at the tick) are pinned by the module's unit tests.

use super::CaseDef;

/// After the assignment rules cases.
pub const ORDER: u32 = 20;

const SECTION: &str = "Access review";
const OWNER: &str = "WP-SEC-02.T4 follow-up: the ReviewEnv adapter, the PG stores in the site and the two-node fixture";

pub fn cases() -> Vec<CaseDef> {
    let rows: [(&str, &str, &str); 7] = [
        (
            "seeded_conflict_matrix_is_detected",
            "the seeded SoD conflict matrix is found by the nightly and the on-change analysis",
            "needs the env adapter",
        ),
        (
            "a_campaign_runs_from_open_to_close_and_a_removed_user_loses_access_on_both_nodes",
            "keep and remove decisions complete the campaign and the removed user loses the access at once on both nodes",
            "needs a two-node fixture",
        ),
        (
            "self_review_is_refused",
            "a reviewer cannot decide an item about themselves and the owner cannot decide others' items",
            "needs the env adapter",
        ),
        (
            "the_deadline_escalates_or_removes_exactly_as_the_policy_says",
            "at the deadline undecided items follow the policy and nothing else",
            "needs a controllable clock",
        ),
        (
            "an_override_stops_covering_at_its_expiry",
            "an SoD override covers one rule, user and scope and stops at its expiry",
            "needs the env adapter",
        ),
        (
            "a_restart_in_the_middle_of_a_campaign_loses_nothing",
            "after a restart the open campaign and its pending removals continue",
            "needs a restartable host",
        ),
        (
            "a_campaign_of_another_tenant_answers_like_a_missing_one",
            "a campaign id of another tenant is the same answer as an unknown id",
            "needs the review routes mounted",
        ),
    ];
    rows.iter()
        .map(|(name, pins, reason)| {
            CaseDef::blocked(
                leak(&format!("access_review: {name}")),
                SECTION,
                pins,
                OWNER,
                reason,
            )
        })
        .collect()
}

/// The case table is built once per run; the strings are `'static` by construction.
fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}
