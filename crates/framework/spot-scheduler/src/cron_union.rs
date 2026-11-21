//! Cron's day-of-month/day-of-week **union** rule, and the day-of-week numbering the manifest
//! and the `cron` crate disagree on.
//!
//! **Union.** In classic Vixie cron, when *both* `day of month` and `day of week` are restricted
//! (neither is `*`), a day matches when **either** field matches. When only one is restricted,
//! that field alone decides. The `cron` crate 0.12 does **not** do this: it requires both fields
//! to match (an intersection), so a job written `0 0 0 1 * 1` would fire only on a 1st that is
//! also a Monday. [`Schedule`](crate::Schedule) therefore runs two programs when both fields are
//! restricted (the expression with `day of week` set to `*`, and the one with `day of month` set
//! to `*`) and takes the earlier firing. [`day_matches`] is the same rule as a pure truth table.
//!
//! **Numbering.** The manifest validates a day of week as `0`-`7` with `0` and `7` both Sunday
//! and `1` Monday (the usual cron numbering). `cron` 0.12 numbers `1` Sunday to `7` Saturday and
//! rejects `0`. [`programs`] converts every day-of-week number before handing the expression to
//! the crate, expanding ranges and steps into an explicit list so `5-7` (Friday to Sunday) works.

use std::collections::BTreeSet;
use std::str::FromStr;

/// The day-of-week number the `cron` crate gives Sunday (its scale is 1 = Sunday to 7 =
/// Saturday); the manifest's Sunday is `0` or `7`.
pub const DAY_OF_WEEK_SUNDAY: u32 = 1;

/// Whether a day matches the schedule, under the union rule [`programs`] implements.
///
/// `dom_restricted` / `dow_restricted` say whether the manifest field was something other than
/// `*` (a restricted field); `dom_matches` / `dow_matches` say whether that field matches the
/// day.
pub fn day_matches(
    dom_restricted: bool,
    dom_matches: bool,
    dow_restricted: bool,
    dow_matches: bool,
) -> bool {
    match (dom_restricted, dow_restricted) {
        // Both restricted: Vixie union.
        (true, true) => dom_matches || dow_matches,
        // Exactly one restricted: that field decides.
        (true, false) => dom_matches,
        (false, true) => dow_matches,
        // Neither: every day.
        (false, false) => true,
    }
}

/// Whether a cron field restricts anything (anything but `*` or `?`).
fn restricted(field: &str) -> bool {
    field != "*" && field != "?"
}

/// A manifest day-of-week field (`0`-`7`, Sunday `0` or `7`) as the `cron` crate's list
/// (`1` Sunday to `7` Saturday). `None` when the field is not numbers, ranges and steps.
fn translate_day_of_week(field: &str) -> Option<String> {
    if !restricted(field) {
        return Some("*".to_owned());
    }
    let mut days = BTreeSet::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, Some(step.parse::<usize>().ok()?)),
            None => (part, None),
        };
        let (lo, hi) = if !restricted(range) {
            (0, 6)
        } else if let Some((lo, hi)) = range.split_once('-') {
            (lo.parse::<usize>().ok()?, hi.parse::<usize>().ok()?)
        } else {
            let lo = range.parse::<usize>().ok()?;
            (lo, if step.is_some() { 7 } else { lo })
        };
        if lo > hi || hi > 7 || step == Some(0) {
            return None;
        }
        days.extend((lo..=hi).step_by(step.unwrap_or(1)).map(|d| d % 7 + 1));
    }
    Some(
        days.iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(","),
    )
}

/// The `cron` programs that together fire exactly when the manifest expression does: one
/// program, or two (day of month only, day of week only) when both day fields are restricted.
pub(crate) fn programs(expression: &str) -> Result<Vec<cron::Schedule>, String> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let [second, minute, hour, dom, month, dow] = fields[..] else {
        return Err(format!(
            "a cron expression has 6 fields, got {}",
            fields.len()
        ));
    };
    let dow_list = translate_day_of_week(dow)
        .ok_or_else(|| format!("day of week `{dow}` is not numbers, ranges and steps in 0-7"))?;
    let build = |dom: &str, dow: &str| {
        cron::Schedule::from_str(&format!("{second} {minute} {hour} {dom} {month} {dow}"))
            .map_err(|err| err.to_string())
    };
    if restricted(dom) && restricted(dow) {
        Ok(vec![build(dom, "*")?, build("*", &dow_list)?])
    } else {
        Ok(vec![build(dom, &dow_list)?])
    }
}

#[cfg(test)]
mod tests {
    use super::translate_day_of_week as t;

    #[test]
    fn day_of_week_numbers_are_translated() {
        assert_eq!(t("*").as_deref(), Some("*"));
        assert_eq!(t("0").as_deref(), Some("1"));
        assert_eq!(t("7").as_deref(), Some("1"));
        assert_eq!(t("1").as_deref(), Some("2"));
        assert_eq!(t("5").as_deref(), Some("6"));
        assert_eq!(t("1-5").as_deref(), Some("2,3,4,5,6"));
        assert_eq!(t("5-7").as_deref(), Some("1,6,7"));
        assert_eq!(t("*/2").as_deref(), Some("1,3,5,7"));
        assert_eq!(t("1,3").as_deref(), Some("2,4"));
        assert_eq!(t("8"), None);
        assert_eq!(t("3-1"), None);
        assert_eq!(t("*/0"), None);
        assert_eq!(t("mon"), None);
    }
}
