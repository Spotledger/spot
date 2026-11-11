//! The link graph, and lints L1 (closed links) and L2 (direction).

use std::collections::BTreeSet;

use spot_meta::{Definition, FieldType};

use super::{Ctx, Finding, codes};
use crate::preset::Preset;

/// One link field of an entity or of one of its child tables, with its possible targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The DocType that declares the field (the entity or one of its child tables).
    pub owner: String,
    /// The field name.
    pub field: String,
    /// The field as shown to the author: `field`, or `table.field` for a child row field.
    pub path: String,
    /// The DocTypes the link may point to: one for a link, the `allowed_targets` for a dynamic
    /// link.
    pub targets: Vec<String>,
    /// A dynamic link with no declared `allowed_targets`.
    pub undeclared: bool,
    /// The link is marked `requires_adopted`.
    pub requires_adopted: bool,
    /// The author accepts that the target arrives by reference only.
    pub carried: bool,
}

/// The links of `root`, in field order: its own and those of its child tables, depth first. A
/// cycle through child tables is walked once.
pub(crate) fn edges_of(ctx: &Ctx<'_>, root: &Definition) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut visited = BTreeSet::new();
    visited.insert(root.name.clone());
    collect(ctx, root, "", &mut visited, &mut edges);
    edges
}

fn collect(
    ctx: &Ctx<'_>,
    definition: &Definition,
    prefix: &str,
    visited: &mut BTreeSet<String>,
    edges: &mut Vec<Edge>,
) {
    for field in &definition.fields {
        let Some(options) = field.options.as_deref() else {
            continue;
        };
        let path = format!("{prefix}{}", field.name);
        match field.ty {
            FieldType::Link => edges.push(edge(
                ctx,
                definition,
                &field.name,
                path,
                vec![options.to_owned()],
                false,
            )),
            FieldType::DynamicLink => {
                let declared = ctx
                    .extras
                    .allowed_targets(&definition.name, &field.name)
                    .filter(|targets| !targets.is_empty());
                let undeclared = declared.is_none();
                edges.push(edge(
                    ctx,
                    definition,
                    &field.name,
                    path,
                    declared.unwrap_or_default(),
                    undeclared,
                ));
            }
            FieldType::Table | FieldType::TableMultiselect => {
                if visited.insert(options.to_owned())
                    && let Some(child) = ctx.defs.get(options)
                {
                    collect(ctx, child, &format!("{path}."), visited, edges);
                }
            }
            _ => {}
        }
    }
}

fn edge(
    ctx: &Ctx<'_>,
    definition: &Definition,
    field: &str,
    path: String,
    targets: Vec<String>,
    undeclared: bool,
) -> Edge {
    Edge {
        owner: definition.name.clone(),
        field: field.to_owned(),
        path,
        targets,
        undeclared,
        requires_adopted: ctx.extras.requires_adopted(&definition.name, field),
        carried: ctx.extras.carried_by_reference(&definition.name, field),
    }
}

/// L1 and L2 over every entity that syncs.
pub(crate) fn lint_links(ctx: &Ctx<'_>, out: &mut Vec<Finding>) {
    for root in ctx.roots() {
        let preset = ctx.preset(&root.name);
        if !preset.is_synced() {
            continue;
        }
        for edge in edges_of(ctx, root) {
            if edge.undeclared {
                out.push(Finding::new(
                    codes::LINT_DYNAMIC_LINK_UNDECLARED,
                    &root.name,
                    format!(
                        "dynamic link `{}` declares no `allowed_targets`, so the closed-link \
                         check cannot tell what it points to; declare the list",
                        edge.path
                    ),
                    &[],
                ));
            }
            for target in &edge.targets {
                // A target that is not in the metadata cannot be judged here; a child table
                // follows its parent's policy and has none of its own.
                let Some(target_def) = ctx.defs.get(target.as_str()) else {
                    continue;
                };
                if target_def.is_child {
                    continue;
                }
                let target_preset = ctx.preset(target);
                if target_preset == Preset::Local {
                    out.push(Finding::new(
                        codes::LINT_CLOSED_LINK,
                        &root.name,
                        format!(
                            "`{}` is {preset} but its link `{}` points to `{target}`, which is \
                             `local`; the destination could never resolve it",
                            root.name, edge.path
                        ),
                        &[target],
                    ));
                }
                if preset == Preset::Master
                    && matches!(target_preset, Preset::Record | Preset::Handover)
                    && !edge.requires_adopted
                {
                    out.push(Finding::new(
                        codes::LINT_MASTER_DEPENDS_ON_BRANCH,
                        &root.name,
                        format!(
                            "master `{}` links through `{}` to {target_preset} `{target}`, so \
                             hub data would depend on branch data; mark the link \
                             `requires_adopted` or point it at a master",
                            root.name, edge.path
                        ),
                        &[target],
                    ));
                }
            }
        }
    }
}
