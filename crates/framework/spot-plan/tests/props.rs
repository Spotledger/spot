//! Property tests: [`DocPlan::check_value`]'s kind check agrees with
//! [`FieldKind::accepts`] for every field kind, over generated values.
//!
//! The values are generated; the fields are every fixture field (so every kind is covered).
//! Because `check_value` runs its kind check first and its constraint checks only after, the
//! property holds exactly: the result is `plan.value.kind` if and only if the kind does not
//! accept the value.
#![allow(missing_docs)]

mod common;

use std::sync::{Arc, LazyLock};

use common::registry;
use proptest::prelude::*;
use spot_app_api::prelude::{Date, Decimal, Month, OffsetDateTime, RecordId, Time, Value};
use spot_meta::EffectiveMeta;
use spot_plan::{PlanError, PlanSet};

/// The fixture metadata, built once for the whole property run.
static META: LazyLock<Arc<EffectiveMeta>> = LazyLock::new(|| registry().effective());

/// A few valid calendar dates.
static DATES: LazyLock<Vec<Date>> = LazyLock::new(|| {
    let ok = |y: i32, m: Month, d: u8| {
        Date::from_calendar_date(y, m, d)
            .ok()
            .unwrap_or_else(|| panic!("test date"))
    };
    vec![
        ok(2026, Month::January, 31),
        ok(1999, Month::December, 1),
        ok(2100, Month::February, 28),
    ]
});

/// A few instant samples (all UTC; `check_value` only sees the value's shape).
static INSTANTS: LazyLock<Vec<OffsetDateTime>> = LazyLock::new(|| {
    let epoch = OffsetDateTime::UNIX_EPOCH;
    vec![epoch, epoch.replace_nanosecond(500).ok().unwrap_or(epoch)]
});

/// A few wall-clock times.
static TIMES: LazyLock<Vec<Time>> = LazyLock::new(|| {
    vec![
        Time::MIDNIGHT,
        Time::from_hms(12, 0, 0)
            .ok()
            .unwrap_or_else(|| panic!("test time")),
        Time::from_hms_nano(23, 59, 59, 999_999_999)
            .ok()
            .unwrap_or_else(|| panic!("test time")),
    ]
});

/// Strategy: any value, of any shape, with no float and no panic-prone construction.
fn value_strategy() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        // Mantissa and scale: every rust_decimal construction here is exact.
        (any::<i64>(), 0u32..=9).prop_map(|(m, s)| Value::Decimal(Decimal::new(m, s))),
        any::<String>().prop_map(Value::Text),
        (0..DATES.len()).prop_map(|i| Value::Date(DATES[i])),
        (0..TIMES.len()).prop_map(|i| Value::Time(TIMES[i])),
        (0..INSTANTS.len()).prop_map(|i| Value::DateTime(INSTANTS[i])),
        // Fresh ids: the checks never look inside an id.
        Just(Value::Id(RecordId::new_v7())),
        any::<String>().prop_map(Value::Json),
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::Bytes),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// For every fixture field and every generated value: `check_value` reports
    /// `plan.value.kind` exactly when `FieldKind::accepts` refuses the value. Constraint
    /// checks never mask the kind check (the kind is checked first) and never invent one.
    #[test]
    fn check_value_agrees_with_accepts_for_the_kind_check(value in value_strategy()) {
        let set = PlanSet::build(&META).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
        for doctype in set.doctypes() {
            let plan = set.get(doctype).unwrap_or_else(|| panic!("no plan"));
            for field in plan.fields() {
                let result = plan.check_value(field.name.as_str(), &value);
                let kind_err = matches!(result, Err(PlanError::KindMismatch { .. }));
                let accepts = field.kind.accepts(&value);
                prop_assert_eq!(
                    kind_err,
                    !accepts,
                    "field `{}` of `{}`: check_value said {}, accepts said {} for {:?}",
                    field.name,
                    plan.name(),
                    kind_err,
                    accepts,
                    value
                );
                // The kind check runs first, so a refusing kind is always what is reported.
                if !accepts {
                    prop_assert_eq!(result.err().map(|e| e.code()), Some("plan.value.kind"));
                }
            }
            // An unknown field is always reported, for any value.
            prop_assert!(plan.check_value("__proptest_unknown__", &value).is_err());
        }
    }

    /// The checks are total: every generated value gets a definite answer (`Ok` or a `plan.*`
    /// error), never a panic, and the answer is stable across two calls on the same plan.
    #[test]
    fn check_value_is_total_and_stable(value in value_strategy()) {
        let set = PlanSet::build(&META).unwrap_or_else(|e| panic!("plans do not compile: {e:?}"));
        let plan = set.get("invoice").unwrap_or_else(|| panic!("no invoice plan"));
        let first = plan.check_value("status", &value);
        let second = plan.check_value("status", &value);
        prop_assert_eq!(format!("{first:?}"), format!("{second:?}"), "stable across two calls");
        let total = first.is_ok()
            || matches!(
                first,
                Err(
                    PlanError::KindMismatch { .. }
                        | PlanError::TooLong { .. }
                        | PlanError::Scale { .. }
                        | PlanError::Precision { .. }
                        | PlanError::NotAnOption { .. }
                )
            );
        prop_assert!(total, "a total check, not a panic: {first:?}");
    }
}
