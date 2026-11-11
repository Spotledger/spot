//! Metadata source: the effective DocTypes, their fields and the site's customisations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_meta::{Customisations, Definition, EffectiveMeta, FieldType, Property, Registry};

use crate::SelfModelError;
use crate::adapters::{push_edge, push_node, push_provenance_cause};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry, Provenance};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData, node_id};

/// Builds the metadata part of the graph from `spot-meta`'s effective metadata and the
/// customisations that produced it.
///
/// Customisation provenance (who changed what, when and what caused it) is not part of
/// `Customisations`; callers that have it supply it with [`MetadataAdapter::with_provenance`].
/// A customisation without provenance still appears with its revision as evidence, but
/// contributes no timed history and no causal edge.
#[derive(Clone, Debug)]
pub struct MetadataAdapter {
    meta: Arc<EffectiveMeta>,
    customisations: Customisations,
    provenance: BTreeMap<String, Provenance>,
}

impl MetadataAdapter {
    /// An adapter over one effective metadata snapshot and its customisations.
    #[must_use]
    pub fn new(meta: Arc<EffectiveMeta>, customisations: Customisations) -> Self {
        Self {
            meta,
            customisations,
            provenance: BTreeMap::new(),
        }
    }

    /// An adapter over a registry's current metadata and customisations.
    #[must_use]
    pub fn from_registry(registry: &Registry) -> Self {
        Self::new(registry.effective(), registry.customisations().clone())
    }

    /// Adds the provenance of the customisation with this key.
    ///
    /// The keys are `custom_field:<doctype>.<field>` and
    /// `override:<doctype>.<field-or-dash>.<property>`.
    #[must_use]
    pub fn with_provenance(mut self, key: impl Into<String>, provenance: Provenance) -> Self {
        self.provenance.insert(key.into(), provenance);
        self
    }

    fn customisation_evidence(&self, key: &str) -> EvidenceRef {
        EvidenceRef::untimed(
            EvidenceSource::Customisation,
            format!("revision:{}:{key}", self.customisations.revision()),
        )
    }

    fn history(
        &self,
        entries: &mut Vec<HistoryEntry>,
        target: &NodeRef,
        key: &str,
        change: ChangeKind,
        detail: String,
    ) {
        if let Some(provenance) = self.provenance.get(key) {
            entries.push(HistoryEntry::new(
                target.id(),
                provenance.at,
                change,
                detail,
                provenance.evidence(EvidenceSource::Customisation),
            ));
        }
    }
}

impl SourceAdapter for MetadataAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Metadata
    }

    /// The metadata version.
    ///
    /// **Informational.** The version can repeat for different content — a rollback, or a
    /// snapshot adopted from another node, can serve changed DocTypes at a number this node
    /// already used — so it is a label for *when* the source was read, never the identity of
    /// what it holds. The identity is in the evidence ids: every DocType's evidence carries a
    /// content hash of the definition next to the version (see [`MetadataAdapter::snapshot`]).
    fn epoch(&self) -> u64 {
        self.meta.version()
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        let version = self.meta.version();
        let known: BTreeSet<&str> = self
            .meta
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();

        for definition in self.meta.definitions() {
            // The version alone would repeat this id for two different definitions at one
            // version, which reads as "nothing changed" when everything did (WP-MET-03.T6).
            let digest = definition_digest(definition);
            let evidence = EvidenceRef::untimed(
                EvidenceSource::Metadata,
                format!("meta:{version}:{}:{digest}", definition.name),
            );
            let mut doctype = Node::new(
                NodeKind::DocType,
                definition.name.clone(),
                vec![evidence.clone()],
            )?
            .with_label(definition.label_or_name())
            .with_attribute("posted", definition.is_posted.to_string())
            .with_attribute("child", definition.is_child.to_string());
            if let Some(app) = &definition.app {
                doctype = doctype.with_app(app.clone());
            }
            let doctype_ref = doctype.reference();
            push_node(&mut data.nodes, doctype);

            for field in &definition.fields {
                let name = format!("{}.{}", definition.name, field.name);
                let mut node = Node::new(NodeKind::Field, name.clone(), vec![evidence.clone()])?
                    .with_label(field.label_or_name())
                    .with_attribute("doctype", definition.name.clone())
                    .with_attribute("field", field.name.clone())
                    .with_attribute("type", field_type(field.ty))
                    .with_attribute("required", field.required.to_string())
                    .with_attribute("read_only", field.read_only.to_string());
                if let Some(app) = &definition.app {
                    node = node.with_app(app.clone());
                }
                if let Some(options) = &field.options {
                    node = node.with_attribute("options", options.clone());
                }
                let field_ref = node.reference();
                push_node(&mut data.nodes, node);
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        doctype_ref.id(),
                        field_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
                if matches!(
                    field.ty,
                    FieldType::Link | FieldType::Table | FieldType::TableMultiselect
                ) && let Some(target) = field.options.as_deref()
                {
                    let target = target.trim();
                    if known.contains(target) {
                        push_edge(
                            &mut data.edges,
                            Edge::new(
                                field_ref.id(),
                                node_id(NodeKind::DocType, target),
                                EdgeKind::References,
                                None,
                                vec![evidence.clone()],
                            )?,
                        );
                    }
                }
            }
        }

        for custom in self.customisations.fields() {
            let key = format!("custom_field:{}.{}", custom.doctype, custom.spec.name);
            let evidence = self.customisation_evidence(&key);
            let target = NodeRef::new(
                NodeKind::Field,
                format!("{}.{}", custom.doctype, custom.spec.name),
            );
            let mut node = Node::new(NodeKind::Customisation, key.clone(), vec![evidence.clone()])?
                .with_attribute("kind", "custom_field")
                .with_attribute("doctype", custom.doctype.clone())
                .with_attribute("field", custom.spec.name.clone())
                .with_attribute("type", field_type(custom.spec.ty));
            if let Some(provenance) = self.provenance.get(&key) {
                node = node
                    .with_attribute("actor", provenance.actor.clone())
                    .with_attribute("at", provenance.at.to_string());
            }
            let custom_ref = node.reference();
            push_node(&mut data.nodes, node);
            if self.meta.definition(&custom.doctype).is_some() {
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        custom_ref.id(),
                        node_id(NodeKind::DocType, &custom.doctype),
                        EdgeKind::Overrides,
                        self.provenance.get(&key).map(|entry| entry.at),
                        vec![evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        custom_ref.id(),
                        target.id(),
                        EdgeKind::Overrides,
                        self.provenance.get(&key).map(|entry| entry.at),
                        vec![evidence],
                    )?,
                );
            }
            if let Some(provenance) = self.provenance.get(&key) {
                push_provenance_cause(
                    &mut data,
                    &custom_ref,
                    provenance,
                    EvidenceSource::Customisation,
                )?;
            }
            self.history(
                &mut data.history,
                &target,
                &key,
                ChangeKind::Created,
                format!("custom field {} was added", custom.spec.name),
            );
        }

        for over in self.customisations.overrides() {
            let field_key = over.field.as_deref().unwrap_or("-");
            let key = format!(
                "override:{}.{}.{}",
                over.doctype,
                field_key,
                over.property.key()
            );
            let evidence = self.customisation_evidence(&key);
            let target = match &over.field {
                Some(field) => NodeRef::new(NodeKind::Field, format!("{}.{field}", over.doctype)),
                None => NodeRef::new(NodeKind::DocType, over.doctype.clone()),
            };
            let mut node = Node::new(NodeKind::Customisation, key.clone(), vec![evidence.clone()])?
                .with_attribute("kind", "property_override")
                .with_attribute("doctype", over.doctype.clone())
                .with_attribute("property", over.property.key())
                .with_attribute("value", property_text(&over.property));
            if let Some(field) = &over.field {
                node = node.with_attribute("field", field.clone());
            }
            if let Some(provenance) = self.provenance.get(&key) {
                node = node
                    .with_attribute("actor", provenance.actor.clone())
                    .with_attribute("at", provenance.at.to_string());
            }
            let override_ref = node.reference();
            push_node(&mut data.nodes, node);
            if self.meta.definition(&over.doctype).is_some() {
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        override_ref.id(),
                        target.id(),
                        EdgeKind::Overrides,
                        self.provenance.get(&key).map(|entry| entry.at),
                        vec![evidence],
                    )?,
                );
                if let Some(provenance) = self.provenance.get(&key) {
                    push_provenance_cause(
                        &mut data,
                        &override_ref,
                        provenance,
                        EvidenceSource::Customisation,
                    )?;
                }
            }
            self.history(
                &mut data.history,
                &target,
                &key,
                ChangeKind::Updated,
                format!(
                    "{} was set to {}",
                    over.property.key(),
                    property_text(&over.property)
                ),
            );
        }

        Ok(data)
    }
}

/// A content hash of the facts this adapter records for one DocType.
///
/// The metadata version cannot identify a definition's content: an adopted snapshot or a
/// rollback can serve different DocTypes at a version this node used before, and an evidence id
/// that repeats for different content claims two states were one. The hash covers the DocType's
/// own recorded facts (name, label, the posted and child flags, the owning app) and every
/// field's name, label, type, required, read-only flag and options, in field order — the same
/// facts the snapshot emits as node attributes. A definition that differs only in facts this
/// adapter does not record gives the same id, which is what determinism asks for: equal
/// snapshots, equal evidence.
fn definition_digest(definition: &Definition) -> String {
    let mut text = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        definition.name,
        definition.label.as_deref().unwrap_or(""),
        definition.is_posted,
        definition.is_child,
        definition.app.as_deref().unwrap_or(""),
    );
    for field in &definition.fields {
        text.push('\u{1e}');
        text.push_str(&format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            field.name,
            field.label.as_deref().unwrap_or(""),
            field_type(field.ty),
            field.required,
            field.read_only,
            field.options.as_deref().unwrap_or(""),
        ));
    }
    spot_graph_model::model::sha256_hex(text.as_bytes())
}

/// The stable text form of a metadata field type.
fn field_type(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "bool",
        FieldType::Int => "int",
        FieldType::Decimal => "decimal",
        FieldType::Text => "text",
        FieldType::LongText => "long_text",
        FieldType::Select => "select",
        FieldType::Date => "date",
        FieldType::Time => "time",
        FieldType::DateTime => "datetime",
        FieldType::Link => "link",
        FieldType::DynamicLink => "dynamic_link",
        FieldType::Json => "json",
        FieldType::Bytes => "bytes",
        FieldType::Geo => "geo",
        FieldType::Range => "range",
        FieldType::Table => "table",
        FieldType::TableMultiselect => "table_multiselect",
        FieldType::Attach => "attach",
        FieldType::Secret => "secret",
    }
}

/// A non-sensitive text form of an overridden property value.
///
/// A default value may be sensitive, so it is reported as set or cleared, never verbatim.
fn property_text(property: &Property) -> String {
    match property {
        Property::Label(value) | Property::TitleField(value) => value.clone(),
        Property::DependsOn(value)
        | Property::MandatoryDependsOn(value)
        | Property::ReadOnlyDependsOn(value)
        | Property::FetchFrom(value)
        | Property::Control(value) => value.clone().unwrap_or_default(),
        Property::Required(value)
        | Property::ReadOnly(value)
        | Property::AllowOnPosted(value)
        | Property::Hidden(value)
        | Property::InListView(value)
        | Property::InStandardFilter(value)
        | Property::NoCopy(value) => value.to_string(),
        Property::Default(value)
        | Property::Description(value)
        | Property::ProtectionGroup(value) => {
            if value.is_some() {
                "set".to_owned()
            } else {
                "none".to_owned()
            }
        }
        Property::Lookup(value) => {
            if value.is_some() {
                "set".to_owned()
            } else {
                "none".to_owned()
            }
        }
        Property::Masking(value) => {
            if value.is_some() {
                "set".to_owned()
            } else {
                "none".to_owned()
            }
        }
        Property::Options(values) | Property::Classification(values) => values.join(","),
        Property::MaxChars(value) => value.to_string(),
        Property::Permlevel(value) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_meta::FieldSpec;

    #[test]
    fn control_hint_is_reported_as_its_name_or_empty() {
        assert_eq!(
            property_text(&Property::Control(Some("radio".to_owned()))),
            "radio"
        );
        assert_eq!(property_text(&Property::Control(None)), "");
    }

    #[test]
    fn a_definition_digest_follows_the_recorded_facts() {
        let mut one = Definition::new("sales_invoice");
        one.fields = vec![FieldSpec::new("total", FieldType::Decimal)];
        assert_eq!(
            definition_digest(&one),
            definition_digest(&one.clone()),
            "equal definitions hash equal"
        );

        let mut added = one.clone();
        added.fields.push(FieldSpec::new("memo", FieldType::Text));
        assert_ne!(
            definition_digest(&one),
            definition_digest(&added),
            "a field the snapshot records moves the digest"
        );

        let mut relabelled = one.clone();
        relabelled.label = Some("Invoice".to_owned());
        assert_ne!(definition_digest(&one), definition_digest(&relabelled));

        let mut read_only = one.clone();
        read_only.fields[0].read_only = true;
        assert_ne!(definition_digest(&one), definition_digest(&read_only));
    }
}
