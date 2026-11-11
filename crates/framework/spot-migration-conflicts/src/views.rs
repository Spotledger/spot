//! View layers against the new metadata.
//!
//! The layers are resolved over the new metadata with `spot_views::resolve`, which drops a diff
//! naming a field the DocType no longer has and records a warning; the resolved view is then
//! checked with `spot_views::validate_layer`, which catches a removed field in a column or sort
//! and a hidden required field. Only a break the release caused is reported: a warning about a
//! field the plan does not remove is a pre-existing site problem, not this release's business.

use std::collections::BTreeMap;

use spot_views::{
    LayerName, ViewDef, ViewErrorKind, ViewKind, ViewLayer, default_view, resolve, validate_layer,
};

use crate::collect::Collector;
use crate::input::ReleaseContext;
use crate::model::{ArtefactKind, ArtefactRef, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the view-layer checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    let mut by_doctype: BTreeMap<&str, Vec<&ViewLayer>> = BTreeMap::new();
    for layer in ctx.view_layers {
        by_doctype
            .entry(layer.doctype.as_str())
            .or_default()
            .push(layer);
    }

    for (doctype, layers) in by_doctype {
        let artefacts: Vec<ArtefactRef> =
            layers.iter().map(|layer| layer_artefact(layer)).collect();
        if let Some((cause, resolution)) = delta.doctype_cause(doctype) {
            for artefact in &artefacts {
                out.push(
                    ConflictCategory::ViewDocTypeRemoved,
                    cause.clone(),
                    artefact.clone(),
                    resolution.clone(),
                );
            }
            continue;
        }
        let Some(base) = base_view(ctx, doctype) else {
            continue;
        };
        let owned: Vec<ViewLayer> = layers.iter().map(|layer| (*layer).clone()).collect();
        let resolved = resolve(&base, &owned, ctx.new_meta);

        for warning in &resolved.warnings {
            if warning.code != ViewErrorKind::UnknownField.code() {
                continue;
            }
            let Some(field) = &warning.subject else {
                continue;
            };
            if let Some((cause, resolution)) = delta.field_cause(doctype, field) {
                let artefact = artefacts
                    .get(warning.layer)
                    .cloned()
                    .unwrap_or_else(|| view_artefact(doctype));
                out.push(
                    ConflictCategory::ViewFieldRemoved,
                    cause,
                    artefact,
                    resolution,
                );
            }
        }

        for error in validate_layer(&resolved, ctx.new_meta) {
            let Some(field) = &error.field else {
                continue;
            };
            match error.kind {
                ViewErrorKind::UnknownField => {
                    if let Some((cause, resolution)) = delta.field_cause(doctype, field) {
                        out.push(
                            ConflictCategory::ViewFieldRemoved,
                            cause,
                            view_artefact(doctype),
                            resolution,
                        );
                    }
                }
                ViewErrorKind::HiddenRequired => {
                    if let Some((cause, resolution)) = delta.tightened_cause(doctype, field) {
                        out.push(
                            ConflictCategory::ViewHiddenRequired,
                            cause,
                            view_artefact(doctype),
                            resolution,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// The base view a DocType's layers diff over: the one the caller supplied, or the generated
/// default form when none was supplied. `None` when the DocType is not in the new metadata.
fn base_view(ctx: &ReleaseContext<'_>, doctype: &str) -> Option<ViewDef> {
    ctx.base_views
        .iter()
        .find(|view| view.doctype == doctype)
        .cloned()
        .or_else(|| default_view(ctx.new_meta, doctype, ViewKind::Form).ok())
}

/// The artefact id of one layer: `<doctype>:<kind>` with the role or step name appended.
fn layer_artefact(layer: &ViewLayer) -> ArtefactRef {
    let kind = match layer.kind.kind {
        LayerName::Standard => "standard",
        LayerName::Site => "site",
        LayerName::Role => "role",
        LayerName::Step => "step",
    };
    let mut id = format!("{}:{}", layer.doctype, kind);
    if let Some(role) = &layer.kind.role {
        id.push(':');
        id.push_str(role);
    }
    if let Some(step) = &layer.kind.step {
        id.push(':');
        id.push_str(step);
    }
    ArtefactRef::new(ArtefactKind::ViewLayer, id)
}

/// The artefact id of a DocType's view as a whole, for a break that cannot be tied to one layer.
fn view_artefact(doctype: &str) -> ArtefactRef {
    ArtefactRef::new(ArtefactKind::ViewLayer, format!("{doctype}:view"))
}
