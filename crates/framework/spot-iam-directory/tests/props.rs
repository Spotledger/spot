//! No sequence of lifecycle operations changes a user's id.
#![allow(missing_docs)]

mod common;

use common::{admin_actor, at};
use proptest::prelude::*;
use spot_iam_directory::{User, UserType, anonymise, is_anonymised};
use spot_storage_contract::RecordId;

#[derive(Clone, Copy, Debug)]
enum Op {
    Disable,
    Enable,
    Anonymise,
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(
        prop_oneof![Just(Op::Disable), Just(Op::Enable), Just(Op::Anonymise)],
        0..24,
    )
}

fn user_type() -> impl Strategy<Value = UserType> {
    prop_oneof![
        Just(UserType::Internal),
        Just(UserType::Portal),
        Just(UserType::Service),
        Just(UserType::Agent),
    ]
}

proptest! {
    #[test]
    fn the_id_never_changes(ty in user_type(), sequence in ops()) {
        let start = User::new(RecordId::new_v7(), ty, "ada", "Ada", at(0));
        let id = start.id;
        let mut user = start;
        let mut clock = 1;
        for op in sequence {
            clock += 1;
            user = match op {
                Op::Disable => user
                    .disable(&admin_actor(), "r", at(clock))
                    .map(|t| t.new_state)
                    .unwrap_or(user),
                Op::Enable => user
                    .enable(&admin_actor(), "r", at(clock))
                    .map(|t| t.new_state)
                    .unwrap_or(user),
                Op::Anonymise => anonymise(&user).into_user(),
            };
            prop_assert_eq!(user.id, id);
            prop_assert_eq!(user.user_type, ty);
            prop_assert!(user.validate().is_ok());
            // Once anonymised, always anonymised and never enabled again.
            if is_anonymised(&user) {
                prop_assert!(!user.enabled);
            }
        }
    }

    #[test]
    fn disable_then_disable_changes_nothing_the_second_time(ty in user_type(), secs in 1_i64..1_000_000) {
        let u = User::new(RecordId::new_v7(), ty, "ada", "Ada", at(0));
        let first = u.disable(&admin_actor(), "r", at(secs)).map(|t| t.new_state);
        prop_assert!(first.is_ok());
        if let Ok(first) = first {
            let second = first.disable(&admin_actor(), "r", at(secs + 5));
            prop_assert!(second.is_ok());
            if let Ok(second) = second {
                prop_assert!(second.is_noop());
                prop_assert_eq!(second.new_state, first);
            }
        }
    }
}
