//! Semantic validation: referential integrity, expressions, panels and components.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use crate::code::{UiExtError, codes, sort_findings};
use crate::components;
use crate::composite;
use crate::expr::{self, Scope};
use crate::facts::{AppContribution, AppFacts};
use crate::limits::{self, ItemKind};
use crate::model::{Action, Component, DestinationRef, Node, Panel, UiDocument};
use crate::panels;
use crate::parse;
use crate::schema;

/// One file of a contribution, parsed.
pub(crate) struct ParsedDoc {
    pub(crate) file: String,
    pub(crate) doc: UiDocument,
}

/// Parses and validates a contribution: the parsed documents when valid, the findings otherwise.
pub(crate) fn analyse(contribution: &AppContribution) -> (Vec<ParsedDoc>, Vec<UiExtError>) {
    let mut docs = Vec::new();
    let mut findings = Vec::new();
    for (file, text) in &contribution.documents {
        match parse::parse_document(file, text) {
            Ok(doc) => docs.push(ParsedDoc {
                file: file.clone(),
                doc,
            }),
            Err(error) => findings.push(error.with_app(&contribution.app)),
        }
    }
    if !findings.is_empty() {
        sort_findings(&mut findings);
        return (Vec::new(), findings);
    }
    for parsed in &docs {
        findings.extend(schema::check_document(&parsed.file, &parsed.doc));
    }
    if !findings.is_empty() {
        sort_findings(&mut findings);
        return (Vec::new(), findings);
    }
    let mut checker = Checker::new(contribution, &docs);
    checker.run();
    let mut findings = checker.findings;
    sort_findings(&mut findings);
    (docs, findings)
}

/// Validates one app's contribution, reporting every problem at once.
#[must_use]
pub fn validate(contribution: &AppContribution) -> Vec<UiExtError> {
    analyse(contribution).1
}

/// A parsed item with where it came from.
pub(crate) struct Located<'a, T> {
    pub(crate) file: &'a str,
    pub(crate) ptr: String,
    pub(crate) item: &'a T,
}

/// The app-wide index of what the app's documents contribute.
pub(crate) struct Index<'a> {
    actions: BTreeMap<String, Located<'a, Action>>,
    panels: BTreeMap<String, Located<'a, Panel>>,
    pub(crate) components: BTreeMap<String, Located<'a, Component>>,
    seen: BTreeMap<String, (String, String)>,
}

/// One item's shared fields.
struct Common<'a> {
    kind: ItemKind,
    file: &'a str,
    ptr: String,
    id: &'a str,
    origin: &'a str,
    slot: &'a str,
    doctype: Option<&'a str>,
    label: &'a str,
    icon: Option<&'a str>,
    permission: Option<&'a str>,
    visible_if: Option<&'a str>,
}

/// Everything the checks read besides the item itself.
pub(crate) struct Cx<'a> {
    pub(crate) app: &'a str,
    pub(crate) facts: &'a AppFacts,
    pub(crate) depends: &'a [String],
    pub(crate) index: &'a Index<'a>,
}

impl Index<'_> {
    pub(crate) fn action(&self, id: &str) -> Option<&Action> {
        self.actions.get(id).map(|located| located.item)
    }

    pub(crate) fn panel(&self, id: &str) -> Option<&Panel> {
        self.panels.get(id).map(|located| located.item)
    }

    pub(crate) fn component(&self, id: &str) -> Option<&Component> {
        self.components.get(id).map(|located| located.item)
    }
}

struct Checker<'a> {
    contribution: &'a AppContribution,
    docs: &'a [ParsedDoc],
    index: Index<'a>,
    findings: Vec<UiExtError>,
    counts: BTreeMap<(String, String), usize>,
}

impl<'a> Checker<'a> {
    fn new(contribution: &'a AppContribution, docs: &'a [ParsedDoc]) -> Self {
        let mut index = Index {
            actions: BTreeMap::new(),
            panels: BTreeMap::new(),
            components: BTreeMap::new(),
            seen: BTreeMap::new(),
        };
        let mut findings = Vec::new();
        let app = contribution.app.as_str();
        for parsed in docs {
            let file = parsed.file.as_str();
            let doc = &parsed.doc;
            let located = |ptr: String, item: &'a Action| Located { file, ptr, item };
            for (position, action) in doc.actions.iter().enumerate() {
                index.actions.insert(
                    action.id.clone(),
                    located(format!("/actions/{position}"), action),
                );
            }
            for (position, panel) in doc.panels.iter().enumerate() {
                index.panels.insert(
                    panel.id.clone(),
                    Located {
                        file,
                        ptr: format!("/panels/{position}"),
                        item: panel,
                    },
                );
            }
            for (position, component) in doc.components.iter().enumerate() {
                index.components.insert(
                    component.id.clone(),
                    Located {
                        file,
                        ptr: format!("/components/{position}"),
                        item: component,
                    },
                );
            }
            for (id, file, ptr) in identities(parsed) {
                match index.seen.entry(id.clone()) {
                    Entry::Occupied(_) => findings.push(
                        UiExtError::new(
                            codes::DUPLICATE_ID,
                            file,
                            ptr,
                            format!("{id} is used twice"),
                        )
                        .with_app(app)
                        .with_id(&id),
                    ),
                    Entry::Vacant(slot) => {
                        slot.insert((file.to_owned(), ptr));
                    }
                }
            }
        }
        Self {
            contribution,
            docs,
            index,
            findings,
            counts: BTreeMap::new(),
        }
    }

    fn run(&mut self) {
        let cx = Cx {
            app: &self.contribution.app,
            facts: &self.contribution.facts,
            depends: &self.contribution.depends,
            index: &self.index,
        };
        for parsed in self.docs {
            let file = parsed.file.as_str();
            for item in common_items(file, &parsed.doc) {
                check_common(&cx, &mut self.findings, &mut self.counts, item);
            }
            for (position, action) in parsed.doc.actions.iter().enumerate() {
                check_action(
                    &cx,
                    &mut self.findings,
                    file,
                    &format!("/actions/{position}"),
                    action,
                );
            }
            for (position, panel) in parsed.doc.panels.iter().enumerate() {
                panels::check_panel(
                    &cx,
                    &mut self.findings,
                    file,
                    &format!("/panels/{position}"),
                    panel,
                );
            }
            for (position, tile) in parsed.doc.tiles.iter().enumerate() {
                components::check_tile(
                    &cx,
                    &mut self.findings,
                    file,
                    &format!("/tiles/{position}"),
                    tile,
                );
            }
            for (position, tab) in parsed.doc.tabs.iter().enumerate() {
                components::check_tab(
                    &cx,
                    &mut self.findings,
                    file,
                    &format!("/tabs/{position}"),
                    tab,
                );
            }
            for (position, component) in parsed.doc.components.iter().enumerate() {
                let ptr = format!("/components/{position}");
                if component.origin != format!("app:{}", cx.app)
                    || !component.id.starts_with(&format!("{}.", cx.app))
                {
                    report(
                        &cx,
                        &mut self.findings,
                        codes::ORIGIN_MISMATCH,
                        file,
                        ptr.clone(),
                        format!("{} is not owned by app:{}", component.id, cx.app),
                        Some(&component.id),
                    );
                }
                components::check_component(&cx, &mut self.findings, file, &ptr, component);
            }
        }
        self.check_composites();
    }

    /// Cycles, depth and expansion size, once every component of the app is known.
    fn check_composites(&mut self) {
        let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (id, located) in &self.index.components {
            let mut used = BTreeSet::new();
            composite::dependencies(&located.item.body, &mut used);
            graph.insert(id.clone(), used);
        }
        let app = self.contribution.app.as_str();
        for id in composite::cycles(&graph) {
            if let Some(located) = self.index.components.get(&id) {
                self.findings.push(
                    UiExtError::new(
                        codes::COMPONENT_CYCLE,
                        located.file,
                        located.ptr.clone(),
                        format!("component cycle through {id}"),
                    )
                    .with_app(app)
                    .with_id(&id),
                );
            }
        }
        let bodies: BTreeMap<String, &Node> = self
            .index
            .components
            .iter()
            .map(|(id, located)| (id.clone(), &located.item.body))
            .collect();
        for (id, located) in &self.index.components {
            let Some((depth, nodes)) = composite::measure(&located.item.body, &bodies) else {
                continue;
            };
            let deep = depth > limits::MAX_COMPONENT_DEPTH as u64;
            let large = nodes > limits::MAX_COMPONENT_NODES as u64;
            if deep {
                self.findings.push(
                    UiExtError::new(
                        codes::COMPONENT_TOO_DEEP,
                        located.file,
                        located.ptr.clone(),
                        format!(
                            "{id} nests {depth} deep; the limit is {}",
                            limits::MAX_COMPONENT_DEPTH
                        ),
                    )
                    .with_app(app)
                    .with_id(id),
                );
            }
            if large {
                self.findings.push(
                    UiExtError::new(
                        codes::COMPONENT_TOO_LARGE,
                        located.file,
                        located.ptr.clone(),
                        format!(
                            "{id} expands to {nodes} nodes; the limit is {}",
                            limits::MAX_COMPONENT_NODES
                        ),
                    )
                    .with_app(app)
                    .with_id(id),
                );
            }
        }
    }
}

/// Every id in a document, with the file and pointer it sits at.
fn identities(parsed: &ParsedDoc) -> Vec<(String, &str, String)> {
    let file = parsed.file.as_str();
    let doc = &parsed.doc;
    let mut ids = Vec::new();
    for (position, action) in doc.actions.iter().enumerate() {
        ids.push((action.id.clone(), file, format!("/actions/{position}")));
    }
    for (position, panel) in doc.panels.iter().enumerate() {
        ids.push((panel.id.clone(), file, format!("/panels/{position}")));
    }
    for (position, component) in doc.components.iter().enumerate() {
        ids.push((
            component.id.clone(),
            file,
            format!("/components/{position}"),
        ));
    }
    for (position, tile) in doc.tiles.iter().enumerate() {
        ids.push((tile.id.clone(), file, format!("/tiles/{position}")));
    }
    for (position, tab) in doc.tabs.iter().enumerate() {
        ids.push((tab.id.clone(), file, format!("/tabs/{position}")));
    }
    ids
}

/// The shared view of every item of one file, in document order.
fn common_items<'a>(file: &'a str, doc: &'a UiDocument) -> Vec<Common<'a>> {
    let mut items = Vec::new();
    let mut add = |kind,
                   ptr: String,
                   id: &'a str,
                   origin: &'a str,
                   slot: &'a str,
                   doctype: Option<&'a str>,
                   label: &'a str,
                   icon: Option<&'a str>,
                   permission: Option<&'a str>,
                   visible_if: Option<&'a str>| {
        items.push(Common {
            kind,
            file,
            ptr,
            id,
            origin,
            slot,
            doctype,
            label,
            icon,
            permission,
            visible_if,
        });
    };
    for (position, action) in doc.actions.iter().enumerate() {
        add(
            ItemKind::Action,
            format!("/actions/{position}"),
            &action.id,
            &action.origin,
            &action.slot,
            action.doctype.as_deref(),
            &action.label,
            action.icon.as_deref(),
            action.permission.as_deref(),
            action.visible_if.as_deref(),
        );
    }
    for (position, panel) in doc.panels.iter().enumerate() {
        add(
            ItemKind::Panel,
            format!("/panels/{position}"),
            &panel.id,
            &panel.origin,
            &panel.slot,
            panel.doctype.as_deref(),
            &panel.label,
            None,
            panel.permission.as_deref(),
            panel.visible_if.as_deref(),
        );
    }
    for (position, tile) in doc.tiles.iter().enumerate() {
        add(
            ItemKind::Tile,
            format!("/tiles/{position}"),
            &tile.id,
            &tile.origin,
            &tile.slot,
            tile.doctype.as_deref(),
            &tile.label,
            tile.icon.as_deref(),
            tile.permission.as_deref(),
            tile.visible_if.as_deref(),
        );
    }
    for (position, tab) in doc.tabs.iter().enumerate() {
        add(
            ItemKind::Tab,
            format!("/tabs/{position}"),
            &tab.id,
            &tab.origin,
            &tab.slot,
            Some(&tab.doctype),
            &tab.label,
            tab.icon.as_deref(),
            tab.permission.as_deref(),
            tab.visible_if.as_deref(),
        );
    }
    items
}

fn check_common(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    counts: &mut BTreeMap<(String, String), usize>,
    item: Common<'_>,
) {
    if let Some(spec) = limits::slot(item.slot) {
        if spec.accepts != item.kind {
            report(
                cx,
                findings,
                codes::SLOT_KIND_MISMATCH,
                item.file,
                format!("{}/slot", item.ptr),
                format!(
                    "{} accepts {}, not {}",
                    item.slot,
                    spec.accepts.as_str(),
                    item.kind.as_str()
                ),
                Some(item.id),
            );
        }
        if spec.is_document_slot() && item.doctype.is_none() {
            report(
                cx,
                findings,
                codes::DOCTYPE_REQUIRED,
                item.file,
                item.ptr.clone(),
                format!("{} sits in {}: name its doctype", item.id, item.slot),
                Some(item.id),
            );
        }
        if !item.label.starts_with("key:") && item.label.chars().count() > spec.label_max {
            report(
                cx,
                findings,
                codes::LABEL_TOO_LONG,
                item.file,
                format!("{}/label", item.ptr),
                format!(
                    "label of {} has {} characters; {} allows {}",
                    item.id,
                    item.label.chars().count(),
                    item.slot,
                    spec.label_max
                ),
                Some(item.id),
            );
        }
        let key = (
            item.slot.to_owned(),
            item.doctype.unwrap_or_default().to_owned(),
        );
        let count = counts.entry(key).or_insert(0);
        *count += 1;
        if *count == spec.max_per_app + 1 {
            report(
                cx,
                findings,
                codes::SLOT_LIMIT,
                item.file,
                item.ptr.clone(),
                format!(
                    "{} puts {} items in {}; the limit is {}",
                    cx.app, count, item.slot, spec.max_per_app
                ),
                Some(item.id),
            );
        }
    } else {
        report(
            cx,
            findings,
            codes::UNKNOWN_SLOT,
            item.file,
            format!("{}/slot", item.ptr),
            format!(
                "slot {} does not exist (contract {})",
                item.slot,
                limits::CONTRACT_VERSION
            ),
            Some(item.id),
        );
    }
    if item.origin != format!("app:{}", cx.app) || !item.id.starts_with(&format!("{}.", cx.app)) {
        report(
            cx,
            findings,
            codes::ORIGIN_MISMATCH,
            item.file,
            item.ptr.clone(),
            format!("{} is not owned by app:{}", item.id, cx.app),
            Some(item.id),
        );
    }
    if let Some(permission) = item.permission
        && !cx.facts.permissions.contains(permission)
    {
        report(
            cx,
            findings,
            codes::UNKNOWN_PERMISSION,
            item.file,
            format!("{}/permission", item.ptr),
            format!("permission {permission} does not exist"),
            Some(item.id),
        );
    }
    if let Some(icon) = item.icon
        && !cx.facts.icon_known(icon)
    {
        report(
            cx,
            findings,
            codes::UNKNOWN_ICON,
            item.file,
            format!("{}/icon", item.ptr),
            format!("icon {icon} is not an icon token"),
            Some(item.id),
        );
    }
    if let Some(source) = item.visible_if {
        let (fields, what) = fields_scope(cx.facts, item.doctype);
        check_expression(
            findings,
            cx.app,
            item.file,
            format!("{}/visible_if", item.ptr),
            source,
            &Scope::Fields(fields.as_deref()),
            &what,
        );
    }
}

fn check_action(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    action: &Action,
) {
    let effects = [
        action.call.is_some(),
        action.open.is_some(),
        action.navigate_url.is_some(),
    ]
    .iter()
    .filter(|present| **present)
    .count();
    if effects != 1 {
        report(
            cx,
            findings,
            codes::EFFECT_INVALID,
            file,
            ptr.to_owned(),
            format!("{} must have exactly one effect", action.id),
            Some(&action.id),
        );
    }
    let (fields, what) = fields_scope(cx.facts, action.doctype.as_deref());
    if let Some(source) = &action.enabled_if {
        check_expression(
            findings,
            cx.app,
            file,
            format!("{ptr}/enabled_if"),
            source,
            &Scope::Fields(fields.as_deref()),
            &what,
        );
    }
    if let Some(call) = &action.call {
        match cx.facts.method(&call.doctype, &call.method) {
            None => report(
                cx,
                findings,
                codes::UNKNOWN_METHOD,
                file,
                format!("{ptr}/call/method"),
                format!("{}.{} is not a declared method", call.doctype, call.method),
                Some(&action.id),
            ),
            Some(method) if !method.args.is_empty() => {
                for name in call.args.keys() {
                    if !method.args.contains(name) {
                        report(
                            cx,
                            findings,
                            codes::UNKNOWN_METHOD,
                            file,
                            format!("{ptr}/call/args/{name}"),
                            format!("{}.{} has no argument {name}", call.doctype, call.method),
                            Some(&action.id),
                        );
                    }
                }
            }
            Some(_) => {}
        }
        let (call_fields, call_what) = fields_scope(cx.facts, Some(&call.doctype));
        for (name, source) in &call.args {
            check_expression(
                findings,
                cx.app,
                file,
                format!("{ptr}/call/args/{name}"),
                source,
                &Scope::Fields(call_fields.as_deref()),
                &call_what,
            );
        }
        if let Some(crate::model::Then::Open { open }) = &call.then {
            check_destination(
                cx,
                findings,
                file,
                &format!("{ptr}/call/then/open"),
                open,
                &Scope::Fields(call_fields.as_deref()),
                &call_what,
            );
        }
    }
    if let Some(open) = &action.open {
        check_destination(
            cx,
            findings,
            file,
            &format!("{ptr}/open"),
            open,
            &Scope::Fields(fields.as_deref()),
            &what,
        );
    }
    if let Some(url) = &action.navigate_url {
        if leaves_app(url) {
            report(
                cx,
                findings,
                codes::EXTERNAL_URL,
                file,
                format!("{ptr}/navigate_url"),
                format!("navigate_url {url} leaves the app"),
                Some(&action.id),
            );
        } else if !cx.facts.route_covers(url) {
            report(
                cx,
                findings,
                codes::UNKNOWN_ROUTE,
                file,
                format!("{ptr}/navigate_url"),
                format!("no declared route covers {url}"),
                Some(&action.id),
            );
        }
    }
}

/// Whether a `navigate_url` has a scheme or host, or is not an absolute path.
fn leaves_app(url: &str) -> bool {
    let scheme = url.find(':').is_some_and(|colon| {
        colon > 0
            && url[..colon]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    scheme || url.starts_with("//") || !url.starts_with('/')
}

pub(crate) fn check_destination(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    reference: &DestinationRef,
    scope: &Scope<'_>,
    what: &str,
) {
    if !cx.facts.destinations.contains(&reference.destination) {
        report(
            cx,
            findings,
            codes::UNKNOWN_DESTINATION,
            file,
            format!("{ptr}/destination"),
            format!("destination {} does not exist", reference.destination),
            Some(&reference.destination),
        );
    }
    for (name, source) in &reference.filters {
        check_expression(
            findings,
            cx.app,
            file,
            format!("{ptr}/filters/{name}"),
            source,
            scope,
            what,
        );
    }
}

/// One finding, named by app, item and position.
pub(crate) fn report(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    code: &'static str,
    file: &str,
    pointer: String,
    message: String,
    id: Option<&str>,
) {
    let mut finding = UiExtError::new(code, file, pointer, message).with_app(cx.app);
    if let Some(id) = id {
        finding = finding.with_id(id);
    }
    findings.push(finding);
}

pub(crate) fn check_expression(
    findings: &mut Vec<UiExtError>,
    app: &str,
    file: &str,
    ptr: String,
    source: &str,
    scope: &Scope<'_>,
    what: &str,
) {
    if let Err(problem) = expr::check(source, scope, what) {
        findings.push(
            UiExtError::new(codes::EXPRESSION_INVALID, file, ptr, problem.message).with_app(app),
        );
    }
}

pub(crate) fn fields_scope(
    facts: &AppFacts,
    doctype: Option<&str>,
) -> (Option<Vec<String>>, String) {
    match doctype.and_then(|doctype| facts.doctype(doctype)) {
        Some(known) => (
            Some(known.fields.clone()),
            format!("the fields of `{}`", known.name),
        ),
        None => (None, "any field".to_owned()),
    }
}
