//! A consumer bearer on the occupancy routes (WP-STO-14.T4, ADR-083).
//!
//! **Blocked, not missing.** The resolver and the module exist (`spot-cli`'s
//! `services/consumer_session.rs`), but there is no consumer login service yet (ADR-083 task 14),
//! so no consumer bearer can be minted for a case, and the fixture's sample app declares no
//! `occupancy` entity (see `occupancy.rs`). The module registers `occupancy_consumer` after
//! `occupancy` once both are unblocked; each entry becomes a `CaseDef::run` of the same name.

use super::CaseDef;

/// After the staff occupancy group.
pub const ORDER: u32 = 51;

const OWNER: &str = "WP-STO-14.T4 follow-up";
const REASON: &str = "no consumer login service exists (ADR-083 task 14) to mint a consumer bearer, and the fixture's sample app declares no `occupancy` entity";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "occupancy_consumer: a_consumer_claims_and_releases_its_own_hold",
            "Occupancy",
            "a consumer bearer claims `201`, the hold's holder is `consumer:<id>`, and the same consumer releases it",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy_consumer: a_consumer_cannot_release_or_read_anothers_hold",
            "Occupancy",
            "another consumer's release answers `occupancy.not_holder`, and the interval state it may read is `busy` with no holder",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy_consumer: a_consumer_bearer_is_refused_on_every_non_listed_route",
            "Occupancy",
            "a consumer bearer on any staff route answers `403 permission.denied`",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy_consumer: a_consumer_session_of_another_site_is_no_session",
            "Occupancy",
            "a consumer session issued on another site answers `401 auth.invalid_credentials`",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "occupancy_consumer: staff_behaviour_is_unchanged_by_the_consumer_resolver",
            "Occupancy",
            "a staff bearer keeps its rights (`occupancy_manage` reads and manages any hold)",
            OWNER,
            REASON,
        ),
    ]
}
