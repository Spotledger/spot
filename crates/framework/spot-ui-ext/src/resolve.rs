//! Resolution: validate every app, merge in dependency order and produce one canonical bundle.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::code::{UiExtError, codes, sort_findings};
use crate::composite;
use crate::facts::{AppContribution, AppFacts, PermissionSet, SiteContext};
use crate::limits::Version;
use crate::limits::{self, ItemKind};
use crate::model::{Action, Component, Panel, Tab, Tile};
use crate::validate::{ParsedDoc, analyse};

/// One app of the resolved bundle, in dependency order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedApp {
    /// The app's name.
    pub app: String,
    /// The version the site runs.
    pub version: String,
}

/// What a resolved item is.
#[derive(Clone, Debug, PartialEq)]
enum Payload {
    Action(Action),
    Panel(Panel),
    Tile(Tile),
    Tab(Tab),
}

/// One item of a slot, with who contributed it and what a permission set must hold to see it.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedItem {
    app: String,
    order: i64,
    requires: Vec<String>,
    payload: Payload,
}

impl ResolvedItem {
    /// The item's namespaced id.
    #[must_use]
    pub fn id(&self) -> &str {
        match &self.payload {
            Payload::Action(action) => &action.id,
            Payload::Panel(panel) => &panel.id,
            Payload::Tile(tile) => &tile.id,
            Payload::Tab(tab) => &tab.id,
        }
    }

    /// The item's kind.
    #[must_use]
    pub fn kind(&self) -> ItemKind {
        match &self.payload {
            Payload::Action(_) => ItemKind::Action,
            Payload::Panel(_) => ItemKind::Panel,
            Payload::Tile(_) => ItemKind::Tile,
            Payload::Tab(_) => ItemKind::Tab,
        }
    }

    /// The app that contributed the item.
    #[must_use]
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The item's order hint (0 when the document had none).
    #[must_use]
    pub fn order(&self) -> i64 {
        self.order
    }

    /// The permissions a viewer must hold for the item to be rendered: the item's own
    /// own `permission` and the permission of every data query it reads, sorted and without
    /// duplicates.
    #[must_use]
    pub fn requires(&self) -> &[String] {
        &self.requires
    }

    /// The action, when the item is one.
    #[must_use]
    pub fn as_action(&self) -> Option<&Action> {
        match &self.payload {
            Payload::Action(action) => Some(action),
            _ => None,
        }
    }

    /// The tab, when the item is one.
    #[must_use]
    pub fn as_tab(&self) -> Option<&Tab> {
        match &self.payload {
            Payload::Tab(tab) => Some(tab),
            _ => None,
        }
    }

    fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("app".to_owned(), Value::String(self.app.clone()));
        object.insert(
            "kind".to_owned(),
            Value::String(self.kind().as_str().to_owned()),
        );
        object.insert("order".to_owned(), Value::from(self.order));
        let payload = match &self.payload {
            Payload::Action(action) => ("action", json(action)),
            Payload::Panel(panel) => ("panel", json(panel)),
            Payload::Tile(tile) => ("tile", json(tile)),
            Payload::Tab(tab) => ("tab", json(tab)),
        };
        object.insert(payload.0.to_owned(), payload.1);
        Value::Object(object)
    }
}

/// One slot's resolved items and the hash of exactly that sub-document.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSlot {
    name: String,
    items: Vec<ResolvedItem>,
    hash: String,
}

impl ResolvedSlot {
    fn new(name: String, items: Vec<ResolvedItem>) -> Self {
        let mut slot = Self {
            name,
            items,
            hash: String::new(),
        };
        slot.rehash();
        slot
    }

    /// The slot's full id.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The slot's items, in render order.
    #[must_use]
    pub fn items(&self) -> &[ResolvedItem] {
        &self.items
    }

    /// The SHA-256 of the slot's canonical sub-document, as lower-case hex.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// The slot's canonical JSON: stable key order, no whitespace.
    #[must_use]
    pub fn to_canonical_json(&self) -> String {
        canonical(&self.to_value())
    }

    fn rehash(&mut self) {
        self.hash = hash_value(&self.value_without_hash());
    }

    fn value_without_hash(&self) -> Value {
        let items: Vec<Value> = self.items.iter().map(ResolvedItem::to_value).collect();
        let mut object = Map::new();
        object.insert("name".to_owned(), Value::String(self.name.clone()));
        object.insert("items".to_owned(), Value::Array(items));
        Value::Object(object)
    }

    fn to_value(&self) -> Value {
        let mut value = self.value_without_hash();
        if let Value::Object(object) = &mut value {
            object.insert("hash".to_owned(), Value::String(self.hash.clone()));
        }
        value
    }
}

/// An item a permission set removed, and why, for the admin and upgrade reports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DroppedItem {
    /// The slot the item would have sat in.
    pub slot: String,
    /// The item's id.
    pub id: String,
    /// The app that contributed it.
    pub app: String,
    /// Why it is missing: the permission the set lacks.
    pub reason: String,
}

/// One site's validated, merged, canonical UI.
///
/// A resolved document is **presentation** (ADR-069 §7). [`for_permissions`](Self::for_permissions)
/// hides what a user may not see, but it is never authorisation: the server checks every operation
/// again, and a client that receives the full document still may not use what it names.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    contract_version: String,
    requires_shell: String,
    shell_contract: String,
    apps: Vec<ResolvedApp>,
    slots: BTreeMap<String, ResolvedSlot>,
    components: BTreeMap<String, Component>,
    dropped: Vec<DroppedItem>,
}

impl Resolved {
    /// The oldest shell that can render every part: the maximum of the parts' `requires_shell`.
    #[must_use]
    pub fn requires_shell(&self) -> &str {
        &self.requires_shell
    }

    /// The contract version the site's shell implements.
    #[must_use]
    pub fn shell_contract(&self) -> &str {
        &self.shell_contract
    }

    /// The apps of the bundle, in dependency order.
    #[must_use]
    pub fn apps(&self) -> &[ResolvedApp] {
        &self.apps
    }

    /// The app names, in dependency order.
    #[must_use]
    pub fn app_order(&self) -> Vec<&str> {
        self.apps.iter().map(|app| app.app.as_str()).collect()
    }

    /// The slots that have items, by name.
    pub fn slots(&self) -> impl Iterator<Item = &ResolvedSlot> {
        self.slots.values()
    }

    /// One slot, when it has items.
    #[must_use]
    pub fn slot(&self, name: &str) -> Option<&ResolvedSlot> {
        self.slots.get(name)
    }

    /// Every component of every app, by id: a tile of one app may render another app's
    /// component when it declares the dependency, so the client needs all of them.
    #[must_use]
    pub fn components(&self) -> &BTreeMap<String, Component> {
        &self.components
    }

    /// One component by id.
    #[must_use]
    pub fn component(&self, id: &str) -> Option<&Component> {
        self.components.get(id)
    }

    /// The items a permission filter removed, sorted, for the admin report.
    #[must_use]
    pub fn dropped(&self) -> &[DroppedItem] {
        &self.dropped
    }

    /// The canonical JSON of the whole bundle: stable key order, no whitespace.
    #[must_use]
    pub fn to_canonical_json(&self) -> String {
        canonical(&self.to_value())
    }

    /// The SHA-256 of [`to_canonical_json`](Self::to_canonical_json), as lower-case hex.
    #[must_use]
    pub fn hash(&self) -> String {
        hash_value(&self.to_value())
    }

    /// The bundle for a shell older than [`requires_shell`](Self::requires_shell), if any.
    #[must_use]
    pub fn shell_upgrade_needed(&self) -> Option<&str> {
        match (
            Version::parse(&self.shell_contract),
            Version::parse(&self.requires_shell),
        ) {
            (Some(shell), Some(required)) if shell < required => Some(&self.requires_shell),
            _ => None,
        }
    }

    /// The same bundle with everything `set` may not see removed.
    #[must_use]
    pub fn for_permissions<S: PermissionSet + ?Sized>(&self, set: &S) -> Self {
        let mut filtered = self.clone();
        filtered.dropped.clear();
        let mut dropped = Vec::new();
        for slot in filtered.slots.values_mut() {
            let mut kept = Vec::with_capacity(slot.items.len());
            for item in slot.items.drain(..) {
                match item.requires.iter().find(|permission| !set.has(permission)) {
                    Some(permission) => dropped.push(DroppedItem {
                        slot: slot.name.clone(),
                        id: item.id().to_owned(),
                        app: item.app.clone(),
                        reason: format!("the permission set lacks {permission}"),
                    }),
                    None => kept.push(item),
                }
            }
            slot.items = kept;
            slot.rehash();
        }
        dropped.sort();
        filtered.dropped = dropped;
        filtered
    }

    /// Who contributed what to `slot`, in render order, and why an item is missing.
    #[must_use]
    pub fn explain_slot(&self, slot: &str) -> Vec<String> {
        let mut lines = Vec::new();
        match self.slots.get(slot) {
            Some(resolved) => {
                lines.push(format!(
                    "{slot}: {} item(s), hash {}",
                    resolved.items.len(),
                    resolved.hash
                ));
                for (position, item) in resolved.items.iter().enumerate() {
                    lines.push(format!(
                        "{}. {} — app {}, kind {}, order {}",
                        position + 1,
                        item.id(),
                        item.app,
                        item.kind().as_str(),
                        item.order
                    ));
                }
            }
            None => {
                if limits::slot(slot).is_some() {
                    lines.push(format!("{slot}: no items"));
                } else {
                    lines.push(format!(
                        "{slot}: not a slot of contract {}",
                        self.contract_version
                    ));
                }
            }
        }
        for item in self.dropped.iter().filter(|item| item.slot == slot) {
            lines.push(format!(
                "missing: {} — app {}: {}",
                item.id, item.app, item.reason
            ));
        }
        lines
    }

    fn to_value(&self) -> Value {
        let apps: Vec<Value> = self
            .apps
            .iter()
            .map(|app| {
                let mut object = Map::new();
                object.insert("app".to_owned(), Value::String(app.app.clone()));
                object.insert("version".to_owned(), Value::String(app.version.clone()));
                Value::Object(object)
            })
            .collect();
        let mut slots = Map::new();
        for (name, slot) in &self.slots {
            slots.insert(name.clone(), slot.to_value());
        }
        let mut components = Map::new();
        for (id, component) in &self.components {
            components.insert(id.clone(), json(component));
        }
        let mut object = Map::new();
        object.insert("apps".to_owned(), Value::Array(apps));
        object.insert("components".to_owned(), Value::Object(components));
        object.insert(
            "contract_version".to_owned(),
            Value::String(self.contract_version.clone()),
        );
        object.insert(
            "requires_shell".to_owned(),
            Value::String(self.requires_shell.clone()),
        );
        object.insert(
            "shell_contract".to_owned(),
            Value::String(self.shell_contract.clone()),
        );
        object.insert("slots".to_owned(), Value::Object(slots));
        Value::Object(object)
    }
}

/// One app's parsed contribution, held while the apps are ordered.
struct Part<'a> {
    contribution: &'a AppContribution,
    docs: Vec<ParsedDoc>,
}

/// A component with the app and file it came from.
struct ComponentMeta<'a> {
    app: &'a str,
    file: &'a str,
    ptr: String,
    component: &'a Component,
}

/// Validates every app and resolves them into one deterministic bundle.
pub fn resolve(
    contributions: &[AppContribution],
    site: &SiteContext,
) -> Result<Resolved, Vec<UiExtError>> {
    let mut errors = Vec::new();
    let mut parts: Vec<Part<'_>> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for contribution in contributions {
        if !seen.insert(contribution.app.clone()) {
            errors.push(
                UiExtError::new(
                    codes::SCHEMA,
                    first_file(contribution),
                    "",
                    format!("app {} contributes twice", contribution.app),
                )
                .with_app(&contribution.app),
            );
            continue;
        }
        let (docs, findings) = analyse(contribution);
        errors.extend(findings);
        parts.push(Part { contribution, docs });
    }
    if !errors.is_empty() {
        sort_findings(&mut errors);
        return Err(errors);
    }

    let order = match dependency_order(&parts) {
        Ok(order) => order,
        Err(mut cycle) => {
            sort_findings(&mut cycle);
            return Err(cycle);
        }
    };
    let components = collect_components(&parts);
    check_cross_app_references(&parts, &components, &mut errors);
    check_cross_app_composites(&components, &mut errors);
    if !errors.is_empty() {
        sort_findings(&mut errors);
        return Err(errors);
    }

    let requires_shell = parts
        .iter()
        .flat_map(|part| &part.docs)
        .filter_map(|doc| Version::parse(&doc.doc.requires_shell))
        .max()
        .map_or_else(
            || limits::CONTRACT_VERSION.to_owned(),
            |version| version.to_string(),
        );

    let mut apps = Vec::new();
    for name in &order {
        let Some(part) = parts.iter().find(|part| part.contribution.app == *name) else {
            continue;
        };
        let version = site
            .version_of(name)
            .unwrap_or(&part.contribution.version)
            .to_owned();
        apps.push(ResolvedApp {
            app: name.clone(),
            version,
        });
    }

    let mut raw: BTreeMap<String, Vec<(usize, ResolvedItem)>> = BTreeMap::new();
    for (rank, name) in order.iter().enumerate() {
        let Some(part) = parts.iter().find(|part| part.contribution.app == *name) else {
            continue;
        };
        let facts = &part.contribution.facts;
        for parsed in &part.docs {
            for action in &parsed.doc.actions {
                push_item(
                    &mut raw,
                    rank,
                    part.contribution.app.as_str(),
                    action.slot.as_str(),
                    action.order.unwrap_or(0),
                    requires_action(action),
                    Payload::Action(action.clone()),
                );
            }
            for panel in &parsed.doc.panels {
                push_item(
                    &mut raw,
                    rank,
                    part.contribution.app.as_str(),
                    panel.slot.as_str(),
                    panel.order.unwrap_or(0),
                    requires_panel(facts, panel),
                    Payload::Panel(panel.clone()),
                );
            }
            for tile in &parsed.doc.tiles {
                push_item(
                    &mut raw,
                    rank,
                    part.contribution.app.as_str(),
                    tile.slot.as_str(),
                    tile.order.unwrap_or(0),
                    requires_tile(facts, tile),
                    Payload::Tile(tile.clone()),
                );
            }
            for tab in &parsed.doc.tabs {
                push_item(
                    &mut raw,
                    rank,
                    part.contribution.app.as_str(),
                    tab.slot.as_str(),
                    tab.order.unwrap_or(0),
                    requires_tab(tab),
                    Payload::Tab(tab.clone()),
                );
            }
        }
    }

    let mut slots = BTreeMap::new();
    for (name, mut items) in raw {
        items.sort_by(|(left_rank, left), (right_rank, right)| {
            (left_rank, left.order, left.app.as_str(), left.id()).cmp(&(
                right_rank,
                right.order,
                right.app.as_str(),
                right.id(),
            ))
        });
        let items: Vec<ResolvedItem> = items.into_iter().map(|(_, item)| item).collect();
        slots.insert(name.clone(), ResolvedSlot::new(name, items));
    }
    let components: BTreeMap<String, Component> = components
        .iter()
        .map(|(id, meta)| (id.clone(), meta.component.clone()))
        .collect();

    Ok(Resolved {
        contract_version: limits::CONTRACT_VERSION.to_owned(),
        requires_shell,
        shell_contract: site.shell_contract.clone(),
        apps,
        slots,
        components,
        dropped: Vec::new(),
    })
}

fn first_file(contribution: &AppContribution) -> String {
    contribution
        .documents
        .first()
        .map_or_else(String::new, |(file, _)| file.clone())
}

/// Adds one item, reporting a cross-app duplicate id rather than letting one replace the other.
fn push_item(
    raw: &mut BTreeMap<String, Vec<(usize, ResolvedItem)>>,
    rank: usize,
    app: &str,
    slot: &str,
    order: i64,
    requires: Vec<String>,
    payload: Payload,
) {
    let item = ResolvedItem {
        app: app.to_owned(),
        order,
        requires,
        payload,
    };
    raw.entry(slot.to_owned()).or_default().push((rank, item));
}

fn requires_action(action: &Action) -> Vec<String> {
    action.permission.iter().cloned().collect()
}

fn requires_panel(facts: &AppFacts, panel: &Panel) -> Vec<String> {
    let mut required: Vec<String> = panel.permission.iter().cloned().collect();
    for block in &panel.blocks {
        let query = match block {
            crate::model::Block::Table { query, .. } | crate::model::Block::Stat { query, .. } => {
                query.as_deref()
            }
            _ => None,
        };
        if let Some(permission) = query.and_then(|query| facts.query_permission(query)) {
            required.push(permission);
        }
    }
    required.sort();
    required.dedup();
    required
}

fn requires_tile(facts: &AppFacts, tile: &Tile) -> Vec<String> {
    let mut required: Vec<String> = tile.permission.iter().cloned().collect();
    for value in tile.props.values() {
        if let crate::model::PropValue::Query { query } = value
            && let Some(permission) = facts.query_permission(query)
        {
            required.push(permission);
        }
    }
    required.sort();
    required.dedup();
    required
}

fn requires_tab(tab: &Tab) -> Vec<String> {
    tab.permission.iter().cloned().collect()
}

/// A topological order over `depends`, ties by app name; apps with no contribution are not
/// part of the order, and a cycle among the rest is reported as `uiext.dependency_cycle`.
fn dependency_order(parts: &[Part<'_>]) -> Result<Vec<String>, Vec<UiExtError>> {
    let names: BTreeSet<&str> = parts
        .iter()
        .map(|part| part.contribution.app.as_str())
        .collect();
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = names
        .iter()
        .map(|name| {
            let part = parts.iter().find(|part| part.contribution.app == *name);
            let declared = part
                .map(|part| {
                    part.contribution
                        .depends
                        .iter()
                        .map(String::as_str)
                        .filter(|dependency| names.contains(dependency))
                        .collect()
                })
                .unwrap_or_default();
            (*name, declared)
        })
        .collect();
    let mut order = Vec::new();
    loop {
        let next = deps
            .iter()
            .find(|(_, dependencies)| dependencies.is_empty())
            .map(|(name, _)| *name);
        let Some(next) = next else {
            break;
        };
        deps.remove(next);
        order.push(next.to_owned());
        for dependencies in deps.values_mut() {
            dependencies.remove(next);
        }
    }
    if deps.is_empty() {
        return Ok(order);
    }
    let mut errors = Vec::new();
    for name in deps.keys() {
        let file = parts
            .iter()
            .find(|part| part.contribution.app == *name)
            .map_or_else(String::new, |part| first_file(part.contribution));
        errors.push(
            UiExtError::new(
                codes::DEPENDENCY_CYCLE,
                file,
                "",
                format!("{name} is part of a dependency cycle; no dependency order exists"),
            )
            .with_app(name),
        );
    }
    Err(errors)
}

/// Every component of every app, by id.
fn collect_components<'a>(parts: &'a [Part<'a>]) -> BTreeMap<String, ComponentMeta<'a>> {
    let mut components: BTreeMap<String, ComponentMeta<'a>> = BTreeMap::new();
    for part in parts {
        for parsed in &part.docs {
            for (position, component) in parsed.doc.components.iter().enumerate() {
                components.insert(
                    component.id.clone(),
                    ComponentMeta {
                        app: &part.contribution.app,
                        file: &parsed.file,
                        ptr: format!("/components/{position}"),
                        component,
                    },
                );
            }
        }
    }
    components
}

/// Reports cross-app component references that name a component nobody contributes, and the
fn check_cross_app_references(
    parts: &[Part<'_>],
    components: &BTreeMap<String, ComponentMeta<'_>>,
    errors: &mut Vec<UiExtError>,
) {
    let mut ids: BTreeMap<String, &str> = BTreeMap::new();
    for part in parts {
        for parsed in &part.docs {
            let app = part.contribution.app.as_str();
            let item_ids = parsed
                .doc
                .actions
                .iter()
                .map(|action| action.id.as_str())
                .chain(parsed.doc.panels.iter().map(|panel| panel.id.as_str()))
                .chain(parsed.doc.tiles.iter().map(|tile| tile.id.as_str()))
                .chain(parsed.doc.tabs.iter().map(|tab| tab.id.as_str()))
                .chain(
                    parsed
                        .doc
                        .components
                        .iter()
                        .map(|component| component.id.as_str()),
                );
            for id in item_ids {
                if let Some(first) = ids.insert(id.to_owned(), app)
                    && first != app
                {
                    errors.push(
                        UiExtError::new(
                            codes::DUPLICATE_ID,
                            &parsed.file,
                            "",
                            format!("{id} is used twice, by apps {first} and {app}"),
                        )
                        .with_app(app)
                        .with_id(id),
                    );
                }
            }
        }
    }
    for part in parts {
        for parsed in &part.docs {
            let app = part.contribution.app.as_str();
            for (position, tile) in parsed.doc.tiles.iter().enumerate() {
                check_reference(
                    app,
                    &parsed.file,
                    &format!("/tiles/{position}/component"),
                    &tile.component,
                    components,
                    errors,
                );
            }
            for (position, component) in parsed.doc.components.iter().enumerate() {
                let mut used = BTreeSet::new();
                composite::dependencies(&component.body, &mut used);
                for id in used {
                    check_reference(
                        app,
                        &parsed.file,
                        &format!("/components/{position}/body"),
                        &id,
                        components,
                        errors,
                    );
                }
            }
        }
    }
}

fn check_reference(
    app: &str,
    file: &str,
    ptr: &str,
    id: &str,
    components: &BTreeMap<String, ComponentMeta<'_>>,
    errors: &mut Vec<UiExtError>,
) {
    let Some(target) = limits::id_app(id) else {
        return;
    };
    if target == app {
        return;
    }
    if !components.contains_key(id) {
        errors.push(
            UiExtError::new(
                codes::UNKNOWN_COMPONENT,
                file,
                ptr.to_owned(),
                format!("component {id} is not contributed by any installed app"),
            )
            .with_app(app)
            .with_id(id),
        );
    }
}

/// Cycles, depth and size over the merged component graph: a reference across apps can still
fn check_cross_app_composites(
    components: &BTreeMap<String, ComponentMeta<'_>>,
    errors: &mut Vec<UiExtError>,
) {
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, meta) in components {
        let mut used = BTreeSet::new();
        composite::dependencies(&meta.component.body, &mut used);
        graph.insert(id.clone(), used);
    }
    let cyclic = composite::cycles(&graph);
    for id in &cyclic {
        if let Some(meta) = components.get(id) {
            errors.push(
                UiExtError::new(
                    codes::COMPONENT_CYCLE,
                    meta.file,
                    meta.ptr.clone(),
                    format!("component cycle through {id}"),
                )
                .with_app(meta.app)
                .with_id(id),
            );
        }
    }
    if !cyclic.is_empty() {
        return;
    }
    let bodies: BTreeMap<String, &crate::model::Node> = components
        .iter()
        .map(|(id, meta)| (id.clone(), &meta.component.body))
        .collect();
    for (id, meta) in components {
        let Some((depth, nodes)) = composite::measure(&meta.component.body, &bodies) else {
            continue;
        };
        if depth > limits::MAX_COMPONENT_DEPTH as u64 {
            errors.push(
                UiExtError::new(
                    codes::COMPONENT_TOO_DEEP,
                    meta.file,
                    meta.ptr.clone(),
                    format!(
                        "{id} nests {depth} deep; the limit is {}",
                        limits::MAX_COMPONENT_DEPTH
                    ),
                )
                .with_app(meta.app)
                .with_id(id),
            );
        }
        if nodes > limits::MAX_COMPONENT_NODES as u64 {
            errors.push(
                UiExtError::new(
                    codes::COMPONENT_TOO_LARGE,
                    meta.file,
                    meta.ptr.clone(),
                    format!(
                        "{id} expands to {nodes} nodes; the limit is {}",
                        limits::MAX_COMPONENT_NODES
                    ),
                )
                .with_app(meta.app)
                .with_id(id),
            );
        }
    }
}

/// Canonical JSON: `serde_json`'s map is ordered, and no whitespace is written.
fn canonical(value: &Value) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(_) => String::from("{}"),
    }
}

fn hash_value(value: &Value) -> String {
    let digest = Sha256::digest(canonical(value).as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn json<T: Serialize>(value: &T) -> Value {
    match serde_json::to_value(value) {
        Ok(value) => value,
        Err(_) => Value::Null,
    }
}
