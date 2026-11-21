//! Fixtures against the new metadata.
//!
//! A fixture is a JSON document a site loaded into a DocType. If the release removes the DocType,
//! the whole fixture is dead; if it removes or renames a field a document sets, the fixture no
//! longer loads into the new schema. Only the documents' top-level keys are checked, because a
//! nested object's keys belong to a child DocType this check does not resolve.

use crate::collect::Collector;
use crate::input::ReleaseContext;
use crate::model::{ArtefactKind, ArtefactRef, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the fixture checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    for fixture in ctx.fixtures {
        let doctype = fixture.doctype.as_str();
        let artefact = ArtefactRef::new(ArtefactKind::Fixture, doctype);
        if let Some((cause, resolution)) = delta.doctype_cause(doctype) {
            out.push(
                ConflictCategory::FixtureDocTypeRemoved,
                cause,
                artefact,
                resolution,
            );
            continue;
        }
        for document in &fixture.documents {
            let Some(object) = document.as_object() else {
                continue;
            };
            for key in object.keys() {
                if let Some((cause, resolution)) = delta.field_cause(doctype, key) {
                    out.push(
                        ConflictCategory::FixtureFieldRemoved,
                        cause,
                        artefact.clone(),
                        resolution,
                    );
                }
            }
        }
    }
}
