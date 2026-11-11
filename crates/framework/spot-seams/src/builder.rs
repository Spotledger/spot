//! [`RegistryBuilder`]: collects controllers, overrides and seam providers, validates them
//! together and precomputes the plans.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use spot_app_api::seams::SeamProvider;
use spot_app_api::{
    Controller, EffectClassName, EffectDecl, EffectKind, HookPoint, OverrideKind, Reversal,
    StepDecl, StepName, StepOverride, StoreName,
};

use crate::conflict::{Conflict, codes};
use crate::effects::{EffectChange, after, internal_stores, pairs, planned_effects};
use crate::explain::{AppliedOverride, ExplainLine, Role};
use crate::registry::{
    InstalledOverride, PlannedStep, Registry, SeamEntry, Table, WrappedStep, kind_rank,
};

/// A step declared by some controller.
struct StepInfo {
    app: String,
    doctype: String,
    point: HookPoint,
    priority: i32,
    budget: Duration,
    controller: Arc<dyn Controller>,
    effects: Vec<EffectDecl>,
}

struct SeamReg {
    app: String,
    name: String,
    version: u32,
    provider: Arc<dyn SeamProvider>,
}

struct SeamReplacement {
    reg: SeamReg,
    reason: String,
}

/// A seam an app needs: `<app>:<seam>` at a major version (WP-EXT-03).
struct SeamRequirement {
    app: String,
    name: String,
    major: u32,
}

/// Collects what apps contribute, then validates and plans it all at once.
///
/// Registration order never matters: [`build`](Self::build) sorts everything, so any order gives
/// the same plans, explanations and conflicts.
#[derive(Default)]
pub struct RegistryBuilder {
    controllers: Vec<(String, Arc<dyn Controller>)>,
    overrides: Vec<InstalledOverride>,
    providers: Vec<SeamReg>,
    replacements: Vec<SeamReplacement>,
    requirements: Vec<SeamRequirement>,
    known_classes: Option<BTreeSet<EffectClassName>>,
    /// Set by [`for_platform`](Self::for_platform): `build` fails closed without a class set.
    platform: bool,
}

impl RegistryBuilder {
    /// An empty **lenient** builder: without [`known_effect_classes`](Self::known_effect_classes)
    /// any well-formed effect class is accepted. For tests and tools only; a platform host must
    /// use [`for_platform`](Self::for_platform).
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `controller` for `app`. Every step it declares must be named `<app>:<step>`.
    pub fn add_controller(&mut self, app: &str, controller: Arc<dyn Controller>) -> &mut Self {
        self.controllers.push((app.to_owned(), controller));
        self
    }

    /// Registers an override declared by `app`.
    pub fn add_override(&mut self, app: &str, over: StepOverride) -> &mut Self {
        self.overrides.push(InstalledOverride {
            app: app.to_owned(),
            over,
        });
        self
    }

    /// Registers `app` as a provider of seam `name` at `version`.
    pub fn add_seam_provider(
        &mut self,
        app: &str,
        name: &str,
        version: u32,
        provider: Arc<dyn SeamProvider>,
    ) -> &mut Self {
        self.providers.push(SeamReg {
            app: app.to_owned(),
            name: name.to_owned(),
            version,
            provider,
        });
        self
    }

    /// Makes `app` replace whoever provides seam `name` at `version`, on purpose.
    pub fn replace_seam(
        &mut self,
        app: &str,
        name: &str,
        version: u32,
        provider: Arc<dyn SeamProvider>,
        reason: &str,
    ) -> &mut Self {
        self.replacements.push(SeamReplacement {
            reg: SeamReg {
                app: app.to_owned(),
                name: name.to_owned(),
                version,
                provider,
            },
            reason: reason.to_owned(),
        });
        self
    }

    /// Records that `app` needs seam `name` at major version `major`.
    ///
    /// Checked at build time: the install is refused when nothing provides the seam, or when it
    /// is provided at a different major ([`Conflict`](crate::Conflict) codes `seam.required_missing`
    /// and `seam.version_mismatch`). Several apps may require the same seam; a provider may
    /// offer several majors side by side.
    pub fn require_seam(&mut self, app: &str, name: &str, major: u32) -> &mut Self {
        self.requirements.push(SeamRequirement {
            app: app.to_owned(),
            name: name.to_owned(),
            major,
        });
        self
    }

    /// Limits the effect classes steps may declare to `classes`. A declaration of any other
    /// class is refused at build time (`effects.unknown_class`). Without this call every
    /// well-formed class is accepted, which is how a 0.4.0 site behaves. Calling it twice adds
    /// to the set.
    pub fn known_effect_classes(&mut self, classes: &[EffectClassName]) -> &mut Self {
        self.known_classes
            .get_or_insert_with(BTreeSet::new)
            .extend(classes.iter().cloned());
        self
    }

    /// An empty builder for a **platform host**. The set of known effect classes is
    /// required: [`build`](Self::build) fails closed with `effects.unknown_class` unless
    /// [`known_effect_classes`](Self::known_effect_classes) was called (an empty slice is a set
    /// that refuses every class), and an undeclared class is refused as `effects.unknown_class`.
    pub fn for_platform() -> Self {
        Self {
            platform: true,
            ..Self::default()
        }
    }

    /// Validates everything and builds the registry, or reports **all** conflicts, sorted.
    ///
    /// # Override rules
    ///
    /// ADR-057 leaves these open; this crate fixes them (reported under the task's open
    /// questions):
    ///
    /// - A step used as the `by` step of any override is **detached** from its own hook point:
    ///   it runs only where overrides put it. Without this, a wrapper or replacement would run
    ///   twice (once on its own, once in the slot). Declaring it at a point is still how it gets
    ///   its priority, budget and controller.
    /// - `Before` and `After` still run when the target is disabled, replaced or wrapped: they
    ///   are attached to the target's slot, not to the target's body.
    /// - Several `Before` (or `After`) overrides of one step run in (priority, name) order of
    ///   their `by` steps.
    /// - Overrides do not chain: a `by` step cannot itself be a target (`override.chained`),
    ///   and a loop is `override.cycle`.
    /// - `Replace` with `Wrap` of one step is refused, like two of either: the result would
    ///   depend on registration order.
    ///
    /// # Effect rules
    ///
    /// Each controller's [`step_effects`](Controller::step_effects) is checked on its own
    /// (against the contract's rules and the known classes, see
    /// [`known_effect_classes`](Self::known_effect_classes)) together with the other checks.
    /// The pairing checks (`effects.no_paired_reversal`, `effects.override_drops_reversal`) need
    /// the finished plans, so they run only when nothing else conflicts. A forward effect is
    /// covered when every store it writes is written by some internal effect in the DocType's
    /// `after_cancel` plan; the class of the reversing effect does not have to match.
    pub fn build(&self) -> Result<Registry, Vec<Conflict>> {
        let mut conflicts = Vec::new();
        if self.platform && self.known_classes.is_none() {
            conflicts.push(Conflict::new(
                codes::EFFECTS_UNKNOWN_CLASS,
                "a platform registry needs its set of known effect classes; none was given"
                    .to_owned(),
                &[],
            ));
        }
        let steps = self.collect_steps(&mut conflicts);
        let overrides = self.validate_overrides(&steps, &mut conflicts);
        let seams = self.collect_seams(&mut conflicts);
        self.validate_seam_requirements(&seams, &mut conflicts);
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Err(conflicts);
        }
        let (plans, explanations) = plan_all(&steps, &overrides);
        pairing_conflicts(&steps, &overrides, &plans, &mut conflicts);
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Err(conflicts);
        }
        let effect_changes = effect_changes(&steps, &overrides);
        Ok(Registry {
            plans,
            explanations,
            overrides,
            effect_changes,
            seams,
        })
    }

    fn collect_steps(&self, conflicts: &mut Vec<Conflict>) -> BTreeMap<StepName, StepInfo> {
        // Sorted by app so a duplicate is reported the same way whatever the order.
        let mut ctrls: Vec<&(String, Arc<dyn Controller>)> = self.controllers.iter().collect();
        ctrls.sort_by(|a, b| a.0.cmp(&b.0));
        let mut steps: BTreeMap<StepName, StepInfo> = BTreeMap::new();
        for (app, ctrl) in ctrls {
            let decls = ctrl.steps();
            let mut effects = self.collect_effects(app, ctrl.as_ref(), &decls, conflicts);
            for d in decls {
                if d.name.app() != app {
                    conflicts.push(Conflict::new(
                        codes::STEP_FOREIGN_APP,
                        format!(
                            "app `{app}` registered a controller with step `{}`, which belongs to app `{}`",
                            d.name,
                            d.name.app()
                        ),
                        &[app, d.name.app()],
                    ));
                    continue;
                }
                if let Some(prev) = steps.get(&d.name) {
                    conflicts.push(Conflict::new(
                        codes::STEP_DUPLICATE,
                        format!(
                            "step `{}` is declared twice: by app `{}` (DocType `{}`) and by app `{app}` (DocType `{}`)",
                            d.name,
                            prev.app,
                            prev.doctype,
                            ctrl.doctype()
                        ),
                        &[&prev.app, app],
                    ));
                    continue;
                }
                let step_effects = effects.remove(&d.name).unwrap_or_default();
                steps.insert(
                    d.name,
                    StepInfo {
                        app: app.clone(),
                        doctype: ctrl.doctype().to_owned(),
                        point: d.point,
                        priority: d.priority,
                        budget: d.budget,
                        controller: ctrl.clone(),
                        effects: step_effects,
                    },
                );
            }
        }
        steps
    }

    /// Reads and checks one controller's effect declarations: each names a step the controller
    /// declares, once; each passes the contract's rules for the hook point of its step; each
    /// class is known (when the builder was given the known classes).
    fn collect_effects(
        &self,
        app: &str,
        ctrl: &dyn Controller,
        decls: &[StepDecl],
        conflicts: &mut Vec<Conflict>,
    ) -> BTreeMap<StepName, Vec<EffectDecl>> {
        let mut out: BTreeMap<StepName, Vec<EffectDecl>> = BTreeMap::new();
        for se in ctrl.step_effects() {
            if out.contains_key(&se.step) {
                conflicts.push(Conflict::new(
                    codes::EFFECTS_DUPLICATE_STEP,
                    format!(
                        "app `{app}` declares the effects of step `{}` twice",
                        se.step
                    ),
                    &[app],
                ));
                continue;
            }
            let Some(step) = decls.iter().find(|d| d.name == se.step) else {
                conflicts.push(Conflict::new(
                    codes::EFFECTS_UNKNOWN_STEP,
                    format!(
                        "app `{app}` declares effects for `{}`, which its controller for DocType `{}` does not declare",
                        se.step,
                        ctrl.doctype()
                    ),
                    &[app],
                ));
                continue;
            };
            for e in &se.effects {
                if let Err(err) = e.validate_at(&step.point) {
                    conflicts.push(Conflict::new(
                        effect_code(&err.code),
                        format!("app `{app}`, step `{}`: {}", se.step, err.message),
                        &[app],
                    ));
                }
                if let Some(known) = &self.known_classes
                    && !known.contains(&e.class)
                {
                    conflicts.push(Conflict::new(
                        codes::EFFECTS_UNKNOWN_CLASS,
                        format!(
                            "app `{app}`, step `{}` declares effect class `{}`, which is not a known effect class",
                            se.step, e.class
                        ),
                        &[app],
                    ));
                }
            }
            out.insert(se.step, se.effects);
        }
        out
    }

    /// Returns the well-formed overrides, sorted by (target, kind, by, app), and reports the rest.
    fn validate_overrides(
        &self,
        steps: &BTreeMap<StepName, StepInfo>,
        conflicts: &mut Vec<Conflict>,
    ) -> Vec<InstalledOverride> {
        let mut sorted: Vec<&InstalledOverride> = self.overrides.iter().collect();
        sorted.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let mut ok: Vec<&InstalledOverride> = Vec::new();
        for io in sorted {
            let (app, o) = (io.app.as_str(), &io.over);
            let Some(target) = steps.get(&o.target) else {
                conflicts.push(Conflict::new(
                    codes::OVERRIDE_UNKNOWN_TARGET,
                    format!(
                        "app `{app}` overrides step `{}`, which no installed app declares",
                        o.target
                    ),
                    &[app, o.target.app()],
                ));
                continue;
            };
            match (&o.by, o.kind) {
                (Some(_), OverrideKind::Disable)
                | (None, OverrideKind::Before)
                | (None, OverrideKind::After)
                | (None, OverrideKind::Wrap)
                | (None, OverrideKind::Replace) => {
                    conflicts.push(Conflict::new(
                        codes::OVERRIDE_BAD_SHAPE,
                        format!(
                            "app `{app}`: the {:?} override of `{}` {}",
                            o.kind,
                            o.target,
                            if o.by.is_some() {
                                "must not name a `by` step"
                            } else {
                                "needs a `by` step"
                            }
                        ),
                        &[app, o.target.app()],
                    ));
                    continue;
                }
                (Some(by), _) => {
                    let Some(by_info) = steps.get(by) else {
                        conflicts.push(Conflict::new(
                            codes::OVERRIDE_UNKNOWN_BY,
                            format!(
                                "app `{app}` overrides `{}` with step `{by}`, which no installed app declares",
                                o.target
                            ),
                            &[app, o.target.app(), by.app()],
                        ));
                        continue;
                    };
                    if by_info.doctype != target.doctype {
                        conflicts.push(Conflict::new(
                            codes::OVERRIDE_DOCTYPE_MISMATCH,
                            format!(
                                "app `{app}` overrides `{}` (DocType `{}`) with `{by}` (DocType `{}`); both must be the same DocType",
                                o.target, target.doctype, by_info.doctype
                            ),
                            &[app, o.target.app(), by.app()],
                        ));
                        continue;
                    }
                }
                (None, OverrideKind::Disable) => {}
            }
            ok.push(io);
        }

        let mut by_target: BTreeMap<&StepName, Vec<&InstalledOverride>> = BTreeMap::new();
        for io in &ok {
            by_target.entry(&io.over.target).or_default().push(io);
        }
        for (target, list) in &by_target {
            target_conflicts(target, list, conflicts);
        }
        graph_conflicts(&ok, conflicts);
        ok.into_iter().cloned().collect()
    }

    fn collect_seams(
        &self,
        conflicts: &mut Vec<Conflict>,
    ) -> HashMap<String, BTreeMap<u32, SeamEntry>> {
        let mut provs: Vec<&SeamReg> = self.providers.iter().collect();
        provs.sort_by(|a, b| (&a.name, a.version, &a.app).cmp(&(&b.name, b.version, &b.app)));
        let mut out: HashMap<String, BTreeMap<u32, SeamEntry>> = HashMap::new();
        for p in provs {
            let versions = out.entry(p.name.clone()).or_default();
            if let Some(prev) = versions.get(&p.version) {
                conflicts.push(Conflict::new(
                    codes::SEAM_DUPLICATE_PROVIDER,
                    format!(
                        "apps `{}` and `{}` both provide seam `{}` v{}; one must replace the other explicitly",
                        prev.app, p.app, p.name, p.version
                    ),
                    &[&prev.app, &p.app],
                ));
                continue;
            }
            versions.insert(
                p.version,
                SeamEntry {
                    app: p.app.clone(),
                    provider: p.provider.clone(),
                    replacement: None,
                },
            );
        }
        let mut reps: Vec<&SeamReplacement> = self.replacements.iter().collect();
        reps.sort_by(|a, b| {
            (&a.reg.name, a.reg.version, &a.reg.app).cmp(&(&b.reg.name, b.reg.version, &b.reg.app))
        });
        for r in reps {
            let r_reg = &r.reg;
            let Some(entry) = out
                .get_mut(&r_reg.name)
                .and_then(|m| m.get_mut(&r_reg.version))
            else {
                conflicts.push(Conflict::new(
                    codes::SEAM_REPLACE_UNKNOWN,
                    format!(
                        "app `{}` replaces seam `{}` v{}, which no installed app provides",
                        r_reg.app, r_reg.name, r_reg.version
                    ),
                    &[&r_reg.app],
                ));
                continue;
            };
            if let Some((prev, _, _)) = &entry.replacement {
                conflicts.push(Conflict::new(
                    codes::SEAM_DUPLICATE_REPLACEMENT,
                    format!(
                        "apps `{prev}` and `{}` both replace seam `{}` v{}",
                        r_reg.app, r_reg.name, r_reg.version
                    ),
                    &[prev, &r_reg.app],
                ));
                continue;
            }
            entry.replacement = Some((r_reg.app.clone(), r_reg.provider.clone(), r.reason.clone()));
        }
        out
    }

    /// Checks every [`require_seam`](Self::require_seam) against what the providers offer, and
    /// reports `seam.required_missing` or `seam.version_mismatch` for each unmet requirement.
    /// Runs even when other checks already failed, so one build reports every problem.
    fn validate_seam_requirements(
        &self,
        seams: &HashMap<String, BTreeMap<u32, SeamEntry>>,
        conflicts: &mut Vec<Conflict>,
    ) {
        let mut reqs: Vec<&SeamRequirement> = self.requirements.iter().collect();
        reqs.sort_by(|a, b| (&a.app, &a.name, a.major).cmp(&(&b.app, &b.name, b.major)));
        for r in reqs {
            let Some(versions) = seams.get(&r.name) else {
                conflicts.push(Conflict::new(
                    codes::SEAM_REQUIRED_MISSING,
                    format!(
                        "app `{app}` needs seam `{name}` at major version {major}, which no installed app provides",
                        app = r.app,
                        name = r.name,
                        major = r.major
                    ),
                    &[&r.app],
                ));
                continue;
            };
            if versions.contains_key(&r.major) {
                continue;
            }
            let offered: Vec<String> = versions.keys().map(|v| v.to_string()).collect();
            let mut providers: Vec<&str> = versions.values().map(|e| e.app.as_str()).collect();
            providers.sort_unstable();
            providers.dedup();
            let providers: Vec<&str> = providers.clone();
            let mut apps: Vec<&str> = providers.clone();
            apps.push(&r.app);
            conflicts.push(Conflict::new(
                codes::SEAM_VERSION_MISMATCH,
                format!(
                    "app `{app}` needs seam `{name}` at major version {major}, but apps {providers} provide it at version(s) {offered}",
                    app = r.app,
                    name = r.name,
                    major = r.major,
                    providers = providers
                        .iter()
                        .map(|a| format!("`{a}`"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                    offered = offered.join(", ")
                ),
                &apps,
            ));
        }
    }
}

fn sort_key(io: &InstalledOverride) -> (&StepName, u8, Option<&StepName>, &str) {
    (
        &io.over.target,
        kind_rank(io.over.kind),
        io.over.by.as_ref(),
        io.app.as_str(),
    )
}

/// Conflicts among the overrides of one target.
fn target_conflicts(target: &StepName, list: &[&InstalledOverride], conflicts: &mut Vec<Conflict>) {
    let of = |k: OverrideKind| -> Vec<&&InstalledOverride> {
        list.iter().filter(|o| o.over.kind == k).collect()
    };
    let apps_of =
        |v: &[&&InstalledOverride]| -> Vec<String> { v.iter().map(|o| o.app.clone()).collect() };
    let join = |v: &[String]| {
        v.iter()
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    };
    let (replace, wrap, disable) = (
        of(OverrideKind::Replace),
        of(OverrideKind::Wrap),
        of(OverrideKind::Disable),
    );
    for (kind, group, code) in [
        ("Replace", &replace, codes::OVERRIDE_DUPLICATE_REPLACE),
        ("Wrap", &wrap, codes::OVERRIDE_DUPLICATE_WRAP),
    ] {
        if group.len() > 1 {
            let apps = apps_of(group);
            let all: Vec<&str> = std::iter::once(target.app())
                .chain(apps.iter().map(String::as_str))
                .collect();
            conflicts.push(Conflict::new(
                code,
                format!(
                    "apps {} both {kind} step `{target}` (app `{}`)",
                    join(&apps),
                    target.app()
                ),
                &all,
            ));
        }
    }
    if !replace.is_empty() && !wrap.is_empty() {
        let (r, w) = (&replace[0].app, &wrap[0].app);
        conflicts.push(Conflict::new(
            codes::OVERRIDE_REPLACE_AND_WRAP,
            format!(
                "app `{r}` replaces and app `{w}` wraps step `{target}` (app `{}`)",
                target.app()
            ),
            &[r, w, target.app()],
        ));
    }
    if let Some(d) = disable.first() {
        for other in replace.iter().chain(wrap.iter()) {
            conflicts.push(Conflict::new(
                codes::OVERRIDE_DISABLED_AND_CHANGED,
                format!(
                    "app `{}` disables step `{target}` (app `{}`) while app `{}` {} it",
                    d.app,
                    target.app(),
                    other.app,
                    if other.over.kind == OverrideKind::Replace {
                        "replaces"
                    } else {
                        "wraps"
                    }
                ),
                &[&d.app, &other.app, target.app()],
            ));
        }
    }
    let mut seen: BTreeSet<(u8, &StepName)> = BTreeSet::new();
    for o in list {
        let Some(by) = &o.over.by else { continue };
        if matches!(o.over.kind, OverrideKind::Before | OverrideKind::After)
            && !seen.insert((kind_rank(o.over.kind), by))
        {
            conflicts.push(Conflict::new(
                codes::OVERRIDE_DUPLICATE,
                format!(
                    "{:?} of `{target}` by `{by}` is declared more than once (app `{}` among them)",
                    o.over.kind, o.app
                ),
                &[&o.app, target.app(), by.app()],
            ));
        }
    }
}

/// Cycles and chains in the target → `by` graph.
fn graph_conflicts(ok: &[&InstalledOverride], conflicts: &mut Vec<Conflict>) {
    let mut edges: BTreeMap<&StepName, BTreeSet<&StepName>> = BTreeMap::new();
    for io in ok {
        if let Some(by) = &io.over.by {
            edges.entry(&io.over.target).or_default().insert(by);
        }
    }
    let reach = |from: &StepName| -> BTreeSet<&StepName> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&StepName> = edges
            .get(from)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                stack.extend(edges.get(n).into_iter().flatten().copied());
            }
        }
        seen
    };
    let mut on_cycle: BTreeSet<&StepName> = BTreeSet::new();
    let mut reported: BTreeSet<Vec<&StepName>> = BTreeSet::new();
    for n in edges.keys() {
        let r = reach(n);
        if !r.contains(n) {
            continue;
        }
        // The loop's members: nodes mutually reachable with `n`.
        let mut members: Vec<&StepName> =
            r.iter().copied().filter(|m| reach(m).contains(n)).collect();
        members.push(n);
        members.sort();
        members.dedup();
        on_cycle.extend(members.iter().copied());
        if reported.insert(members.clone()) {
            let names = members
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let apps: Vec<&str> = ok
                .iter()
                .filter(|io| members.contains(&&io.over.target))
                .map(|io| io.app.as_str())
                .chain(members.iter().map(|m| m.app()))
                .collect();
            conflicts.push(Conflict::new(
                codes::OVERRIDE_CYCLE,
                format!("overrides loop through steps {names}"),
                &apps,
            ));
        }
    }
    let used_as_by: BTreeMap<&StepName, &str> = ok
        .iter()
        .filter_map(|io| io.over.by.as_ref().map(|b| (b, io.app.as_str())))
        .collect();
    for io in ok {
        let t = &io.over.target;
        if on_cycle.contains(t) {
            continue;
        }
        if let Some(first) = used_as_by.get(t) {
            conflicts.push(Conflict::new(
                codes::OVERRIDE_CHAINED,
                format!(
                    "app `{}` overrides step `{t}`, but app `{first}` uses it as an override step; override the original step instead",
                    io.app
                ),
                &[&io.app, first, t.app()],
            ));
        }
    }
}

fn applied(io: &InstalledOverride) -> AppliedOverride {
    AppliedOverride {
        kind: io.over.kind,
        target: io.over.target.clone(),
        by: io.over.by.clone(),
        declared_by: io.app.clone(),
        reason: io.over.reason.clone(),
    }
}

/// Precomputes the plan and explanation of every (DocType, hook point).
fn plan_all(
    steps: &BTreeMap<StepName, StepInfo>,
    overrides: &[InstalledOverride],
) -> (Table<PlannedStep>, Table<ExplainLine>) {
    let detached: BTreeSet<&StepName> = overrides
        .iter()
        .filter_map(|o| o.over.by.as_ref())
        .collect();
    let mut on_target: BTreeMap<&StepName, Vec<&InstalledOverride>> = BTreeMap::new();
    for io in overrides {
        on_target.entry(&io.over.target).or_default().push(io);
    }
    // (priority, name) order of a step.
    let rank = |n: &StepName| (steps.get(n).map_or(0, |s| s.priority), n.clone());

    let mut groups: BTreeMap<(&str, usize), Vec<&StepName>> = BTreeMap::new();
    let mut points: Vec<&HookPoint> = Vec::new();
    let mut idx: HashMap<(&str, &HookPoint), usize> = HashMap::new();
    for (name, s) in steps {
        if detached.contains(name) {
            continue;
        }
        let key = (s.doctype.as_str(), &s.point);
        let i = *idx.entry(key).or_insert_with(|| {
            points.push(&s.point);
            points.len() - 1
        });
        groups
            .entry((s.doctype.as_str(), i))
            .or_default()
            .push(name);
    }

    let mut plans: Table<PlannedStep> = HashMap::new();
    let mut explains: Table<ExplainLine> = HashMap::new();
    for ((doctype, i), mut names) in groups {
        names.sort_by_key(|n| rank(n));
        let mut plan = Vec::new();
        let mut lines = Vec::new();
        for name in names {
            let Some(info) = steps.get(name) else {
                continue;
            };
            let ovs: &[&InstalledOverride] = on_target.get(name).map_or(&[], Vec::as_slice);
            let all: Vec<AppliedOverride> = ovs.iter().map(|o| applied(o)).collect();
            let side = |kind: OverrideKind| -> Vec<&InstalledOverride> {
                let mut v: Vec<&InstalledOverride> = ovs
                    .iter()
                    .copied()
                    .filter(|o| o.over.kind == kind)
                    .collect();
                v.sort_by_key(|o| o.over.by.as_ref().map(&rank));
                v
            };
            let planned = |n: &StepName, wrapped: Option<&StepName>| -> Option<PlannedStep> {
                let s = steps.get(n)?;
                Some(PlannedStep {
                    step: n.clone(),
                    app: s.app.clone(),
                    priority: s.priority,
                    budget: s.budget,
                    controller: s.controller.clone(),
                    wrapped: wrapped.and_then(|w| {
                        let ws = steps.get(w)?;
                        Some(WrappedStep {
                            step: w.clone(),
                            app: ws.app.clone(),
                            controller: ws.controller.clone(),
                            effects: ws.effects.clone(),
                        })
                    }),
                    effects: s.effects.clone(),
                })
            };
            let push = |plan: &mut Vec<PlannedStep>,
                        lines: &mut Vec<ExplainLine>,
                        n: &StepName,
                        wrapped: Option<&StepName>,
                        role: Role,
                        ov: Vec<AppliedOverride>| {
                if let (Some(p), Some(s)) = (planned(n, wrapped), steps.get(n)) {
                    let effects = planned_effects(&p).map(|(_, _, e)| e.clone()).collect();
                    lines.push(ExplainLine {
                        step: n.clone(),
                        app: s.app.clone(),
                        priority: s.priority,
                        role,
                        overrides: ov,
                        runs: true,
                        effects,
                    });
                    plan.push(p);
                }
            };

            for o in side(OverrideKind::Before) {
                if let Some(by) = &o.over.by {
                    push(
                        &mut plan,
                        &mut lines,
                        by,
                        None,
                        Role::Before {
                            target: name.clone(),
                        },
                        vec![applied(o)],
                    );
                }
            }
            let kind_of = |k: OverrideKind| ovs.iter().find(|o| o.over.kind == k);
            if kind_of(OverrideKind::Disable).is_some() {
                lines.push(ExplainLine {
                    step: name.clone(),
                    app: info.app.clone(),
                    priority: info.priority,
                    role: Role::Declared,
                    overrides: all.clone(),
                    runs: false,
                    effects: info.effects.clone(),
                });
            } else if let Some(by) = kind_of(OverrideKind::Replace).and_then(|o| o.over.by.as_ref())
            {
                push(
                    &mut plan,
                    &mut lines,
                    by,
                    None,
                    Role::Replacement {
                        target: name.clone(),
                    },
                    all.clone(),
                );
            } else if let Some(by) = kind_of(OverrideKind::Wrap).and_then(|o| o.over.by.as_ref()) {
                push(
                    &mut plan,
                    &mut lines,
                    by,
                    Some(name),
                    Role::Wrapper {
                        target: name.clone(),
                    },
                    all.clone(),
                );
            } else {
                push(
                    &mut plan,
                    &mut lines,
                    name,
                    None,
                    Role::Declared,
                    all.clone(),
                );
            }
            for o in side(OverrideKind::After) {
                if let Some(by) = &o.over.by {
                    push(
                        &mut plan,
                        &mut lines,
                        by,
                        None,
                        Role::After {
                            target: name.clone(),
                        },
                        vec![applied(o)],
                    );
                }
            }
        }
        let point = points[i].clone();
        plans
            .entry(doctype.to_owned())
            .or_default()
            .insert(point.clone(), plan);
        explains
            .entry(doctype.to_owned())
            .or_default()
            .insert(point, lines);
    }
    (plans, explains)
}

/// The conflict code of a contract refusal.
fn effect_code(code: &str) -> &'static str {
    match code {
        "effects.internal_not_reversible" => codes::EFFECTS_INTERNAL_NOT_REVERSIBLE,
        "effects.internal_writes_nothing" => codes::EFFECTS_INTERNAL_WRITES_NOTHING,
        "effects.irreversible_reason" => codes::EFFECTS_IRREVERSIBLE_REASON,
        "effects.wrong_phase" => codes::EFFECTS_WRONG_PHASE,
        "effects.wrong_reversal" => codes::EFFECTS_WRONG_REVERSAL,
        _ => codes::EFFECTS_INVALID,
    }
}

/// Pairing of forward and reversing effects (ADR-080 6.3): every store an internal effect at
/// `after_<op>` writes, reversible by the pair of `op`, is written by some internal effect at
/// `after_<pair>` of the same DocType. If the pairing holds without overrides but not with them,
/// an override dropped the reversal.
fn pairing_conflicts(
    steps: &BTreeMap<StepName, StepInfo>,
    overrides: &[InstalledOverride],
    plans: &Table<PlannedStep>,
    conflicts: &mut Vec<Conflict>,
) {
    let (base_plans, _) = plan_all(steps, &[]);
    let mut doctypes: Vec<&String> = plans.keys().collect();
    doctypes.sort();
    for doctype in doctypes {
        for (op, rev) in pairs() {
            let fwd_point = after(op);
            let rev_point = after(rev);
            let plan_at = |t: &Table<PlannedStep>, p: &HookPoint| -> Vec<PlannedStep> {
                t.get(doctype)
                    .and_then(|m| m.get(p))
                    .cloned()
                    .unwrap_or_default()
            };
            let forward = plan_at(plans, &fwd_point);
            let covered = plan_at(plans, &rev_point);
            let covered = internal_stores(&covered);
            let base_rev = plan_at(&base_plans, &rev_point);
            let base_covered = internal_stores(&base_rev);
            for p in &forward {
                for (step, app, e) in planned_effects(p) {
                    if e.kind != EffectKind::Internal || e.reversal != Reversal::ReversibleBy(rev) {
                        continue;
                    }
                    let missing: Vec<&StoreName> =
                        e.writes.iter().filter(|w| !covered.contains(w)).collect();
                    if missing.is_empty() {
                        continue;
                    }
                    let list = missing
                        .iter()
                        .map(|m| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let dropped = missing.iter().all(|m| base_covered.contains(*m));
                    if !dropped {
                        conflicts.push(Conflict::new(
                            codes::EFFECTS_NO_PAIRED_REVERSAL,
                            format!(
                                "step `{step}` (DocType `{doctype}`) writes {list} at {} and is reversible by {}, but no step at {} reverses {list}",
                                fwd_point.name(),
                                spot_app_api::lifecycle::op_name(rev),
                                rev_point.name()
                            ),
                            &[app],
                        ));
                        continue;
                    }
                    // The reversing steps the baseline had; the overrides that target them.
                    let reversers: BTreeSet<&StepName> = base_rev
                        .iter()
                        .flat_map(planned_effects)
                        .filter(|(_, _, re)| {
                            re.kind == EffectKind::Internal
                                && re.writes.iter().any(|w| missing.contains(&w))
                        })
                        .map(|(n, _, _)| n)
                        .collect();
                    let culprits: Vec<&InstalledOverride> = overrides
                        .iter()
                        .filter(|o| reversers.contains(&o.over.target))
                        .collect();
                    let mut apps: Vec<&str> = vec![app];
                    apps.extend(culprits.iter().map(|o| o.app.as_str()));
                    let by = culprits
                        .iter()
                        .map(|o| {
                            format!(
                                "{:?} of `{}` by app `{}`",
                                o.over.kind, o.over.target, o.app
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    conflicts.push(Conflict::new(
                        codes::EFFECTS_OVERRIDE_DROPS_REVERSAL,
                        format!(
                            "step `{step}` (DocType `{doctype}`) still writes {list} at {}, but an override removed the step that reverses it at {}: {by}",
                            fwd_point.name(),
                            rev_point.name()
                        ),
                        &apps,
                    ));
                }
            }
        }
    }
}

/// The overrides whose effect on the effects that happen is not nothing, sorted.
fn effect_changes(
    steps: &BTreeMap<StepName, StepInfo>,
    overrides: &[InstalledOverride],
) -> Vec<EffectChange> {
    let mut out = Vec::new();
    for io in overrides {
        let Some(target) = steps.get(&io.over.target) else {
            continue;
        };
        let by = io
            .over
            .by
            .as_ref()
            .and_then(|b| steps.get(b))
            .map(|b| b.effects.clone())
            .unwrap_or_default();
        let (removed, added) = match io.over.kind {
            OverrideKind::Disable => (target.effects.clone(), Vec::new()),
            OverrideKind::Replace => (target.effects.clone(), by),
            OverrideKind::Wrap | OverrideKind::Before | OverrideKind::After => (Vec::new(), by),
        };
        // A replacement that declares exactly what the target did changes nothing.
        let (removed, added) = if io.over.kind == OverrideKind::Replace && removed == added {
            (Vec::new(), Vec::new())
        } else {
            (removed, added)
        };
        if removed.is_empty() && added.is_empty() {
            continue;
        }
        out.push(EffectChange {
            app: io.app.clone(),
            kind: crate::report::kind_of(io.over.kind),
            target_step: io.over.target.to_string(),
            doctype: target.doctype.clone(),
            hook_point: target.point.name(),
            removed,
            added,
        });
    }
    out.sort_by(|a, b| {
        (&a.app, a.kind, &a.target_step, &a.doctype, &a.hook_point).cmp(&(
            &b.app,
            b.kind,
            &b.target_step,
            &b.doctype,
            &b.hook_point,
        ))
    });
    out
}
