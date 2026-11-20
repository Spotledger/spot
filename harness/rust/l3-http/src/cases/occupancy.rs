//! Booking holds over HTTP (`/api/v1/occupancy/{entity}/...`, WP-STO-14.T3): the race cases of
//! ADR-087 section 6.3 through the real stack.
//!
//! These cases are **blocked**, not missing: the host serves the routes (the hosting module is
//! `spot-cli`'s `services/occupancy.rs`), but the fixture's sample app declares no `occupancy`
//! entity, so the module stays inert on the fixture's site. A case needs two things the fixture
//! does not have: the occupancy DocType in the private copy of the sample app (built with
//! `spot_occupancy::definition("project", Span::Instants)`) **before the host starts**, because
//! the service is created on the first build and a reload does not create it, and the table
//! `[services.occupancy]` in the fixture's `common.toml`. `fixture.rs` and the sample app are not
//! owned by this task; the lead widens them (see the task report), after which each entry below
//! becomes a `CaseDef::run` of the same name.
//!
//! The same behaviours already run at full scale, on real PostgreSQL, in-process through the
//! production port, in `crates/framework/spot-cli/tests/occupancy_service.rs` (200 concurrent
//! takers on 20 units; 10^5 attempts across 50 units with a SQL overlap check).

use super::CaseDef;

/// After `function`; independent of the other groups' data.
pub const ORDER: u32 = 50;

const OWNER: &str = "WP-STO-14.T3 follow-up";
const REASON: &str = "the fixture's sample app declares no `occupancy` entity and common.toml no `[services.occupancy]`, so the hosting module is inert on the fixture's site";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "occupancy: two_concurrent_claims_of_one_interval_have_exactly_one_winner",
            "Occupancy",
            "two claims of one interval race over HTTP: one answers `201`, the other `409 occupancy.conflict`, and the conflict body names no holder",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: two_hundred_concurrent_takers_on_twenty_units_never_double_book",
            "Occupancy",
            "200 concurrent takers on 20 units: no unit is double booked and the confirmed rows equal the distinct winners",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: one_hundred_thousand_attempts_across_fifty_units_leave_no_overlap",
            "Occupancy",
            "10^5 claim attempts across 50 units over HTTP leave no overlapping rows, checked by a direct SQL overlap query",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: an_expired_unswept_hold_is_replaced_and_the_old_confirm_is_hold_expired",
            "Occupancy",
            "an expired hold nobody swept is taken over by a new claim in one transaction, and the old holder's late confirm is `409 occupancy.hold_expired`",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: a_write_on_a_node_that_does_not_own_the_unit_is_partition_not_owner",
            "Occupancy",
            "a claim for a unit whose owning node is another answers `403 partition.not_owner` without touching storage",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: a_consumer_claims_for_itself_and_cannot_release_anothers_hold",
            "Occupancy",
            "a consumer principal holds as `consumer:<id>`, and releasing another principal's hold is `403 occupancy.not_holder`",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy: tenant_b_cannot_see_or_affect_tenant_as_units",
            "Occupancy",
            "with the same unit id on two tenants, one tenant's holds are neither visible nor blocking to the other",
            OWNER,
            REASON,
        ),
    ]
}
