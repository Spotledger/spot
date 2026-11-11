//! `resolve`: match every import to an export by semver range, check the compose graph is
//! acyclic, refuse capability escalation, and give the provider-before-consumer order.
//!
//! The compose graph has one node per app and one edge **provider → consumer** for every import
//! that resolved, so a topological order puts providers before the apps that extend them. A
//! cycle (two apps extending each other, directly or transitively) is refused with the witness
//! cycle from [`spot_graph_algo`]. Everything is reported at once: one call reports every
//! missing provider, version conflict, cycle and escalation it found.

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use spot_graph_algo::{AdjacencyGraph, GraphError, Limits, topological_order};

use crate::interface::{AppInterfaces, Capability, ExportedInterface};
use crate::problem::{Problem, Problems, codes};

/// One import that matched an export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The importing app.
    pub importer: String,
    /// The interface it imports.
    pub interface: String,
    /// The app that exports it.
    pub provider: String,
    /// The exact version chosen.
    pub version: Version,
    /// Whether the import was optional.
    pub optional: bool,
}

/// What `resolve` found: the order to install in, the bindings, the capabilities each app
/// obtains through its providers, and the candidate's blast radius.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// Every app (installed plus the candidate), providers before the apps that import them.
    pub order: Vec<String>,
    /// The import-to-export matches, sorted.
    pub bindings: Vec<Binding>,
    /// The capabilities each app obtains through the interfaces it imports, sorted.
    pub obtained: BTreeMap<String, Vec<Capability>>,
    /// The apps that depend on the candidate, directly or transitively, sorted.
    pub blast_radius: Vec<String>,
}

impl Resolution {
    /// The capabilities `app` obtains through its providers, sorted; empty when it imports none.
    #[must_use]
    pub fn obtained_by(&self, app: &str) -> &[Capability] {
        self.obtained.get(app).map_or(&[], Vec::as_slice)
    }
}

/// Resolves `candidate` against the `installed` apps.
///
/// # Errors
///
/// A [`Problems`] holding every problem found: a duplicate app or export, a missing provider, a
/// version conflict, a cycle in the compose graph, a capability escalation or a grant the
/// provider does not hold.
pub fn resolve(
    installed: &[AppInterfaces],
    candidate: &AppInterfaces,
) -> Result<Resolution, Problems> {
    let mut problems: Vec<Problem> = Vec::new();

    // --- index the apps, refusing duplicates -----------------------------------------------
    let mut apps: BTreeMap<String, &AppInterfaces> = BTreeMap::new();
    for app in installed {
        if apps.insert(app.app.clone(), app).is_some() {
            problems.push(Problem::new(
                codes::DUPLICATE_APP,
                format!("app `{}` is listed more than once", app.app),
                &[&app.app],
            ));
        }
    }
    if apps.contains_key(&candidate.app) {
        problems.push(Problem::new(
            codes::DUPLICATE_APP,
            format!(
                "app `{}` is already installed, so it cannot be installed again",
                candidate.app
            ),
            &[&candidate.app],
        ));
    } else {
        apps.insert(candidate.app.clone(), candidate);
    }

    // --- index the exports, refusing a duplicate export ------------------------------------
    let mut exports: BTreeMap<String, Vec<&ExportedInterface>> = BTreeMap::new();
    for app in apps.values() {
        for export in &app.exports {
            exports
                .entry(export.interface.clone())
                .or_default()
                .push(export);
        }
    }
    for (interface, list) in &exports {
        let mut seen: BTreeMap<&Version, &str> = BTreeMap::new();
        for export in list {
            if let Some(previous) = seen.insert(&export.version, &export.app) {
                problems.push(Problem::new(
                    codes::DUPLICATE_EXPORT,
                    format!(
                        "apps `{previous}` and `{}` both export interface `{interface}` v{}",
                        export.app, export.version
                    ),
                    &[previous, &export.app, interface],
                ));
            }
        }
    }

    // --- match every import ----------------------------------------------------------------
    let mut bindings: Vec<Binding> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    for app in apps.values() {
        for import in &app.imports {
            let Some(list) = exports.get(&import.interface) else {
                if !import.optional {
                    problems.push(Problem::new(
                        codes::MISSING_PROVIDER,
                        format!(
                            "app `{}` imports interface `{}` {}, which no installed app exports",
                            app.app, import.interface, import.version_range
                        ),
                        &[&app.app],
                    ));
                }
                continue;
            };
            let matching: Vec<&ExportedInterface> = list
                .iter()
                .copied()
                .filter(|export| import.version_range.matches(&export.version))
                .collect();
            if matching.is_empty() {
                let offered: Vec<String> = list
                    .iter()
                    .map(|export| format!("{} (by `{}`)", export.version, export.app))
                    .collect();
                problems.push(Problem::new(
                    codes::VERSION_CONFLICT,
                    format!(
                        "app `{}` needs interface `{}` {}, but the installed providers offer {}",
                        app.app,
                        import.interface,
                        import.version_range,
                        offered.join(", ")
                    ),
                    &[&app.app],
                ));
                continue;
            }
            // The highest matching version wins; a tie is broken by provider name so the
            // choice is independent of the input order.
            let mut best = matching[0];
            for export in &matching {
                if export.version > best.version
                    || (export.version == best.version && export.app < best.app)
                {
                    best = export;
                }
            }
            bindings.push(Binding {
                importer: app.app.clone(),
                interface: import.interface.clone(),
                provider: best.app.clone(),
                version: best.version.clone(),
                optional: import.optional,
            });
            edges.push((best.app.clone(), app.app.clone()));
        }
    }
    bindings.sort_by(|a, b| {
        (&a.importer, &a.interface, &a.provider).cmp(&(&b.importer, &b.interface, &b.provider))
    });

    // --- the compose graph: acyclic, providers first ---------------------------------------
    let names: Vec<String> = apps.keys().cloned().collect();
    let index: BTreeMap<&str, u32> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i as u32))
        .collect();
    let edge_idx: Vec<(u32, u32)> = edges
        .iter()
        .filter_map(|(from, to)| Some((*index.get(from.as_str())?, *index.get(to.as_str())?)))
        .collect();
    let mut order = names.clone();
    if let Ok(graph) = AdjacencyGraph::from_edges(names.len(), &edge_idx) {
        match topological_order(&graph, &Limits::UNLIMITED) {
            Ok(topo) => {
                order = topo.iter().map(|i| names[*i as usize].clone()).collect();
            }
            Err(GraphError::Cycle(cycle)) => {
                let path: Vec<String> = cycle
                    .nodes
                    .iter()
                    .map(|n| names[*n as usize].clone())
                    .collect();
                let apps: Vec<&str> = path.iter().map(String::as_str).collect();
                problems.push(Problem::new(
                    codes::CYCLE,
                    format!("the compose graph is cyclic: {}", path.join(" -> ")),
                    &apps,
                ));
            }
            Err(_) => {}
        }
    }

    // --- capabilities: escalation and grants ------------------------------------------------
    let obtained = obtained_capabilities(&bindings, &exports);
    check_capabilities(&apps, &bindings, &obtained, &mut problems);

    // --- blast radius: who depends on the candidate ----------------------------------------
    let blast_radius = blast_radius(&candidate.app, &edges);

    if problems.is_empty() {
        Ok(Resolution {
            order,
            bindings,
            obtained: obtained
                .into_iter()
                .map(|(app, set)| (app, set.into_iter().collect()))
                .collect(),
            blast_radius,
        })
    } else {
        Err(Problems::new(problems))
    }
}

/// The capabilities each app obtains through the interfaces it imports, keyed by app.
fn obtained_capabilities(
    bindings: &[Binding],
    exports: &BTreeMap<String, Vec<&ExportedInterface>>,
) -> BTreeMap<String, BTreeSet<Capability>> {
    let mut obtained: BTreeMap<String, BTreeSet<Capability>> = BTreeMap::new();
    for binding in bindings {
        let grants = exports
            .get(&binding.interface)
            .into_iter()
            .flatten()
            .find(|export| export.app == binding.provider && export.version == binding.version)
            .map(|export| export.grants.iter().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        obtained
            .entry(binding.importer.clone())
            .or_default()
            .extend(grants);
    }
    obtained
}

/// Reports a capability an importing app declares that no provider grants, and a grant a
/// provider does not hold.
fn check_capabilities(
    apps: &BTreeMap<String, &AppInterfaces>,
    bindings: &[Binding],
    obtained: &BTreeMap<String, BTreeSet<Capability>>,
    problems: &mut Vec<Problem>,
) {
    let importers: BTreeSet<&str> = bindings.iter().map(|b| b.importer.as_str()).collect();
    for app in apps.values() {
        let declared: BTreeSet<Capability> = Capability::from_capabilities(&app.declared)
            .into_iter()
            .collect();
        let obtained_here = obtained.get(&app.app).cloned().unwrap_or_default();

        // An app that extends another obtains capabilities only through its providers: what it
        // declares must be granted by an interface it imports.
        if importers.contains(app.app.as_str()) {
            for capability in &declared {
                if !obtained_here.contains(capability) {
                    problems.push(Problem::new(
                        codes::CAPABILITY_ESCALATION,
                        format!(
                            "app `{}` declares {} but no provider it imports grants it",
                            app.app,
                            capability.describe()
                        ),
                        &[&app.app],
                    ));
                }
            }
        }

        // A provider may grant only what it holds: its own declared capabilities plus what it
        // obtained from the providers it imports.
        let mut held = declared.clone();
        held.extend(obtained_here.iter().cloned());
        for export in &app.exports {
            for grant in &export.grants {
                if !held.contains(grant) {
                    problems.push(Problem::new(
                        codes::GRANT_NOT_HELD,
                        format!(
                            "app `{}` grants {} through interface `{}` but does not hold it",
                            app.app,
                            grant.describe(),
                            export.interface
                        ),
                        &[&app.app],
                    ));
                }
            }
        }
    }
}

/// The apps that depend on `app`, directly or transitively, sorted and without `app` itself.
fn blast_radius(app: &str, edges: &[(String, String)]) -> Vec<String> {
    // Reverse the provider → consumer edges and walk from `app` to its consumers.
    let mut consumers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (provider, consumer) in edges {
        consumers
            .entry(provider.as_str())
            .or_default()
            .push(consumer.as_str());
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = consumers.get(app).cloned().unwrap_or_default();
    while let Some(node) = stack.pop() {
        if seen.insert(node) {
            stack.extend(consumers.get(node).into_iter().flatten().copied());
        }
    }
    seen.into_iter()
        .filter(|name| *name != app)
        .map(str::to_owned)
        .collect()
}
