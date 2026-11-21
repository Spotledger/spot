//! The behaviour diff: lifecycle steps, methods, schedules, capabilities and seam overrides.
//!
//! Lifecycle steps are compared per DocType and hook point in the order the seam registry would
//! run them. A method change is reported only when it **breaks** callers: a removed argument, a
//! changed argument type, or a new required argument. A capability is reported only when it
//! **widens**. All of it comes from the manifests and the registered steps; nothing runs.

use std::collections::{BTreeMap, BTreeSet};

use crate::change::{BlockingClass, Change, ChangeKind};
use crate::model::{
    AppVersion, ArgSpec, CapabilitiesSummary, MethodDescription, OverrideDecl, PlannedStep,
    ScheduleDecl, SystemDescription,
};
use crate::risk::{RiskTable, rules};

/// Diffs the behaviour of `from` against `to`.
pub fn diff(from: &SystemDescription, to: &SystemDescription, table: &RiskTable) -> Vec<Change> {
    let mut out = Vec::new();
    steps(from, to, table, &mut out);
    methods(from, to, table, &mut out);
    schedules(from, to, table, &mut out);
    capabilities(from, to, table, &mut out);
    overrides(from, to, table, &mut out);
    out
}

/// Groups steps by `(doctype, point)`, preserving run order.
fn group_steps(system: &SystemDescription) -> BTreeMap<(String, String), Vec<&PlannedStep>> {
    let mut map: BTreeMap<(String, String), Vec<&PlannedStep>> = BTreeMap::new();
    for s in &system.seam_steps {
        map.entry((s.doctype.clone(), s.point.clone()))
            .or_default()
            .push(s);
    }
    map
}

/// The lifecycle-step changes.
fn steps(
    from: &SystemDescription,
    to: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_map = group_steps(from);
    let to_map = group_steps(to);
    let mut keys: BTreeSet<&(String, String)> = BTreeSet::new();
    keys.extend(from_map.keys());
    keys.extend(to_map.keys());
    for key in keys {
        let (doctype, point) = key;
        let empty: Vec<&PlannedStep> = Vec::new();
        let a = from_map.get(key).unwrap_or(&empty);
        let b = to_map.get(key).unwrap_or(&empty);
        let a_names: BTreeSet<&str> = a.iter().map(|s| s.step.as_str()).collect();
        let b_names: BTreeSet<&str> = b.iter().map(|s| s.step.as_str()).collect();
        for step in b_names.difference(&a_names) {
            let rule = rules::BEHAVIOUR_STEP_ADDED;
            out.push(
                Change::new(
                    format!("behaviour/{doctype}/step_added/{point}/{step}"),
                    ChangeKind::StepAdded,
                    rule,
                    format!("{doctype} {point} {step}"),
                    format!("Step `{step}` is added at `{point}` of `{doctype}`."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                )
                .evidence("absent", "registered"),
            );
        }
        for step in a_names.difference(&b_names) {
            let rule = rules::BEHAVIOUR_STEP_REMOVED;
            out.push(
                Change::new(
                    format!("behaviour/{doctype}/step_removed/{point}/{step}"),
                    ChangeKind::StepRemoved,
                    rule,
                    format!("{doctype} {point} {step}"),
                    format!("Step `{step}` is removed from `{point}` of `{doctype}`."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                )
                .evidence("registered", "absent")
                .why("a removed step may be behaviour callers rely on"),
            );
        }
        let a_by_name: BTreeMap<&str, &PlannedStep> =
            a.iter().map(|s| (s.step.as_str(), *s)).collect();
        let b_by_name: BTreeMap<&str, &PlannedStep> =
            b.iter().map(|s| (s.step.as_str(), *s)).collect();
        let mut priority_changed = false;
        for (step, old) in &a_by_name {
            let Some(new) = b_by_name.get(step) else {
                continue;
            };
            if old.priority != new.priority {
                priority_changed = true;
                let rule = rules::BEHAVIOUR_STEP_REPRIORITISED;
                out.push(
                    Change::new(
                        format!("behaviour/{doctype}/step_priority/{point}/{step}"),
                        ChangeKind::StepReprioritised,
                        rule,
                        format!("{doctype} {point} {step}"),
                        format!("Step `{step}` at `{point}` of `{doctype}` changes priority."),
                        table.risk_of(rule),
                        BlockingClass::Online,
                    )
                    .evidence(old.priority.to_string(), new.priority.to_string())
                    .why("changing the order of steps can change the result"),
                );
            }
        }
        if !priority_changed {
            let common_a: Vec<&str> = a
                .iter()
                .filter(|s| b_names.contains(s.step.as_str()))
                .map(|s| s.step.as_str())
                .collect();
            let common_b: Vec<&str> = b
                .iter()
                .filter(|s| a_names.contains(s.step.as_str()))
                .map(|s| s.step.as_str())
                .collect();
            if common_a != common_b {
                let rule = rules::BEHAVIOUR_STEP_REORDERED;
                out.push(
                    Change::new(
                        format!("behaviour/{doctype}/step_reordered/{point}"),
                        ChangeKind::StepReordered,
                        rule,
                        format!("{doctype} {point}"),
                        format!("Steps at `{point}` of `{doctype}` are reordered."),
                        table.risk_of(rule),
                        BlockingClass::Online,
                    )
                    .evidence(common_a.join(", "), common_b.join(", "))
                    .why("changing the order of steps can change the result"),
                );
            }
        }
    }
}

/// The method changes.
fn methods(
    from: &SystemDescription,
    to: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_map = method_map(from);
    let to_map = method_map(to);
    let mut keys: BTreeSet<&(String, String)> = BTreeSet::new();
    keys.extend(from_map.keys());
    keys.extend(to_map.keys());
    for key in keys {
        let (doctype, name) = key;
        match (from_map.get(key), to_map.get(key)) {
            (None, Some(_)) => {
                let rule = rules::BEHAVIOUR_METHOD_ADDED;
                out.push(Change::new(
                    format!("behaviour/{doctype}/method_added/{name}"),
                    ChangeKind::MethodAdded,
                    rule,
                    format!("{doctype}.{name}"),
                    format!("Method `{doctype}.{name}` is added."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                ));
            }
            (Some(_), None) => {
                let rule = rules::BEHAVIOUR_METHOD_REMOVED;
                out.push(
                    Change::new(
                        format!("behaviour/{doctype}/method_removed/{name}"),
                        ChangeKind::MethodRemoved,
                        rule,
                        format!("{doctype}.{name}"),
                        format!("Method `{doctype}.{name}` is removed."),
                        table.risk_of(rule),
                        BlockingClass::Online,
                    )
                    .why("a removed method breaks every caller"),
                );
            }
            (Some(old), Some(new)) => {
                if breaking(old, new) {
                    let rule = rules::BEHAVIOUR_METHOD_SIGNATURE_CHANGED;
                    out.push(
                        Change::new(
                            format!("behaviour/{doctype}/method_signature/{name}"),
                            ChangeKind::MethodSignatureChanged,
                            rule,
                            format!("{doctype}.{name}"),
                            format!("Method `{doctype}.{name}` changes its argument schema."),
                            table.risk_of(rule),
                            BlockingClass::Online,
                        )
                        .evidence(args_text(old), args_text(new))
                        .why("the change removes, retypes or newly requires an argument")
                        .alternative("add the new argument as optional, or keep the old one"),
                    );
                }
            }
            (None, None) => {}
        }
    }
}

/// The schedule changes.
fn schedules(
    from: &SystemDescription,
    to: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_map = schedule_map(from);
    let to_map = schedule_map(to);
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(from_map.keys());
    keys.extend(to_map.keys());
    for name in keys {
        let old = from_map.get(name);
        let new = to_map.get(name);
        let changed = match (old, new) {
            (None, Some(_)) | (Some(_), None) => true,
            (Some(a), Some(b)) => a.cron != b.cron || a.step != b.step || a.queue != b.queue,
            (None, None) => false,
        };
        if changed {
            let rule = rules::BEHAVIOUR_SCHEDULE_CHANGED;
            out.push(
                Change::new(
                    format!("behaviour/schedule/{name}"),
                    ChangeKind::ScheduleChanged,
                    rule,
                    name.clone(),
                    format!("Schedule `{name}` changes."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                )
                .evidence(schedule_text(old), schedule_text(new))
                .why("a schedule decides when work runs"),
            );
        }
    }
}

/// The capability changes.
fn capabilities(
    from: &SystemDescription,
    to: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_map = app_map(from);
    let to_map = app_map(to);
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(from_map.keys());
    keys.extend(to_map.keys());
    for name in keys {
        let empty = AppVersion {
            name: name.clone(),
            version: String::new(),
            manifest: crate::model::ManifestSummary::default(),
        };
        let a = from_map.get(name).copied().unwrap_or(&empty);
        let b = to_map.get(name).copied().unwrap_or(&empty);
        let widened = widened(&a.manifest.capabilities, &b.manifest.capabilities);
        if !widened.is_empty() {
            let rule = rules::BEHAVIOUR_CAPABILITY_WIDENED;
            out.push(
                Change::new(
                    format!("behaviour/{name}/capability"),
                    ChangeKind::CapabilityWidened,
                    rule,
                    name.clone(),
                    format!("App `{name}` widens its capabilities."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                )
                .evidence(
                    capability_text(&a.manifest.capabilities),
                    capability_text(&b.manifest.capabilities),
                )
                .why(format!("widened: {}", widened.join(", "))),
            );
        }
    }
}

/// The seam-override changes.
fn overrides(
    from: &SystemDescription,
    to: &SystemDescription,
    table: &RiskTable,
    out: &mut Vec<Change>,
) {
    let from_map = override_map(from);
    let to_map = override_map(to);
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(from_map.keys());
    keys.extend(to_map.keys());
    for target in keys {
        let old = from_map.get(target);
        let new = to_map.get(target);
        let changed = match (old, new) {
            (None, Some(_)) | (Some(_), None) => true,
            (Some(a), Some(b)) => a.kind != b.kind || a.by != b.by,
            (None, None) => false,
        };
        if changed {
            let rule = rules::BEHAVIOUR_SEAM_OVERRIDE_CHANGED;
            out.push(
                Change::new(
                    format!("behaviour/override/{target}"),
                    ChangeKind::SeamOverrideChanged,
                    rule,
                    target.clone(),
                    format!("The override of step `{target}` changes."),
                    table.risk_of(rule),
                    BlockingClass::Online,
                )
                .evidence(override_text(old), override_text(new))
                .why("changing which step wraps or replaces another changes behaviour"),
            );
        }
    }
}

/// A map of methods keyed by `(doctype, name)`.
fn method_map(system: &SystemDescription) -> BTreeMap<(String, String), &MethodDescription> {
    system
        .methods
        .iter()
        .map(|m| ((m.doctype.clone(), m.name.clone()), m))
        .collect()
}

/// A map of schedules keyed by name.
fn schedule_map(system: &SystemDescription) -> BTreeMap<String, &ScheduleDecl> {
    system
        .apps
        .iter()
        .flat_map(|a| a.manifest.schedules.iter())
        .map(|s| (s.name.clone(), s))
        .collect()
}

/// A map of apps keyed by name.
fn app_map(system: &SystemDescription) -> BTreeMap<String, &AppVersion> {
    system.apps.iter().map(|a| (a.name.clone(), a)).collect()
}

/// A map of overrides keyed by target step.
fn override_map(system: &SystemDescription) -> BTreeMap<String, &OverrideDecl> {
    system
        .apps
        .iter()
        .flat_map(|a| a.manifest.overrides.iter())
        .map(|o| (o.target.clone(), o))
        .collect()
}

/// Whether a method's new schema breaks callers: a removed argument, a changed type, or a new
/// required argument.
fn breaking(old: &MethodDescription, new: &MethodDescription) -> bool {
    let new_by_name: BTreeMap<&str, &ArgSpec> =
        new.args.iter().map(|a| (a.name.as_str(), a)).collect();
    for arg in &old.args {
        match new_by_name.get(arg.name.as_str()) {
            None => return true,
            Some(other) if other.ty != arg.ty => return true,
            Some(_) => {}
        }
    }
    let old_names: BTreeSet<&str> = old.args.iter().map(|a| a.name.as_str()).collect();
    new.args
        .iter()
        .any(|a| a.required && !old_names.contains(a.name.as_str()))
}

/// A text of a method's argument schema.
fn args_text(method: &MethodDescription) -> String {
    method
        .args
        .iter()
        .map(|a| format!("{}{}: {}", if a.required { "" } else { "?" }, a.name, a.ty))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A text of a schedule, or `absent`.
fn schedule_text(schedule: Option<&&ScheduleDecl>) -> String {
    match schedule {
        Some(s) => format!("{} {} {} {}", s.name, s.cron, s.step, s.queue),
        None => "absent".to_owned(),
    }
}

/// A text of an override, or `absent`.
fn override_text(override_decl: Option<&&OverrideDecl>) -> String {
    match override_decl {
        Some(o) => format!(
            "{} by {}",
            o.kind,
            o.by.clone().unwrap_or_else(|| "none".to_owned())
        ),
        None => "absent".to_owned(),
    }
}

/// The capabilities a target system has that the source does not (a widening).
fn widened(from: &CapabilitiesSummary, to: &CapabilitiesSummary) -> Vec<String> {
    let mut items = Vec::new();
    for endpoint in &to.network {
        if !from.network.contains(endpoint) {
            items.push(format!("network {endpoint}"));
        }
    }
    if to.email && !from.email {
        items.push("email".to_owned());
    }
    if file_rank(&to.files) > file_rank(&from.files) {
        items.push(format!("files {} -> {}", from.files, to.files));
    }
    for doctype in &to.doctypes_read {
        if !from.doctypes_read.contains(doctype) {
            items.push(format!("read {doctype}"));
        }
    }
    for doctype in &to.doctypes_write {
        if !from.doctypes_write.contains(doctype) {
            items.push(format!("write {doctype}"));
        }
    }
    items
}

/// The rank of a file-access capability.
fn file_rank(access: &str) -> u8 {
    match access {
        "read_write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// A text of a capability set.
fn capability_text(caps: &CapabilitiesSummary) -> String {
    format!(
        "network=[{}] email={} files={} read=[{}] write=[{}]",
        caps.network.join(","),
        caps.email,
        caps.files,
        caps.doctypes_read.join(","),
        caps.doctypes_write.join(",")
    )
}
