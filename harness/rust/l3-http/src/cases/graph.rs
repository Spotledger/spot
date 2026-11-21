//! Graph and lineage end to end over HTTP on a running `spot serve` (WP-GRA-02.T3): the edges a
//! document write leaves, queries and lineage that respect permissions, an index that survives a
//! restart and a gap, the hard limits and a flood, two sites that share nothing, and the
//! security regressions of the graph routes.
//!
//! The shared fixture's host has neither the DocTypes these cases need (a chain of linked
//! documents and an item tree) nor a graph service tuned for a quick feed, and some cases stop
//! and start the server, so each case builds its own host (`graph/host.rs`): the fixture's
//! database, site and users, an app `chain` added to the catalogue, and a host started through
//! `spot_cli::start_with` as `crates/framework/spot-cli/tests/e2e.rs` does. That costs a few
//! seconds a case, which is why only the quick ones are in the pull request subset.
//!
//! The case files sit in `graph/`; `#[path]` keeps them out of the registry scan (which reads
//! `src/cases/*.rs` only) and names them from this file's directory.

#[path = "graph/answer.rs"]
mod answer;
#[path = "graph/edges.rs"]
mod edges;
#[path = "graph/host.rs"]
mod host;
#[path = "graph/isolation.rs"]
mod isolation;
#[path = "graph/limits.rs"]
mod limits;
#[path = "graph/lineage.rs"]
mod lineage;
#[path = "graph/perms.rs"]
mod perms;
#[path = "graph/rules.rs"]
mod rules;
#[path = "graph/security.rs"]
mod security;

use super::CaseDef;

/// After the functions case; every case owns its fixture, so none depends on another's data.
pub const ORDER: u32 = 41;

const SECTION: &str = "Graph and lineage over HTTP";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "graph: a_bad_rule_set_keeps_the_old_one_and_names_itself",
            SECTION,
            "a rule set of an app loads and readiness is clean; a set that is not range-restricted arrives with a reload and readiness (still 200, the site stays in rotation) names it and says `did not compile` without the good set's name or any predicate; a fixed set clears the report",
            false,
            |_| Box::pin(rules::a_bad_rule_set_keeps_the_old_one_and_names_itself()),
        ),
        CaseDef::blocked(
            "graph: a_hidden_edge_property_is_absent",
            SECTION,
            "an edge property the caller may not read is absent from the answer while the edge shows",
            "WP-GRA-02.T2",
            "the host derives the graph model with no relation specs, so no edge carries properties and the fixture has no relation DocType to hide one on",
        ),
        CaseDef::blocked(
            "graph: a_pushed_batch_carries_edge_keys_only",
            SECTION,
            "enabled against a local sink the projection pushes edge keys and types only, never a document value",
            "WP-GRA-07.T3",
            "the egress guard refuses a loopback sink and the projection takes https endpoints only, so no local sink can receive a push; the batch bytes are pinned by the module's unit tests",
        ),
        CaseDef::blocked(
            "graph: a_rule_set_derives_an_approver_differently_for_two_principals",
            SECTION,
            "a rule set from the fixture metadata derives an approver differently for two principals with different visibility",
            "WP-GRA-04.T2",
            "no route and no in-process handle reaches the rule graph port (`derive` is a Rust trait), so a derivation cannot be asked for over HTTP",
        ),
        CaseDef::run(
            "graph: edges_follow_document_writes_and_a_rolled_back_write_leaves_none",
            SECTION,
            "an order, receipt, invoice and payment chain is one walk in either direction; moving a link moves the edge; a refused write (a duplicate unique number) leaves no edge row and no node; a deleted payment leaves the walk",
            true,
            |_| Box::pin(edges::edges_follow_document_writes()),
        ),
        CaseDef::run(
            "graph: lineage_answers_chain_trace_bom_and_where_used_with_exact_decimals",
            SECTION,
            "the chain around an order, a forward and a backward trace, a bill of materials whose quantities are decimal strings (0.10 + 0.20 is 0.3, 99999999999999999.99 + 0.01 is 100000000000000000), where-used, a node limit and a depth limit that truncate and say so",
            true,
            |_| Box::pin(lineage::lineage_answers_chain_trace_bom_and_where_used()),
        ),
        CaseDef::run(
            "graph: queries_and_lineage_respect_permissions",
            SECTION,
            "a Viewer who cannot read the receipt gets the order with the receipt and everything beyond it removed and `withheld: true` (equal to the answer for an order with no receipts, apart from the flag; at once and from the hot index), the same for lineage, and an unreadable start (a Viewer's receipt, an Observer's order) is byte for byte the `404 graph.not_found` of an unknown id for the query and every lineage kind",
            true,
            |_| Box::pin(perms::queries_and_lineage_respect_permissions()),
        ),
        CaseDef::run(
            "graph: the_graph_routes_refuse_forged_bodies_and_injection",
            SECTION,
            "a body naming `tenant` or a principal is `422 graph.query_invalid` without echoing the value (graph and lineage), a header naming another site changes nothing, hostile strings as ids, DocTypes, edge types and filters are 4xx data and leave the edge table and the answers unchanged, no document value is in an answer, a site file or the run log, and the projection is off by default (no cursor file, no table)",
            false,
            |_| Box::pin(security::the_graph_routes_refuse_forged_bodies_and_injection()),
        ),
        CaseDef::run(
            "graph: the_hard_limits_refuse_first_and_a_flood_is_answered_in_budget",
            SECTION,
            "depth 9, limit 1001, 17 edge types and filters nested 6 deep or of 40 terms are `422 graph.too_large` for a record that does not exist (so before any work), other faults are `graph.query_invalid` without the value, lineage depth 13 and limit 5001 are too large; a star of more than 1,000 spokes (`L3_GRAPH_STAR`, up to 100000) is answered `truncated` by `nodes` inside 15 s while plain reads keep being served",
            false,
            |_| Box::pin(limits::the_hard_limits_refuse_first_and_a_flood_is_answered_in_budget()),
        ),
        CaseDef::run(
            "graph: the_index_survives_a_restart_and_a_gap_and_equals_the_database",
            SECTION,
            "after a server restart mid-feed the answer is the same; an edge that vanishes behind the feed (a gap) is found by the drift check and the index rebuilds; then the index and a database-only host give the same answer, and a rebuilt index equals the database",
            false,
            |_| Box::pin(edges::the_index_survives_a_restart_and_a_gap()),
        ),
        CaseDef::run(
            "graph: two_sites_share_no_edge",
            SECTION,
            "two sites of one server each see only their own edges by query, lineage and list, a record of the other site is the answer of an unknown id, a session of one site is refused on the other, and all of it holds again after a restart rebuilds both indexes",
            false,
            |_| Box::pin(isolation::two_sites_share_no_edge()),
        ),
    ]
}
