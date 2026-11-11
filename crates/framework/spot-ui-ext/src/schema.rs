//! The schema half of validation: closed objects, patterns and per-collection limits.

use std::collections::BTreeMap;

use crate::code::{UiExtError, codes};
use crate::limits::Version;
use crate::limits::{self, CONTRACT_VERSION};
use crate::model::{
    Action, Block, Call, Component, DestinationRef, LiteralValue, Node, Panel, PropValue, Tab,
    Tile, UiDocument,
};

/// Collects schema findings for one file.
pub(crate) struct Findings<'a> {
    pub(crate) app: &'a str,
    pub(crate) file: &'a str,
    pub(crate) out: Vec<UiExtError>,
}

impl Findings<'_> {
    /// Adds a `uiext.schema` finding at `pointer`.
    pub(crate) fn schema(&mut self, pointer: impl Into<String>, message: impl Into<String>) {
        self.out
            .push(UiExtError::new(codes::SCHEMA, self.file, pointer, message).with_app(self.app));
    }
}

/// Checks one document against the schema half of the contract.
pub(crate) fn check_document(file: &str, doc: &UiDocument) -> Vec<UiExtError> {
    let mut findings = Findings {
        app: &doc.app,
        file,
        out: Vec::new(),
    };
    check_document_into(&mut findings, doc);
    findings.out
}

fn check_document_into(findings: &mut Findings<'_>, doc: &UiDocument) {
    if doc.contract_version != CONTRACT_VERSION {
        findings.schema(
            "/contract_version",
            format!(
                "contract version {} is not {CONTRACT_VERSION}",
                doc.contract_version
            ),
        );
    }
    if Version::parse(&doc.requires_shell).is_none() {
        findings.schema(
            "/requires_shell",
            format!(
                "requires_shell {} is not a MAJOR.MINOR.PATCH version",
                doc.requires_shell
            ),
        );
    }
    check_name(findings, "/app", "app", &doc.app);
    if doc.actions.len() > 100 {
        findings.schema("/actions", "at most 100 actions are allowed");
    }
    if doc.panels.len() > 50 {
        findings.schema("/panels", "at most 50 panels are allowed");
    }
    if doc.components.len() > 50 {
        findings.schema("/components", "at most 50 components are allowed");
    }
    if doc.tiles.len() > 50 {
        findings.schema("/tiles", "at most 50 tiles are allowed");
    }
    if doc.tabs.len() > 50 {
        findings.schema("/tabs", "at most 50 tabs are allowed");
    }
    for (index, action) in doc.actions.iter().enumerate() {
        check_action(findings, &format!("/actions/{index}"), action);
    }
    for (index, panel) in doc.panels.iter().enumerate() {
        check_panel(findings, &format!("/panels/{index}"), panel);
    }
    for (index, component) in doc.components.iter().enumerate() {
        check_component(findings, &format!("/components/{index}"), component);
    }
    for (index, tile) in doc.tiles.iter().enumerate() {
        check_tile(findings, &format!("/tiles/{index}"), tile);
    }
    for (index, tab) in doc.tabs.iter().enumerate() {
        check_tab(findings, &format!("/tabs/{index}"), tab);
    }
}

fn check_action(findings: &mut Findings<'_>, at: &str, action: &Action) {
    check_item_identity(findings, at, &action.id, &action.origin, &action.label);
    check_optional_name(findings, at, "doctype", action.doctype.as_deref());
    check_optional_order(findings, at, action.order);
    check_optional_expression(findings, at, "visible_if", action.visible_if.as_deref());
    check_optional_expression(findings, at, "enabled_if", action.enabled_if.as_deref());
    check_optional_permission(findings, at, action.permission.as_deref());
    if let Some(confirm) = &action.confirm
        && (confirm.is_empty() || confirm.chars().count() > 200)
    {
        findings.schema(
            format!("{at}/confirm"),
            "confirm must be 1 to 200 characters",
        );
    }
    if let Some(call) = &action.call {
        check_call(findings, &format!("{at}/call"), call);
    }
    if let Some(open) = &action.open {
        check_destination(findings, &format!("{at}/open"), open);
    }
    if let Some(url) = &action.navigate_url
        && (url.is_empty() || url.chars().count() > 200)
    {
        findings.schema(
            format!("{at}/navigate_url"),
            "navigate_url must be 1 to 200 characters",
        );
    }
}

fn check_call(findings: &mut Findings<'_>, at: &str, call: &Call) {
    check_name(findings, &format!("{at}/doctype"), "doctype", &call.doctype);
    check_name(findings, &format!("{at}/method"), "method", &call.method);
    if call.args.len() > 8 {
        findings.schema(format!("{at}/args"), "a call takes at most 8 arguments");
    }
    for (name, value) in &call.args {
        check_name(
            findings,
            &format!("{at}/args/{name}"),
            "argument name",
            name,
        );
        check_expression_value(findings, &format!("{at}/args/{name}"), value);
    }
    if let Some(crate::model::Then::Open { open }) = &call.then {
        check_destination(findings, &format!("{at}/then/open"), open);
    }
}

fn check_destination(findings: &mut Findings<'_>, at: &str, reference: &DestinationRef) {
    if !limits::is_id(&reference.destination) {
        findings.schema(
            format!("{at}/destination"),
            format!(
                "destination {} is not a namespaced id",
                reference.destination
            ),
        );
    }
    if reference.filters.len() > 8 {
        findings.schema(format!("{at}/filters"), "at most 8 filters are allowed");
    }
    for (name, value) in &reference.filters {
        check_name(
            findings,
            &format!("{at}/filters/{name}"),
            "filter name",
            name,
        );
        check_expression_value(findings, &format!("{at}/filters/{name}"), value);
    }
}

fn check_panel(findings: &mut Findings<'_>, at: &str, panel: &Panel) {
    check_item_identity(findings, at, &panel.id, &panel.origin, &panel.label);
    check_optional_name(findings, at, "doctype", panel.doctype.as_deref());
    check_optional_order(findings, at, panel.order);
    check_optional_expression(findings, at, "visible_if", panel.visible_if.as_deref());
    check_optional_permission(findings, at, panel.permission.as_deref());
    if panel.blocks.is_empty() || panel.blocks.len() > 12 {
        findings.schema(format!("{at}/blocks"), "a panel has 1 to 12 blocks");
    }
    for (index, block) in panel.blocks.iter().enumerate() {
        check_block(findings, &format!("{at}/blocks/{index}"), block);
    }
}

fn check_block(findings: &mut Findings<'_>, at: &str, block: &Block) {
    match block {
        Block::FieldList { of, fields } => {
            check_optional_name(findings, at, "of", of.as_deref());
            if fields.is_empty() || fields.len() > 20 {
                findings.schema(format!("{at}/fields"), "a field list has 1 to 20 fields");
            }
            for (index, field) in fields.iter().enumerate() {
                check_name(findings, &format!("{at}/fields/{index}"), "field", field);
            }
        }
        Block::KeyValue { items } => {
            if items.is_empty() || items.len() > 20 {
                findings.schema(format!("{at}/items"), "a key/value block has 1 to 20 items");
            }
            for (index, item) in items.iter().enumerate() {
                let item_at = format!("{at}/items/{index}");
                check_label(findings, &format!("{item_at}/label"), &item.label);
                check_expression_value(findings, &format!("{item_at}/value"), &item.value);
            }
        }
        Block::Text { markdown } => {
            if markdown.is_empty() || markdown.chars().count() > 2000 {
                findings.schema(
                    format!("{at}/markdown"),
                    "markdown must be 1 to 2000 characters",
                );
            }
        }
        Block::Table {
            columns, max_rows, ..
        } => {
            if columns.is_empty() || columns.len() > 8 {
                findings.schema(format!("{at}/columns"), "a table has 1 to 8 columns");
            }
            for (index, column) in columns.iter().enumerate() {
                let column_at = format!("{at}/columns/{index}");
                check_name(
                    findings,
                    &format!("{column_at}/field"),
                    "field",
                    &column.field,
                );
                if let Some(label) = &column.label {
                    check_label(findings, &format!("{column_at}/label"), label);
                }
            }
            if !(1..=50).contains(max_rows) {
                findings.schema(format!("{at}/max_rows"), "max_rows must be 1 to 50");
            }
        }
        Block::Stat { label, .. } => {
            check_label(findings, &format!("{at}/label"), label);
        }
        Block::ListOfLinks { items } => {
            if items.is_empty() || items.len() > 12 {
                findings.schema(format!("{at}/items"), "at most 12 links are allowed");
            }
            for (index, item) in items.iter().enumerate() {
                let item_at = format!("{at}/items/{index}");
                check_label(findings, &format!("{item_at}/label"), &item.label);
                check_destination(findings, &format!("{item_at}/open"), &item.open);
            }
        }
        Block::EmptyState { text, .. } => {
            check_label(findings, &format!("{at}/text"), text);
        }
    }
}

fn check_component(findings: &mut Findings<'_>, at: &str, component: &Component) {
    if !limits::is_id(&component.id) {
        findings.schema(
            format!("{at}/id"),
            format!("{} is not a namespaced id", component.id),
        );
    }
    if !limits::is_origin(&component.origin) {
        findings.schema(
            format!("{at}/origin"),
            format!("origin {} is not app:<name>", component.origin),
        );
    }
    if component.props.len() > 16 {
        findings.schema(format!("{at}/props"), "at most 16 props are allowed");
    }
    let mut names = BTreeMap::new();
    for (index, prop) in component.props.iter().enumerate() {
        let prop_at = format!("{at}/props/{index}");
        check_name(
            findings,
            &format!("{prop_at}/name"),
            "prop name",
            &prop.name,
        );
        if names.insert(prop.name.as_str(), index).is_some() {
            findings.schema(
                format!("{prop_at}/name"),
                format!("prop {} is declared twice", prop.name),
            );
        }
    }
    if component.slots.len() > 8 {
        findings.schema(format!("{at}/slots"), "at most 8 slots are allowed");
    }
    let mut slot_names = BTreeMap::new();
    for (index, slot) in component.slots.iter().enumerate() {
        check_name(findings, &format!("{at}/slots/{index}"), "slot name", slot);
        if slot_names.insert(slot.as_str(), index).is_some() {
            findings.schema(
                format!("{at}/slots/{index}"),
                format!("slot {slot} is declared twice"),
            );
        }
    }
    check_node(findings, &format!("{at}/body"), &component.body);
}

fn check_node(findings: &mut Findings<'_>, at: &str, node: &Node) {
    match node {
        Node::Row {
            children,
            columns,
            title,
        }
        | Node::Column {
            children,
            columns,
            title,
        }
        | Node::Grid {
            children,
            columns,
            title,
        }
        | Node::Group {
            children,
            columns,
            title,
        } => {
            if children.len() > 100 {
                findings.schema(format!("{at}/children"), "at most 100 children are allowed");
            }
            if let Some(columns) = columns
                && !(1..=6).contains(columns)
            {
                findings.schema(format!("{at}/columns"), "columns must be 1 to 6");
            }
            if let Some(title) = title {
                check_label(findings, &format!("{at}/title"), title);
            }
            for (index, child) in children.iter().enumerate() {
                check_node(findings, &format!("{at}/children/{index}"), child);
            }
        }
        Node::Repeat { over, max, body } => {
            check_name(findings, &format!("{at}/over"), "prop name", over);
            if !(1..=limits::MAX_REPEAT).contains(max) {
                findings.schema(
                    format!("{at}/max"),
                    format!("repeat.max must be 1 to {}", limits::MAX_REPEAT),
                );
            }
            check_node(findings, &format!("{at}/body"), body);
        }
        Node::When { test, then } => {
            check_expression_value(findings, &format!("{at}/test"), test);
            check_node(findings, &format!("{at}/then"), then);
        }
        Node::Control {
            text,
            value,
            on_click,
            ..
        } => {
            if let Some(text) = text {
                check_label(findings, &format!("{at}/text"), text);
            }
            if let Some(value) = value {
                check_expression_value(findings, &format!("{at}/value"), value);
            }
            if let Some(action) = on_click
                && !limits::is_id(action)
            {
                findings.schema(
                    format!("{at}/on_click"),
                    format!("{action} is not a namespaced action id"),
                );
            }
        }
        Node::Use {
            component,
            props,
            fill,
        } => {
            if !limits::is_id(component) {
                findings.schema(
                    format!("{at}/component"),
                    format!("{component} is not a namespaced component id"),
                );
            }
            if props.len() > 16 {
                findings.schema(format!("{at}/props"), "at most 16 props are allowed");
            }
            for (name, value) in props {
                check_name(findings, &format!("{at}/props/{name}"), "prop name", name);
                check_expression_value(findings, &format!("{at}/props/{name}"), value);
            }
            if fill.len() > 8 {
                findings.schema(format!("{at}/fill"), "at most 8 slots can be filled");
            }
            for (name, body) in fill {
                check_name(findings, &format!("{at}/fill/{name}"), "slot name", name);
                check_node(findings, &format!("{at}/fill/{name}"), body);
            }
        }
        Node::Slot { name } => check_name(findings, &format!("{at}/name"), "slot name", name),
    }
}

fn check_tile(findings: &mut Findings<'_>, at: &str, tile: &Tile) {
    check_item_identity(findings, at, &tile.id, &tile.origin, &tile.label);
    check_optional_name(findings, at, "doctype", tile.doctype.as_deref());
    check_optional_order(findings, at, tile.order);
    check_optional_expression(findings, at, "visible_if", tile.visible_if.as_deref());
    check_optional_permission(findings, at, tile.permission.as_deref());
    if !limits::is_id(&tile.component) {
        findings.schema(
            format!("{at}/component"),
            format!("{} is not a namespaced component id", tile.component),
        );
    }
    if tile.props.len() > 16 {
        findings.schema(format!("{at}/props"), "at most 16 props are allowed");
    }
    for (name, value) in &tile.props {
        check_name(findings, &format!("{at}/props/{name}"), "prop name", name);
        match value {
            PropValue::Query { query } => {
                if !limits::is_id(query) {
                    findings.schema(
                        format!("{at}/props/{name}/query"),
                        format!("{query} is not a namespaced query id"),
                    );
                }
            }
            PropValue::Literal(LiteralValue::Text(text)) => {
                if text.chars().count() > 200 {
                    findings.schema(
                        format!("{at}/props/{name}"),
                        "a literal value is at most 200 characters",
                    );
                }
            }
            PropValue::Literal(_) => {}
        }
    }
    if let Some(open) = &tile.open {
        check_destination(findings, &format!("{at}/open"), open);
    }
}

fn check_tab(findings: &mut Findings<'_>, at: &str, tab: &Tab) {
    check_item_identity(findings, at, &tab.id, &tab.origin, &tab.label);
    check_name(findings, &format!("{at}/doctype"), "doctype", &tab.doctype);
    check_optional_order(findings, at, tab.order);
    check_optional_expression(findings, at, "visible_if", tab.visible_if.as_deref());
    check_optional_permission(findings, at, tab.permission.as_deref());
    if !limits::is_id(&tab.panel) {
        findings.schema(
            format!("{at}/panel"),
            format!("{} is not a namespaced panel id", tab.panel),
        );
    }
}

fn check_item_identity(findings: &mut Findings<'_>, at: &str, id: &str, origin: &str, label: &str) {
    if !limits::is_id(id) {
        findings.schema(format!("{at}/id"), format!("{id} is not a namespaced id"));
    }
    if !limits::is_origin(origin) {
        findings.schema(
            format!("{at}/origin"),
            format!("origin {origin} is not app:<name>"),
        );
    }
    check_label(findings, &format!("{at}/label"), label);
}

fn check_label(findings: &mut Findings<'_>, at: &str, label: &str) {
    if label.is_empty() || label.chars().count() > 64 {
        findings.schema(at, "a label must be 1 to 64 characters");
    }
}

fn check_name(findings: &mut Findings<'_>, at: &str, what: &str, value: &str) {
    if !limits::is_name(value) {
        findings.schema(at, format!("{what} {value} is not a lower-case name"));
    }
}

fn check_optional_name(findings: &mut Findings<'_>, at: &str, field: &str, value: Option<&str>) {
    if let Some(value) = value {
        check_name(findings, &format!("{at}/{field}"), field, value);
    }
}

fn check_optional_order(findings: &mut Findings<'_>, at: &str, order: Option<i64>) {
    if let Some(order) = order
        && !(-1000..=1000).contains(&order)
    {
        findings.schema(
            format!("{at}/order"),
            "order must be between -1000 and 1000",
        );
    }
}

fn check_optional_permission(findings: &mut Findings<'_>, at: &str, value: Option<&str>) {
    if let Some(permission) = value
        && !limits::is_permission_ref(permission)
    {
        findings.schema(
            format!("{at}/permission"),
            format!("{permission} is not a <doctype>:<right> reference"),
        );
    }
}

fn check_optional_expression(
    findings: &mut Findings<'_>,
    at: &str,
    field: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        check_expression_value(findings, &format!("{at}/{field}"), value);
    }
}

fn check_expression_value(findings: &mut Findings<'_>, at: &str, value: &str) {
    if value.is_empty() || value.chars().count() > 500 {
        findings.schema(at, "an expression must be 1 to 500 characters");
    }
}
