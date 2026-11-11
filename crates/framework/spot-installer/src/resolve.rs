//! Building a plan: request checks, exhaustive dependency resolution, and the ordered actions.
//!
//! # The resolution algorithm
//!
//! The catalogue is small, so the planner searches it exhaustively rather than heuristically.
//! It is a backtracking search over a constraint store:
//!
//! - **Variables** are the apps that must have a version in the world after the plan. They start
//!   as the site's installed apps (minus the ones the request removes) plus the app a request
//!   targets, and grow as each placed app's `depends` introduce new apps.
//! - **Constraints** are the `(requirer, VersionReq)` pairs every app's `depends` place on those
//!   variables. An installed app contributes its dependencies; a placed candidate contributes the
//!   dependencies its own manifest declares.
//! - **Candidate order** is the preference: a requested app (or a dependency still to be added)
//!   tries its highest catalogue version first; an app already installed tries its installed
//!   version first and only then higher catalogue versions, so the planner never changes an app it
//!   does not have to. This is what "the highest version satisfying every requirement" means here.
//! - The search assigns variables in name order, trying candidates in preference order, and
//!   rejects a candidate the moment it violates a known constraint or one of its own dependencies
//!   is already placed at an incompatible version. On failure it backtracks, so a lower version of
//!   one app is found when the highest would break a dependent, never silently rejected.
//! - A dependency the request removes, a dependency with no catalogue bundle, or a set of ranges
//!   with no satisfying version each produce their own refusal (see [`crate::PlanError`]).
//!
//! Once every variable holds a version, the dependency graph is walked in post-order to give the
//! action order (dependencies first); a cycle in that walk is `installer.dependency_cycle`.
//! Uninstalls are emitted in reverse install order.

use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};
use spot_bundle::Digest;
use spot_manifest::{AppDependency, PatchIntent, PatchSub};

use crate::error::{
    ALREADY_INSTALLED, BREAKS_DEPENDENT, CONFLICTING_REQUIREMENT, DEPENDENCY_CYCLE, DIGEST_CHANGED,
    DOWNGRADE, DOWNGRADE_UNSAFE, HAS_DEPENDENTS, MISSING_DEPENDENCY, NO_CANDIDATE, NOT_INSTALLED,
    PATCH_REMOVED, REQUIRES_HOST, UNVERIFIED, WIT_WORLD_MISMATCH,
};
use crate::model::{
    Approval, Candidate, CapabilityDelta, CapabilityRequest, Catalogue, GrantedCapabilities, Host,
    InstalledApp, PlanState, Request, SiteState,
};
use crate::plan::{Action, InstallPlan, Operation, PatchStep, Risk};

/// Builds the install, upgrade or uninstall plan for one site.
///
/// The plan is deterministic: the same inputs always give the same actions and the same
/// [`InstallPlan::hash`]. It reads nothing but its arguments (no storage, no clock, no network)
/// and runs nothing.
///
/// # Errors
/// A [`crate::PlanError`] naming the first refusal, with a stable `installer.*` code.
pub fn plan(
    site: &SiteState,
    catalogue: &Catalogue,
    host: &Host,
    request: &Request,
    approval: &Approval,
) -> Result<InstallPlan, crate::PlanError> {
    let installed: BTreeMap<String, &InstalledApp> =
        site.apps.iter().map(|a| (a.name.clone(), a)).collect();
    let mut intent: BTreeMap<String, Intent> = BTreeMap::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut uninstall_purge = false;

    match request {
        Request::Install { app, version_req } => {
            if installed.contains_key(app) {
                return Err(crate::PlanError::new(
                    ALREADY_INSTALLED,
                    format!("`{app}` is already installed on this site"),
                ));
            }
            intent.insert(
                app.clone(),
                Intent::Install {
                    req: version_req.clone(),
                },
            );
        }
        Request::Upgrade { app, to } => {
            let Some(current) = installed.get(app) else {
                return Err(crate::PlanError::new(
                    NOT_INSTALLED,
                    format!("`{app}` is not installed, so it cannot be upgraded"),
                ));
            };
            if let Some(target) = to {
                if target < &current.version {
                    return Err(crate::PlanError::new(
                        DOWNGRADE,
                        format!(
                            "`{app}` would move from {} down to {target} without a downgrade request",
                            current.version
                        ),
                    ));
                }
                check_dependents(app, target, &installed)?;
            }
            intent.insert(app.clone(), Intent::Upgrade { to: to.clone() });
        }
        Request::Downgrade { app, to } => {
            let Some(current) = installed.get(app) else {
                return Err(crate::PlanError::new(
                    NOT_INSTALLED,
                    format!("`{app}` is not installed, so it cannot be downgraded"),
                ));
            };
            if to >= &current.version {
                return Err(crate::PlanError::new(
                    DOWNGRADE,
                    format!("`{app}` is at {} and {to} is not lower", current.version),
                ));
            }
            check_dependents(app, to, &installed)?;
            intent.insert(app.clone(), Intent::Downgrade { to: to.clone() });
        }
        Request::Uninstall { app, purge } => {
            if !installed.contains_key(app) {
                return Err(crate::PlanError::new(
                    NOT_INSTALLED,
                    format!("`{app}` is not installed, so it cannot be uninstalled"),
                ));
            }
            let dependents: Vec<&str> = installed
                .values()
                .filter(|a| a.name != *app)
                .filter(|a| a.depends.iter().any(|d| d.app.as_str() == app))
                .map(|a| a.name.as_str())
                .collect();
            if !dependents.is_empty() {
                return Err(crate::PlanError::new(
                    HAS_DEPENDENTS,
                    format!(
                        "`{app}` cannot be uninstalled while {} depends on it",
                        dependents.join(", ")
                    ),
                ));
            }
            removed.insert(app.clone());
            uninstall_purge = *purge;
        }
        Request::UpgradeAll => {
            for a in installed.values() {
                intent.insert(a.name.clone(), Intent::Upgrade { to: None });
            }
        }
    }

    let solver = Solver {
        index: Index::new(&catalogue.candidates),
        installed,
        removed,
        uninstall_purge,
        intent,
        host,
        approval,
    };
    let mut state = State::default();
    solver.solve(&mut state)?;
    solver.finish(&state)
}

/// Refuses when an explicit target version breaks an installed app's requirement on `app`.
fn check_dependents(
    app: &str,
    target: &Version,
    installed: &BTreeMap<String, &InstalledApp>,
) -> Result<(), crate::PlanError> {
    for other in installed.values() {
        if other.name == app {
            continue;
        }
        for dep in &other.depends {
            if dep.app.as_str() == app && !dep.version.matches(target) {
                return Err(crate::PlanError::new(
                    BREAKS_DEPENDENT,
                    format!(
                        "moving `{app}` to {target} breaks `{}`, which needs {}",
                        other.name, dep.version
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// How a requested app is being changed.
#[derive(Clone, Debug)]
enum Intent {
    /// Install, at the highest version matching the request.
    Install { req: VersionReq },
    /// Upgrade, to a named version or the highest compatible one.
    Upgrade { to: Option<Version> },
    /// Downgrade to a named lower version.
    Downgrade { to: Version },
}

/// The catalogue, indexed by app and sorted highest version first.
struct Index<'a> {
    candidates: &'a [Candidate],
    by_app: BTreeMap<String, Vec<usize>>,
}

impl<'a> Index<'a> {
    /// Indexes the candidates, sorting each app's versions descending (ties by digest) so the
    /// search is independent of the catalogue's order.
    fn new(candidates: &'a [Candidate]) -> Self {
        let mut by_app: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, candidate) in candidates.iter().enumerate() {
            by_app
                .entry(candidate.app().to_owned())
                .or_default()
                .push(i);
        }
        for refs in by_app.values_mut() {
            refs.sort_by(|a, b| {
                candidates[*b]
                    .version()
                    .cmp(candidates[*a].version())
                    .then_with(|| {
                        candidates[*a]
                            .digest
                            .to_hex()
                            .cmp(&candidates[*b].digest.to_hex())
                    })
            });
        }
        Self { candidates, by_app }
    }

    /// The candidate indices for `app`, highest version first.
    fn refs(&self, app: &str) -> &[usize] {
        self.by_app.get(app).map_or(&[], Vec::as_slice)
    }
}

/// One app's chosen version in the world the plan builds.
#[derive(Clone)]
struct Placed {
    /// The version.
    version: Version,
    /// The bundle's canonical digest.
    digest: Digest,
    /// The dependencies the chosen version declares.
    depends: Vec<AppDependency>,
    /// The app's display title.
    title: String,
    /// The catalogue index the version came from; `None` for an unchanged installed app.
    candidate: Option<usize>,
}

/// The search state: what is placed and the constraints seen so far.
#[derive(Default)]
struct State {
    placed: BTreeMap<String, Placed>,
    constraints: BTreeMap<String, Vec<(String, VersionReq)>>,
}

/// The exhaustive resolver.
struct Solver<'a> {
    index: Index<'a>,
    installed: BTreeMap<String, &'a InstalledApp>,
    removed: BTreeSet<String>,
    /// Whether the uninstall request also deletes the app's data.
    uninstall_purge: bool,
    intent: BTreeMap<String, Intent>,
    host: &'a Host,
    approval: &'a Approval,
}

impl Solver<'_> {
    /// Searches for a version for every variable, backtracking on conflicts.
    fn solve(&self, state: &mut State) -> Result<(), crate::PlanError> {
        let Some(app) = self.next_unplaced(state) else {
            return Ok(());
        };
        let candidates = self.candidates(&app);
        if candidates.is_empty() {
            return Err(self.no_candidates_error(&app, state));
        }
        let mut last: Option<crate::PlanError> = None;
        for candidate in &candidates {
            if !self.satisfied(&app, &candidate.version, state) {
                continue;
            }
            match self.place(&app, candidate, state) {
                Ok(()) => match self.solve(state) {
                    Ok(()) => return Ok(()),
                    Err(err) => last = Some(err),
                },
                Err(err) => last = Some(err),
            }
            self.undo(&app, state);
        }
        Err(last.unwrap_or_else(|| self.no_candidates_error(&app, state)))
    }

    /// The next variable, in name order.
    fn next_unplaced(&self, state: &State) -> Option<String> {
        self.wanted(state)
            .into_iter()
            .find(|app| !state.placed.contains_key(app))
    }

    /// Every app the world must contain: installed (minus removed), requested, and the
    /// dependencies of anything placed.
    fn wanted(&self, state: &State) -> BTreeSet<String> {
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        for app in self.installed.keys() {
            if !self.removed.contains(app) {
                wanted.insert(app.clone());
            }
        }
        for app in self.intent.keys() {
            wanted.insert(app.clone());
        }
        for placed in state.placed.values() {
            for dep in &placed.depends {
                let name = dep.app.as_str().to_owned();
                if !self.removed.contains(&name) {
                    wanted.insert(name);
                }
            }
        }
        wanted
    }

    /// The versions `app` may take, in preference order.
    fn candidates(&self, app: &str) -> Vec<Placed> {
        let mut out = Vec::new();
        if let Some(intent) = self.intent.get(app) {
            match intent {
                Intent::Install { req } => {
                    for &i in self.index.refs(app) {
                        if req.matches(self.index.candidates[i].version()) {
                            out.push(self.as_placed_candidate(i));
                        }
                    }
                }
                Intent::Upgrade { to } => {
                    let current = self.installed.get(app).map(|a| a.version.clone());
                    for &i in self.index.refs(app) {
                        let version = self.index.candidates[i].version();
                        if let Some(target) = to
                            && version != target
                        {
                            continue;
                        }
                        if let Some(current) = &current
                            && version < current
                        {
                            continue;
                        }
                        out.push(self.as_placed_candidate(i));
                    }
                    if to.is_none()
                        && let Some(installed) = self.installed.get(app)
                    {
                        out.push(self.as_placed_installed(installed));
                    }
                }
                Intent::Downgrade { to } => {
                    for &i in self.index.refs(app) {
                        if self.index.candidates[i].version() == to {
                            out.push(self.as_placed_candidate(i));
                        }
                    }
                }
            }
        } else if let Some(installed) = self.installed.get(app) {
            out.push(self.as_placed_installed(installed));
            for &i in self.index.refs(app) {
                let version = self.index.candidates[i].version();
                if version <= &installed.version {
                    continue;
                }
                out.push(self.as_placed_candidate(i));
            }
        } else {
            for &i in self.index.refs(app) {
                out.push(self.as_placed_candidate(i));
            }
        }
        out
    }

    /// The placed app for a catalogue candidate.
    fn as_placed_candidate(&self, i: usize) -> Placed {
        let candidate = &self.index.candidates[i];
        Placed {
            version: candidate.manifest.app.version.clone(),
            digest: candidate.digest,
            depends: candidate.manifest.app.depends.clone(),
            title: display_title(&candidate.manifest.app.title, candidate.app()),
            candidate: Some(i),
        }
    }

    /// The placed app for an unchanged installed app.
    fn as_placed_installed(&self, app: &InstalledApp) -> Placed {
        Placed {
            version: app.version.clone(),
            digest: app.digest,
            depends: app.depends.clone(),
            title: app.name.clone(),
            candidate: None,
        }
    }

    /// Whether every constraint currently on `app` accepts `version`.
    fn satisfied(&self, app: &str, version: &Version, state: &State) -> bool {
        state
            .constraints
            .get(app)
            .is_none_or(|cs| cs.iter().all(|(_, req)| req.matches(version)))
    }

    /// Places `app` and records the constraints its dependencies add; `Err` leaves the placement
    /// to be undone by the caller.
    fn place(
        &self,
        app: &str,
        candidate: &Placed,
        state: &mut State,
    ) -> Result<(), crate::PlanError> {
        state.placed.insert(app.to_owned(), candidate.clone());
        for dep in &candidate.depends {
            let name = dep.app.as_str().to_owned();
            if self.removed.contains(&name) {
                return Err(crate::PlanError::new(
                    MISSING_DEPENDENCY,
                    format!("`{app}` needs `{name}`, which this plan removes"),
                ));
            }
            if let Some(placed) = state.placed.get(&name)
                && !dep.version.matches(&placed.version)
            {
                return Err(crate::PlanError::new(
                    CONFLICTING_REQUIREMENT,
                    format!(
                        "`{app}` needs `{name}` {} but {} was chosen",
                        dep.version, placed.version
                    ),
                ));
            }
            state
                .constraints
                .entry(name)
                .or_default()
                .push((app.to_owned(), dep.version.clone()));
        }
        Ok(())
    }

    /// Removes a placement and the constraints it added.
    fn undo(&self, app: &str, state: &mut State) {
        let Some(placed) = state.placed.remove(app) else {
            return;
        };
        for dep in &placed.depends {
            let name = dep.app.as_str();
            if let Some(cs) = state.constraints.get_mut(name) {
                if let Some(pos) = cs.iter().rposition(|(requirer, _)| requirer == app) {
                    cs.remove(pos);
                }
                if cs.is_empty() {
                    state.constraints.remove(name);
                }
            }
        }
    }

    /// Why `app` has no usable version, phrased as the most specific refusal.
    fn no_candidates_error(&self, app: &str, state: &State) -> crate::PlanError {
        if let Some(Intent::Install { req }) = self.intent.get(app) {
            return crate::PlanError::new(
                NO_CANDIDATE,
                format!("the catalogue has no version of `{app}` matching {req}"),
            );
        }
        if let Some(Intent::Upgrade { to: Some(target) }) = self.intent.get(app) {
            return crate::PlanError::new(
                NO_CANDIDATE,
                format!("the catalogue has no version {target} of `{app}`"),
            );
        }
        if let Some(Intent::Downgrade { to }) = self.intent.get(app) {
            return crate::PlanError::new(
                NO_CANDIDATE,
                format!("the catalogue has no version {to} of `{app}`"),
            );
        }
        let constraints = state.constraints.get(app).cloned().unwrap_or_default();
        if self.index.refs(app).is_empty() && !self.installed.contains_key(app) {
            let (requirer, req) = constraints
                .first()
                .map(|(requirer, req)| (requirer.clone(), req.to_string()))
                .unwrap_or_else(|| ("the request".to_owned(), "*".to_owned()));
            return crate::PlanError::new(
                MISSING_DEPENDENCY,
                format!(
                    "`{app}` is required by `{requirer}` ({req}) but the catalogue has no bundle \
                     for it"
                ),
            );
        }
        if let Some((dependent, _)) = constraints
            .iter()
            .find(|(requirer, _)| self.installed.contains_key(requirer.as_str()))
        {
            return crate::PlanError::new(
                BREAKS_DEPENDENT,
                format!("changing `{app}` would break the installed app `{dependent}`"),
            );
        }
        let requirers = constraints
            .iter()
            .map(|(requirer, req)| format!("`{requirer}` needs {req}"))
            .collect::<Vec<_>>()
            .join(", ");
        crate::PlanError::new(
            CONFLICTING_REQUIREMENT,
            format!("no available version of `{app}` satisfies every requirement ({requirers})"),
        )
    }

    /// Builds the plan from a solved state.
    fn finish(&self, state: &State) -> Result<InstallPlan, crate::PlanError> {
        let order = self.order_world(state)?;
        let mut actions: Vec<Action> = Vec::new();
        let mut revocation_generation = 0_u64;
        for app in &order {
            if self.removed.contains(app) {
                continue;
            }
            let Some(placed) = state.placed.get(app) else {
                continue;
            };
            match self.installed.get(app) {
                None => {
                    let (action, generation) = self.action_for(app, None, placed, false)?;
                    revocation_generation = revocation_generation.max(generation);
                    actions.push(action);
                }
                Some(current) => {
                    if placed.version == current.version {
                        if placed.digest != current.digest {
                            return Err(crate::PlanError::new(
                                DIGEST_CHANGED,
                                format!(
                                    "`{app}` {} is already installed with a different bundle digest",
                                    placed.version
                                ),
                            ));
                        }
                        continue;
                    }
                    let downgrade = placed.version < current.version;
                    if downgrade && !matches!(self.intent.get(app), Some(Intent::Downgrade { .. }))
                    {
                        return Err(crate::PlanError::new(
                            DOWNGRADE,
                            format!(
                                "`{app}` would move from {} down to {} without a downgrade request",
                                current.version, placed.version
                            ),
                        ));
                    }
                    let (action, generation) =
                        self.action_for(app, Some(current), placed, downgrade)?;
                    revocation_generation = revocation_generation.max(generation);
                    actions.push(action);
                }
            }
        }
        actions.extend(self.uninstall_actions());
        let has_downgrade = actions
            .iter()
            .any(|action| matches!(&action.op, Operation::Upgrade { from, to, .. } if to < from));
        if has_downgrade
            && actions.iter().any(|action| {
                matches!(
                    action.patch_intent,
                    Some(PatchIntent::Schema) | Some(PatchIntent::Data)
                )
            })
        {
            return Err(crate::PlanError::new(
                DOWNGRADE_UNSAFE,
                "a downgrade plan cannot run a data or schema patch",
            ));
        }
        let plan_state = approval_state(&actions);
        Ok(InstallPlan::build(
            actions,
            plan_state,
            self.host.version.clone(),
            revocation_generation,
        ))
    }

    /// The uninstall actions, in reverse install order.
    fn uninstall_actions(&self) -> Vec<Action> {
        let mut removed: Vec<&InstalledApp> = self
            .installed
            .values()
            .copied()
            .filter(|a| self.removed.contains(&a.name))
            .collect();
        removed.sort_by_key(|a| std::cmp::Reverse(a.order));
        removed
            .into_iter()
            .map(|app| Action {
                op: Operation::Uninstall {
                    app: app.name.clone(),
                    purge: self.uninstall_purge,
                },
                title: app.name.clone(),
                patches: Vec::new(),
                fixtures_to_load: Vec::new(),
                risk: if self.uninstall_purge {
                    Risk::High
                } else {
                    Risk::Low
                },
                reversible: !self.uninstall_purge,
                capability_delta: CapabilityDelta::default(),
                capabilities: GrantedCapabilities::default(),
                patch_intent: None,
            })
            .collect()
    }

    /// Builds one install or upgrade action and its verification generation.
    fn action_for(
        &self,
        app: &str,
        current: Option<&InstalledApp>,
        placed: &Placed,
        downgrade: bool,
    ) -> Result<(Action, u64), crate::PlanError> {
        let Some(i) = placed.candidate else {
            return Err(crate::PlanError::new(
                CONFLICTING_REQUIREMENT,
                format!("`{app}` has no catalogue candidate to install"),
            ));
        };
        let candidate = &self.index.candidates[i];
        if !candidate.verified.ok {
            return Err(crate::PlanError::new(
                UNVERIFIED,
                format!("the bundle for `{app}` {} is not verified", placed.version),
            ));
        }
        if let Some(req) = &candidate.manifest.app.requires_host
            && !req.matches(&self.host.version)
        {
            return Err(crate::PlanError::new(
                REQUIRES_HOST,
                format!(
                    "`{app}` {} needs a host in {req}; this host is {}",
                    placed.version, self.host.version
                ),
            ));
        }
        if candidate.wit_world != self.host.wit_world {
            return Err(crate::PlanError::new(
                WIT_WORLD_MISMATCH,
                format!(
                    "`{app}` {} targets the WIT world `{}`; this host serves `{}`",
                    placed.version, candidate.wit_world, self.host.wit_world
                ),
            ));
        }
        let manifest = &candidate.manifest;
        let applied: BTreeSet<&str> = current
            .map(|a| a.applied_patches.iter().map(String::as_str).collect())
            .unwrap_or_default();
        if let Some(current) = current {
            for id in &current.applied_patches {
                if !manifest.patches.iter().any(|p| &p.id == id) {
                    return Err(crate::PlanError::new(
                        PATCH_REMOVED,
                        format!(
                            "`{app}` {} no longer declares the applied patch `{id}`",
                            placed.version
                        ),
                    ));
                }
            }
        }
        let run: Vec<&PatchSub> = manifest
            .patches
            .iter()
            .filter(|p| !applied.contains(p.id.as_str()))
            .collect();
        let mut risk = Risk::Low;
        let mut patch_intent: Option<PatchIntent> = None;
        for patch in &run {
            patch_intent = Some(match (patch_intent, patch.intent) {
                (Some(PatchIntent::Schema), _) | (_, PatchIntent::Schema) => PatchIntent::Schema,
                (Some(PatchIntent::Data), _) | (_, PatchIntent::Data) => PatchIntent::Data,
                _ => PatchIntent::Metadata,
            });
            match patch.intent {
                PatchIntent::Schema => risk = Risk::High,
                PatchIntent::Data => {
                    if risk < Risk::Medium {
                        risk = Risk::Medium;
                    }
                }
                PatchIntent::Metadata => {}
            }
            if !patch.reversible {
                risk = Risk::High;
            }
        }
        let reversible = run.iter().all(|p| p.reversible);
        if downgrade {
            risk = Risk::High;
            if run
                .iter()
                .any(|p| matches!(p.intent, PatchIntent::Schema | PatchIntent::Data))
            {
                return Err(crate::PlanError::new(
                    DOWNGRADE_UNSAFE,
                    format!(
                        "downgrading `{app}` to {} would run a data or schema patch",
                        placed.version
                    ),
                ));
            }
        }
        let op = match current {
            None => Operation::Install {
                app: app.to_owned(),
                version: placed.version.clone(),
                digest: placed.digest,
            },
            Some(current) => Operation::Upgrade {
                app: app.to_owned(),
                from: current.version.clone(),
                to: placed.version.clone(),
                digest: placed.digest,
            },
        };
        let action = Action {
            op,
            title: placed.title.clone(),
            patches: run
                .iter()
                .map(|p| PatchStep {
                    id: p.id.clone(),
                    reversible: p.reversible,
                })
                .collect(),
            fixtures_to_load: manifest
                .fixtures
                .iter()
                .map(|f| f.file.as_str().to_owned())
                .collect(),
            risk,
            reversible,
            capability_delta: CapabilityDelta::of(
                &manifest.capabilities,
                &self.approval.approved(app),
            ),
            capabilities: GrantedCapabilities {
                network: manifest.capabilities.network.clone(),
                email: manifest.capabilities.email,
                files: manifest.capabilities.files,
                doctypes: manifest.capabilities.doctypes.clone(),
            },
            patch_intent,
        };
        Ok((action, candidate.verified.revocation_generation))
    }

    /// The world's apps in dependency-first order; a cycle is a refusal.
    fn order_world(&self, state: &State) -> Result<Vec<String>, crate::PlanError> {
        let mut order = Vec::new();
        let mut colour: BTreeMap<String, u8> = BTreeMap::new();
        let mut stack: Vec<String> = Vec::new();
        for app in state.placed.keys() {
            if colour.get(app).copied().unwrap_or(0) == 0 {
                self.visit(app, state, &mut colour, &mut order, &mut stack)?;
            }
        }
        Ok(order)
    }

    /// Post-order visit of one app; `colour` is 0 unseen, 1 on the stack, 2 done.
    fn visit(
        &self,
        app: &str,
        state: &State,
        colour: &mut BTreeMap<String, u8>,
        order: &mut Vec<String>,
        stack: &mut Vec<String>,
    ) -> Result<(), crate::PlanError> {
        colour.insert(app.to_owned(), 1);
        stack.push(app.to_owned());
        if let Some(placed) = state.placed.get(app) {
            for dep in &placed.depends {
                let name = dep.app.as_str();
                if !state.placed.contains_key(name) {
                    continue;
                }
                match colour.get(name).copied().unwrap_or(0) {
                    0 => self.visit(name, state, colour, order, stack)?,
                    1 => {
                        let cycle: Vec<String> = stack
                            .iter()
                            .skip_while(|item| item.as_str() != name)
                            .cloned()
                            .collect();
                        let mut shown = cycle;
                        shown.push(name.to_owned());
                        return Err(crate::PlanError::new(
                            DEPENDENCY_CYCLE,
                            format!("the dependencies form a cycle: {}", shown.join(" -> ")),
                        ));
                    }
                    _ => {}
                }
            }
        }
        stack.pop();
        colour.insert(app.to_owned(), 2);
        order.push(app.to_owned());
        Ok(())
    }
}

/// The plan state: `NeedsApproval` when any action asks for unapproved capabilities.
fn approval_state(actions: &[Action]) -> PlanState {
    let requests: Vec<CapabilityRequest> = actions
        .iter()
        .filter(|action| !action.capability_delta.is_empty())
        .map(|action| CapabilityRequest {
            app: action.app().to_owned(),
            delta: action.capability_delta.clone(),
        })
        .collect();
    if requests.is_empty() {
        PlanState::Ready
    } else {
        PlanState::NeedsApproval { requests }
    }
}

/// The display title: the manifest's, or the app name when it states none.
fn display_title(title: &str, name: &str) -> String {
    if title.is_empty() {
        name.to_owned()
    } else {
        title.to_owned()
    }
}
