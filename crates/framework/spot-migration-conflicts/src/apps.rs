//! Installed apps against the new host and metadata.
//!
//! An app declares what it needs: the WIT world its bundle targets, a `requires_host` version
//! range, the DocTypes it subscribes to, reads, writes and loads fixtures into, and the hook
//! points it runs at. If the release publishes a host the app's world does not match, drops a
//! host version the app accepts, removes a DocType the app names, or removes the field a
//! `field_changed:<field>` hook watches, the app must be updated before the release goes out.
//!
//! Capability entries name DocTypes by label (`Sales Invoice`), not storage name, so a label is
//! mapped to a storage name the documented way (lower-case, spaces to underscores) before it is
//! matched against the plan.

use std::collections::BTreeSet;

use spot_app_api::HookPoint;

use crate::collect::Collector;
use crate::input::{HostSpec, InstalledApp, ReleaseContext};
use crate::model::{ArtefactKind, ArtefactRef, Cause, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the installed-app checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    for app in ctx.apps {
        let name = app.name().to_owned();
        let artefact = ArtefactRef::new(ArtefactKind::App, name.clone());

        if let Some(host) = ctx.host {
            wit_world(app, host, &artefact, out);
            requires_host(app, host, &artefact, out);
        }
        doctypes(ctx, delta, app, &artefact, out);
        hooks(delta, app, &artefact, out);
    }
}

/// The WIT world an app's bundle targets must be the one the new host serves.
fn wit_world(app: &InstalledApp, host: &HostSpec, artefact: &ArtefactRef, out: &mut Collector) {
    if app.wit_world != host.wit_world {
        out.push(
            ConflictCategory::AppWitWorldMismatch,
            Cause::new(
                "release/host/wit_world",
                format!("the new host serves the WIT world `{}`", host.wit_world),
            ),
            artefact.clone(),
            format!(
                "Update `{}` to a bundle that targets the WIT world `{}`.",
                app.name(),
                host.wit_world
            ),
        );
    }
}

/// The app's `requires_host` range must admit the new host version.
fn requires_host(app: &InstalledApp, host: &HostSpec, artefact: &ArtefactRef, out: &mut Collector) {
    let Some(req) = &app.manifest.app.requires_host else {
        return;
    };
    if !req.matches(&host.version) {
        out.push(
            ConflictCategory::AppHostIncompatible,
            Cause::new(
                "release/host/version",
                format!("the new host is version {}", host.version),
            ),
            artefact.clone(),
            format!(
                "Update `{}` so its `requires_host` admits host version {}.",
                app.name(),
                host.version
            ),
        );
    }
}

/// Every DocType the app names must still exist after the release.
///
/// A manifest names DocTypes by label (`Sales Invoice`), not storage name, so each label is
/// mapped the documented way before it is matched: a definition whose label is the label, or a
/// definition whose storage name is the label lower-cased with spaces to underscores.
fn doctypes(
    ctx: &ReleaseContext<'_>,
    delta: &ReleaseDelta,
    app: &InstalledApp,
    artefact: &ArtefactRef,
    out: &mut Collector,
) {
    for label in referenced_labels(app) {
        let storage = label_to_storage(&label);
        let known = ctx.new_meta.definition(&storage).is_some()
            || ctx
                .new_meta
                .definitions()
                .iter()
                .any(|d| d.label_or_name() == label);
        if !known {
            report_doctype(delta, app, artefact, &storage, out);
        }
    }
}

/// Records an app depending on a removed DocType, when the release removes it.
fn report_doctype(
    delta: &ReleaseDelta,
    app: &InstalledApp,
    artefact: &ArtefactRef,
    doctype: &str,
    out: &mut Collector,
) {
    if let Some((cause, _)) = delta.doctype_cause(doctype) {
        out.push(
            ConflictCategory::AppDocTypeRemoved,
            cause,
            artefact.clone(),
            format!(
                "Update `{}` so it no longer needs the DocType `{doctype}`, or keep the DocType in the release.",
                app.name()
            ),
        );
    }
}

/// The hook points the app subscribes to that watch one field.
fn hooks(delta: &ReleaseDelta, app: &InstalledApp, artefact: &ArtefactRef, out: &mut Collector) {
    for event in &app.manifest.events {
        let HookPoint::FieldChanged(field) = &event.event else {
            continue;
        };
        let doctype = label_to_storage(&event.doctype);
        if let Some((cause, _)) = delta.field_cause(&doctype, field) {
            out.push(
                ConflictCategory::AppHookRemoved,
                cause,
                artefact.clone(),
                format!(
                    "Update `{}` so its `field_changed` hook no longer watches `{doctype}.{field}`.",
                    app.name()
                ),
            );
        }
    }
}

/// Every DocType label an app names: its events, methods, fixtures and capabilities.
fn referenced_labels(app: &InstalledApp) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for event in &app.manifest.events {
        out.insert(event.doctype.clone());
    }
    for method in &app.manifest.methods {
        out.insert(method.doctype.clone());
    }
    for fixture in &app.manifest.fixtures {
        out.insert(fixture.doctype.clone());
    }
    for label in &app.manifest.capabilities.doctypes.read {
        out.insert(label.clone());
    }
    for label in &app.manifest.capabilities.doctypes.write {
        out.insert(label.clone());
    }
    out
}

/// The storage name a capability label maps to (lower-case, spaces to underscores).
fn label_to_storage(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c == ' ' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}
