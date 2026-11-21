//! The detection run: every check, grouped into one report.

use crate::collect::Collector;
use crate::input::ReleaseContext;
use crate::model::ConflictReport;
use crate::release::ReleaseDelta;
use crate::{apps, fields, fixtures, rules, scripts, views};

/// Detects every conflict this release causes on the site.
///
/// Runs the custom-field and override checks, the fixture checks, the view-layer checks, the
/// rule and expression checks, the script checks and the installed-app checks, then groups the
/// findings that share a root cause into one [`ConflictItem`](crate::ConflictItem) each. The
/// result is deterministic: the same context always gives the same report.
#[must_use]
pub fn detect(ctx: &ReleaseContext<'_>) -> ConflictReport {
    let delta = ReleaseDelta::from_plan(ctx.plan);
    let mut collector = Collector::new();
    fields::check(ctx, &delta, &mut collector);
    fixtures::check(ctx, &delta, &mut collector);
    views::check(ctx, &delta, &mut collector);
    rules::check(ctx, &delta, &mut collector);
    scripts::check(ctx, &delta, &mut collector);
    apps::check(ctx, &delta, &mut collector);
    collector.finish(ctx.plan.plan_digest.clone())
}
