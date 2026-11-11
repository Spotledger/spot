//! The component half of validation: composite bodies, uses, tiles and tabs.

use std::collections::BTreeMap;

use crate::code::{UiExtError, codes};
use crate::expr::Scope;
use crate::limits;
use crate::model::{Component, LiteralValue, Node, Prop, PropType, PropValue, Tab, Tile};
use crate::validate::{Cx, check_destination, check_expression, fields_scope, report};

pub(crate) fn check_tile(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    tile: &Tile,
) {
    if let Some(component) = own_component(cx, findings, file, ptr, &tile.component, &tile.id) {
        check_props(cx, findings, file, ptr, component, &tile.props, &tile.id);
    }
    let (fields, what) = fields_scope(cx.facts, tile.doctype.as_deref());
    if let Some(open) = &tile.open {
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
}

pub(crate) fn check_tab(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    tab: &Tab,
) {
    let own = limits::id_app(&tab.panel).is_some_and(|app| app == cx.app);
    if !own || cx.index.panel(&tab.panel).is_none() {
        report(
            cx,
            findings,
            codes::UNKNOWN_PANEL,
            file,
            format!("{ptr}/panel"),
            format!("panel {} is not in this document", tab.panel),
            Some(&tab.id),
        );
    }
}

/// Resolves a component reference of the same app, or reports the dependency rule for another
/// app's component. Cross-app existence is checked when the apps are resolved together.
fn own_component<'a>(
    cx: &Cx<'a>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    component: &str,
    item_id: &str,
) -> Option<&'a Component> {
    match limits::id_app(component) {
        Some(app) if app == cx.app => match cx.index.component(component) {
            Some(found) => Some(found),
            None => {
                report(
                    cx,
                    findings,
                    codes::UNKNOWN_COMPONENT,
                    file,
                    format!("{ptr}/component"),
                    format!("component {component} is not in this document"),
                    Some(item_id),
                );
                None
            }
        },
        Some(app) if !cx.depends.iter().any(|dependency| dependency == app) => {
            report(
                cx,
                findings,
                codes::EXTENSION_WITHOUT_DEPENDENCY,
                file,
                format!("{ptr}/component"),
                format!(
                    "component {component} belongs to app {app}, which {} does not depend on",
                    cx.app
                ),
                Some(item_id),
            );
            None
        }
        _ => None,
    }
}

fn check_props(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    component: &Component,
    props: &BTreeMap<String, PropValue>,
    item_id: &str,
) {
    for prop in &component.props {
        let at = format!("{ptr}/props/{}", prop.name);
        match props.get(&prop.name) {
            None if prop.required == Some(true) => report(
                cx,
                findings,
                codes::SCHEMA,
                file,
                format!("{ptr}/props"),
                format!("component {} requires prop {}", component.id, prop.name),
                Some(item_id),
            ),
            None => {}
            Some(PropValue::Query { query }) => {
                if prop.ty == PropType::List {
                    report(
                        cx,
                        findings,
                        codes::SCHEMA,
                        file,
                        at.clone(),
                        format!("prop {} is a list; a query supplies one value", prop.name),
                        Some(item_id),
                    );
                }
                match cx.facts.query(query) {
                    Some(info) if info.kind == crate::facts::QueryKind::Aggregate => {}
                    _ => report(
                        cx,
                        findings,
                        codes::UNKNOWN_QUERY,
                        file,
                        at,
                        format!("query {query} is not a declared aggregate query"),
                        Some(item_id),
                    ),
                }
            }
            Some(PropValue::Literal(value)) if !literal_matches(prop.ty, value) => report(
                cx,
                findings,
                codes::SCHEMA,
                file,
                at,
                format!(
                    "prop {} is {}; the value is not",
                    prop.name,
                    prop_type_name(prop.ty)
                ),
                Some(item_id),
            ),
            Some(PropValue::Literal(_)) => {}
        }
    }
    for name in props.keys() {
        if !component.props.iter().any(|prop| &prop.name == name) {
            report(
                cx,
                findings,
                codes::SCHEMA,
                file,
                format!("{ptr}/props/{name}"),
                format!("component {} has no prop {name}", component.id),
                Some(item_id),
            );
        }
    }
}

fn literal_matches(ty: PropType, value: &LiteralValue) -> bool {
    match (ty, value) {
        (PropType::List, _) => false,
        (_, LiteralValue::Null) => true,
        (PropType::Text | PropType::Date | PropType::DateTime, LiteralValue::Text(_)) => true,
        (PropType::Int | PropType::Decimal, LiteralValue::Int(_)) => true,
        (PropType::Bool, LiteralValue::Bool(_)) => true,
        _ => false,
    }
}

fn prop_type_name(ty: PropType) -> &'static str {
    match ty {
        PropType::Text => "text",
        PropType::Int => "int",
        PropType::Decimal => "decimal",
        PropType::Bool => "bool",
        PropType::Date => "date",
        PropType::DateTime => "datetime",
        PropType::List => "list",
    }
}

pub(crate) fn check_component(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    component: &Component,
) {
    let what = format!("the props of `{}`", component.id);
    let names: Vec<String> = component
        .props
        .iter()
        .map(|prop| prop.name.clone())
        .collect();
    check_node(
        cx,
        findings,
        file,
        &format!("{ptr}/body"),
        &component.body,
        &component.props,
        &names,
        &component.slots,
        &what,
        &component.id,
    );
}

#[allow(clippy::too_many_arguments)]
fn check_node(
    cx: &Cx<'_>,
    findings: &mut Vec<UiExtError>,
    file: &str,
    ptr: &str,
    node: &Node,
    props: &[Prop],
    prop_names: &[String],
    slots: &[String],
    what: &str,
    component_id: &str,
) {
    match node {
        Node::Row { children, .. }
        | Node::Column { children, .. }
        | Node::Grid { children, .. }
        | Node::Group { children, .. } => {
            for (index, child) in children.iter().enumerate() {
                check_node(
                    cx,
                    findings,
                    file,
                    &format!("{ptr}/children/{index}"),
                    child,
                    props,
                    prop_names,
                    slots,
                    what,
                    component_id,
                );
            }
        }
        Node::Repeat { over, body, .. } => {
            match props.iter().find(|prop| &prop.name == over) {
                None => report(
                    cx,
                    findings,
                    codes::SCHEMA,
                    file,
                    format!("{ptr}/over"),
                    format!("repeat over {over}, which is not a prop"),
                    Some(component_id),
                ),
                Some(prop) if prop.ty != PropType::List => report(
                    cx,
                    findings,
                    codes::SCHEMA,
                    file,
                    format!("{ptr}/over"),
                    format!("repeat over {over}, which is not a list prop"),
                    Some(component_id),
                ),
                Some(_) => {}
            }
            check_node(
                cx,
                findings,
                file,
                &format!("{ptr}/body"),
                body,
                props,
                prop_names,
                slots,
                what,
                component_id,
            );
        }
        Node::When { test, then } => {
            check_expression(
                findings,
                cx.app,
                file,
                format!("{ptr}/test"),
                test,
                &Scope::Props(prop_names),
                what,
            );
            check_node(
                cx,
                findings,
                file,
                &format!("{ptr}/then"),
                then,
                props,
                prop_names,
                slots,
                what,
                component_id,
            );
        }
        Node::Control {
            value, on_click, ..
        } => {
            if let Some(value) = value {
                check_expression(
                    findings,
                    cx.app,
                    file,
                    format!("{ptr}/value"),
                    value,
                    &Scope::Props(prop_names),
                    what,
                );
            }
            if let Some(action) = on_click
                && cx.index.action(action).is_none()
            {
                report(
                    cx,
                    findings,
                    codes::UNKNOWN_ACTION,
                    file,
                    format!("{ptr}/on_click"),
                    format!("action {action} is not in this document"),
                    Some(component_id),
                );
            }
        }
        Node::Use {
            component,
            props: values,
            fill,
        } => {
            for (name, value) in values {
                check_expression(
                    findings,
                    cx.app,
                    file,
                    format!("{ptr}/props/{name}"),
                    value,
                    &Scope::Props(prop_names),
                    what,
                );
            }
            let Some(target) = own_component(cx, findings, file, ptr, component, component_id)
            else {
                return;
            };
            for prop in &target.props {
                if prop.required == Some(true) && !values.contains_key(&prop.name) {
                    report(
                        cx,
                        findings,
                        codes::SCHEMA,
                        file,
                        format!("{ptr}/props"),
                        format!("component {} requires prop {}", target.id, prop.name),
                        Some(component_id),
                    );
                }
            }
            for name in values.keys() {
                if !target.props.iter().any(|prop| &prop.name == name) {
                    report(
                        cx,
                        findings,
                        codes::SCHEMA,
                        file,
                        format!("{ptr}/props/{name}"),
                        format!("component {} has no prop {name}", target.id),
                        Some(component_id),
                    );
                }
            }
            for name in fill.keys() {
                if !target.slots.contains(name) {
                    report(
                        cx,
                        findings,
                        codes::SCHEMA,
                        file,
                        format!("{ptr}/fill/{name}"),
                        format!("component {} has no slot {name}", target.id),
                        Some(component_id),
                    );
                }
            }
            for (name, body) in fill {
                check_node(
                    cx,
                    findings,
                    file,
                    &format!("{ptr}/fill/{name}"),
                    body,
                    props,
                    prop_names,
                    slots,
                    what,
                    component_id,
                );
            }
        }
        Node::Slot { name } => {
            if !slots.contains(name) {
                report(
                    cx,
                    findings,
                    codes::SCHEMA,
                    file,
                    format!("{ptr}/name"),
                    format!("slot {name} is not declared by this component"),
                    Some(component_id),
                );
            }
        }
    }
}
