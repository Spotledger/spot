//! The host adapter the suite ships: a [`LivenessHost`] over [`spot_meta::Registry`], and the
//! conversion from the suite's change inputs to the registry's API. `tests/meta.rs` runs the
//! whole suite through [`StandardHost`].
//!
//! What this adapter proves about the *contract* (not about any one crate): an ADR-05 host is
//! "definitions + customisations + merge + version". `spot_meta::Registry` is exactly that
//! today; WP-MET-02 (plan probe), WP-STO-11 (schema probe) and WP-UI-10 (view probe) replace
//! or extend [`StandardHost::probe`].

use std::sync::Arc;

use spot_meta::{Definition, EffectiveMeta, FieldSpec, MetaErrorKind, Property, Registry};

use crate::change::{
    AddDocTypeDef, Change, ChangeEffect, DocTypeDefInput, FieldSpecInput, HostError, Refusal,
    SetProperty,
};
use crate::{HostFactory, LivenessHost, Probe, ProbeOutcome};

// ------------------------------------------------------------------------------------------
// The registry adapter
// ------------------------------------------------------------------------------------------

/// A [`LivenessHost`] over [`spot_meta::Registry`], starting from the standard two DocTypes
/// (`party`, `invoice`). The suite's reference adapter; `tests/meta.rs` runs every scenario
/// on it.
pub struct StandardHost {
    definitions: Vec<Definition>,
    registry: Registry,
}

/// Builds fresh [`StandardHost`]s.
pub struct StandardFactory;

impl HostFactory for StandardFactory {
    fn fresh(&self) -> Box<dyn LivenessHost> {
        Box::new(StandardHost::new())
    }
}

impl StandardHost {
    /// A host over the standard two DocTypes.
    ///
    /// # Panics
    /// Never in practice: the standard definitions build by construction; a panic here is a bug
    /// in the suite, not a refused change.
    pub fn new() -> Self {
        let definitions = standard_definitions();
        let registry = Registry::new(definitions.clone())
            .unwrap_or_else(|e| panic!("the standard definitions do not build: {e:?}"));
        Self {
            definitions,
            registry,
        }
    }
}

impl Default for StandardHost {
    fn default() -> Self {
        Self::new()
    }
}

impl LivenessHost for StandardHost {
    fn apply(&mut self, change: &Change) -> Result<ChangeEffect, HostError> {
        let version = match change {
            Change::AddCustomField {
                doctype,
                spec,
                insert_after,
            } => apply_add(self, doctype, spec.clone(), insert_after.as_deref())?,
            Change::RemoveCustomField { doctype, name } => self
                .registry
                .remove_custom_field(doctype, name)
                .map_err(refuse)?,
            Change::SetProperty {
                doctype,
                field,
                property,
            } => self
                .registry
                .set_property(doctype, field.as_deref(), property_of(property))
                .map_err(refuse)?,
            Change::AddDocType { definition } => {
                let mut definitions = self.definitions.clone();
                definitions.push(definition_of(&runtime_as_standard(definition)));
                self.registry
                    .set_definitions(definitions.clone())
                    .map_err(refuse)?;
                self.definitions = definitions;
                // `set_definitions` reports the version after the whole set was rebuilt.
                self.registry.effective().version()
            }
            Change::ReplaceDefinitions { definitions } => {
                let defs: Vec<Definition> = definitions.iter().map(definition_of).collect();
                self.registry
                    .set_definitions(defs.clone())
                    .map_err(refuse)?;
                self.definitions = defs;
                self.registry.effective().version()
            }
        };
        Ok(ChangeEffect {
            new_version: version,
        })
    }

    fn meta(&self) -> Arc<EffectiveMeta> {
        self.registry.effective()
    }

    fn probe(&mut self, _layer: Probe) -> ProbeOutcome {
        // The registry keeps no plan, schema-migration or view layer of its own; WP-MET-02,
        // WP-STO-11 and WP-UI-10 provide the hosts that do. Reporting NotSupported keeps the
        // scenarios honest instead of asserting a layer that is not there.
        ProbeOutcome::NotSupported
    }
}

/// Adds one custom field, converting the field input first.
fn apply_add(
    host: &mut StandardHost,
    doctype: &str,
    spec: FieldSpecInput,
    insert_after: Option<&str>,
) -> Result<u64, HostError> {
    host.registry
        .add_custom_field(doctype, spec.into_meta_spec(), insert_after)
        .map_err(refuse)
}

/// Maps the registry's error list to the suite's typed refusal. Validation reports every
/// problem; the first decides the kind, and scenarios assert only what ADRs fix (a refusal and
/// an unmoved version), never the wording.
#[must_use]
pub fn refusal_of(errors: &[spot_meta::MetaError]) -> Option<Refusal> {
    let first = errors.first()?;
    let refusal = match first.kind {
        MetaErrorKind::UnknownName => Refusal::UnknownName,
        MetaErrorKind::ReservedName => Refusal::ReservedName,
        MetaErrorKind::BadCustomField => Refusal::BadCustomFieldName,
        // A duplicate names a DocType twice (a runtime DocType colliding with a standard one)
        // or a field (a custom field colliding with a standard one); carry the name either way.
        MetaErrorKind::Duplicate => match &first.field {
            Some(field) => Refusal::FieldCollision {
                doctype: first.doctype.clone().unwrap_or_default(),
                field: field.clone(),
            },
            None => Refusal::NameCollision {
                name: first.doctype.clone().unwrap_or_default(),
            },
        },
        MetaErrorKind::BadOverride => Refusal::BadOverride,
        MetaErrorKind::BadTable
        | MetaErrorKind::BadTitleField
        | MetaErrorKind::BadProperty
        | MetaErrorKind::BadDefault
        | MetaErrorKind::BadOptions
        | MetaErrorKind::BadIndex
        | MetaErrorKind::InvalidName
        | MetaErrorKind::UnknownSeam
        | MetaErrorKind::NotYetSupported
        | MetaErrorKind::GeolocationNotAPoint
        | MetaErrorKind::BadExclusion
        | MetaErrorKind::BadSearch
        | MetaErrorKind::SearchFieldProtected
        | MetaErrorKind::BadRange
        | MetaErrorKind::BackendUnsupported
        | MetaErrorKind::UnknownTarget => Refusal::InvalidDefinitions,
        MetaErrorKind::Io
        | MetaErrorKind::Parse
        | MetaErrorKind::FileName
        | MetaErrorKind::Compile => Refusal::BadInput {
            why: first.message.clone(),
        },
    };
    Some(refusal)
}

/// Maps the registry's error list to the suite's typed refusal; an empty list becomes the
/// unexplained `BadInput` (a bug in the adapter, surfaced, never swallowed).
fn refuse(errors: Vec<spot_meta::MetaError>) -> HostError {
    let refusal = refusal_of(&errors).unwrap_or(Refusal::BadInput {
        why: "refused without naming a cause".to_owned(),
    });
    HostError { refusal }
}

// ------------------------------------------------------------------------------------------
// Input conversion: the suite expresses changes in its own input types, so a scenario never
// depends on a constructor shape the host does not control.
// ------------------------------------------------------------------------------------------

/// Converts the suite's property to the metadata one.
#[must_use]
pub fn property_of(property: &SetProperty) -> Property {
    match property {
        SetProperty::Label(v) => Property::Label(v.clone()),
        SetProperty::Required(v) => Property::Required(*v),
        SetProperty::ReadOnly(v) => Property::ReadOnly(*v),
        SetProperty::AllowOnPosted(v) => Property::AllowOnPosted(*v),
        SetProperty::Default(v) => Property::Default(v.clone()),
        SetProperty::Options(v) => Property::Options(v.clone()),
        SetProperty::MaxChars(v) => Property::MaxChars(*v),
        SetProperty::TitleField(v) => Property::TitleField(v.clone()),
    }
}

/// Converts the suite's field input to a [`FieldSpec`]. Unknown type keys are a suite bug and
/// panic; a refused *change* is a host answer and never panics.
#[must_use]
pub fn spec_of(input: &FieldSpecInput) -> FieldSpec {
    input.clone().into_meta_spec()
}

/// Converts the suite's DocType input to a [`Definition`].
#[must_use]
pub fn definition_of(input: &DocTypeDefInput) -> Definition {
    let mut d = Definition::new(&input.name);
    d.label = input.label.clone();
    d.is_posted = input.is_posted;
    d.title_field = input.title_field.clone();
    d.fields = input.fields.iter().map(spec_of).collect();
    d
}

/// Views a runtime DocType input as the standard input the converter takes.
fn runtime_as_standard(input: &AddDocTypeDef) -> DocTypeDefInput {
    DocTypeDefInput {
        name: input.name.clone(),
        label: input.label.clone(),
        is_posted: input.is_posted,
        title_field: input.title_field.clone(),
        fields: input.fields.clone(),
    }
}

/// The standard definitions, from the suite's scenario inputs.
fn standard_definitions() -> Vec<Definition> {
    crate::scenarios::standard_defs()
        .iter()
        .map(definition_of)
        .collect()
}
