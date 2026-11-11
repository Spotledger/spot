//! The registry: merges definitions with customisations into the effective metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::meta::DocTypeDef;
use spot_storage_contract::schema::{Catalog, Collation};

use crate::compile;
use crate::custom::{CUSTOM_PREFIX, CustomField, Customisations, Override, Property};
use crate::definition::{Definition, FieldLookup, FieldSpec, FieldType, MaskSpec};
use crate::derive::{self, Derivation, DerivationSource, FieldRef, InheritanceMap};
use crate::descriptor::{descriptor, doctype_descriptor};
use crate::error::{MetaError, MetaErrorKind as K};
use crate::report::{CompileReport, Warning};
use crate::validate::{AMENDED_FROM, DOC_STATE, validate_with_seams};

/// Extra inputs of a build: what the platform knows beyond the definitions.
///
/// `known_seams` set means the build checks every lookup's seam against the set: a seam not in
/// it (and not [`crate::PLATFORM_LINK_SEARCH`]) is refused, naming the DocType, the field and the
/// seam (ADR-068 §3.7). `None`, and [`Registry::build`], run without the seam check.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildOptions {
    /// The seam names this build accepts in a `lookup`, or `None` for the no-check behaviour.
    pub known_seams: Option<BTreeSet<String>>,
    /// The derivations (a derived field and its inputs) known to this build. They drive
    /// computed inheritance of protection groups and the `meta.derived_unprotected` lint.
    /// WP-MET-02 supplies them from expressions; until then the caller does.
    pub derivations: Vec<Derivation>,
}

impl BuildOptions {
    /// No extra inputs: every build behaves like [`Registry::build`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the derivations of `source` into this build.
    #[must_use]
    pub fn with_derivations(mut self, source: &dyn DerivationSource) -> Self {
        self.derivations = source.derivations();
        self
    }
}

/// The protection information for a single field, computed for spot-permissions (ADR-070 section 12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldProtection {
    /// The field's path (doctype.fieldname).
    pub path: String,
    /// Protection groups: the field's own group plus inherited groups, sorted and deduplicated.
    pub groups: Vec<String>,
    /// Classification tags (secret, pii, etc).
    pub tags: Vec<String>,
    /// How to mask the value for partial access (read_masked).
    pub mask: Option<String>,
    /// Groups that write access traces to the audit log (rule 9).
    pub trace_groups: Vec<String>,
}

/// The rule properties of a field, resolved and still text (ADR-068 §1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldRules {
    /// Shown while this expression holds.
    pub depends_on: Option<String>,
    /// Required while this expression holds.
    pub mandatory_depends_on: Option<String>,
    /// Read-only while this expression holds.
    pub read_only_depends_on: Option<String>,
    /// `<link_field>.<target_field>`.
    pub fetch_from: Option<String>,
    /// The default's expression text (without the `=`) when the default is an expression.
    pub default_expression: Option<String>,
}

impl FieldRules {
    /// Whether the field's default is an expression rather than a literal.
    pub fn default_is_expression(&self) -> bool {
        self.default_expression.is_some()
    }
}

/// The merged, validated metadata at one version.
///
/// Immutable: a change builds a new one, so an operation holding this keeps a consistent view
/// (ADR-05).
#[derive(Clone, Debug)]
pub struct EffectiveMeta {
    version: u64,
    definitions: Vec<Definition>,
    views: BTreeMap<String, Arc<DocTypeDef>>,
    /// Declassified fields: (doctype.field, reason) pairs for every field that is declassified.
    declassifications: Vec<(String, String)>,
    /// Groups every derived field inherits from its inputs (computed, never authored).
    inheritance: InheritanceMap,
    /// What compiling found but did not refuse.
    report: CompileReport,
}

impl EffectiveMeta {
    /// The metadata version: it grows whenever anything changes. It is also stamped on the
    /// compiled [`Catalog`] and on every [`DocTypeDef`].
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Every DocType with customisations merged and the engine-managed fields added, in name
    /// order.
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// One effective DocType.
    pub fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|d| d.name == name)
    }

    /// The rule properties of one field as resolved (definition, then site override), for
    /// `spot-plan` (ADR-068 §1). The app API's `FieldDef` has no slot for them yet, so this
    /// is their app view. Nothing is parsed or evaluated.
    pub fn field_rules(&self, doctype: &str, field: &str) -> Option<FieldRules> {
        let f = self
            .definition(doctype)?
            .fields
            .iter()
            .find(|f| f.name == field)?;
        Some(FieldRules {
            depends_on: f.depends_on.clone(),
            mandatory_depends_on: f.mandatory_depends_on.clone(),
            read_only_depends_on: f.read_only_depends_on.clone(),
            fetch_from: f.fetch_from.clone(),
            default_expression: f.default_expression().map(str::to_owned),
        })
    }

    /// The naming property of a DocType, as written.
    pub fn naming(&self, doctype: &str) -> Option<&str> {
        self.definition(doctype)?.naming.as_deref()
    }

    /// The app API view of a DocType.
    pub fn doctype(&self, name: &str) -> Option<Arc<DocTypeDef>> {
        self.views.get(name).cloned()
    }

    /// The app API view of every DocType, in name order.
    pub fn doctypes(&self) -> impl Iterator<Item = &Arc<DocTypeDef>> {
        self.views.values()
    }

    /// Compiles the storage catalog. Deterministic: the same metadata gives an equal catalog.
    pub fn catalog(&self, collation: Collation) -> Result<Catalog, MetaError> {
        compile::catalog(self.version, &self.definitions, collation)
    }

    /// As [`EffectiveMeta::catalog`], for a backend with the given `capabilities`: a DocType
    /// that declares an exclusion or a search the backend cannot serve is refused with
    /// [`crate::MetaErrorKind::BackendUnsupported`] (`meta.backend_unsupported`) at compile time, not
    /// at the first query. Geo storage and plain range fields are not refused here; see
    /// [`compile::catalog_for`].
    pub fn catalog_for(
        &self,
        collation: Collation,
        capabilities: &spot_storage_contract::Capabilities,
    ) -> Result<Catalog, MetaError> {
        compile::catalog_for(self.version, &self.definitions, collation, capabilities)
    }

    /// As [`EffectiveMeta::catalog`], with the `reverse` indexes of traversable Link fields (see
    /// [`crate::ReverseIndex`]). An empty list gives the catalog of [`EffectiveMeta::catalog`].
    ///
    /// # Errors
    /// [`crate::MetaErrorKind::Compile`] when an entry names an unknown DocType, a field that is
    /// not a Link, or an index name already taken.
    pub fn catalog_with_reverse_indexes(
        &self,
        collation: Collation,
        reverse: &[compile::ReverseIndex],
    ) -> Result<Catalog, MetaError> {
        compile::catalog_with_reverse_indexes(self.version, &self.definitions, collation, reverse)
    }

    /// As [`EffectiveMeta::catalog_for`], with the `reverse` indexes of
    /// [`EffectiveMeta::catalog_with_reverse_indexes`].
    ///
    /// # Errors
    /// As [`EffectiveMeta::catalog_for`] and [`EffectiveMeta::catalog_with_reverse_indexes`].
    pub fn catalog_for_with_reverse_indexes(
        &self,
        collation: Collation,
        capabilities: &spot_storage_contract::Capabilities,
        reverse: &[compile::ReverseIndex],
    ) -> Result<Catalog, MetaError> {
        compile::catalog_for_with_reverse_indexes(
            self.version,
            &self.definitions,
            collation,
            capabilities,
            reverse,
        )
    }

    /// The protection information for every field, in order by DocType then field name.
    /// This is the data structure spot-permissions uses to enforce field access control.
    pub fn field_protection(&self) -> Vec<FieldProtection> {
        let mut result = Vec::new();
        for d in &self.definitions {
            for f in &d.fields {
                let mut groups = Vec::new();
                if let Some(g) = &f.protection_group {
                    groups.push(g.clone());
                }
                if f.declassified.is_none()
                    && let Some(inherited) = self
                        .inheritance
                        .get(&FieldRef::new(d.name.clone(), f.name.clone()))
                {
                    groups.extend(inherited.iter().cloned());
                }
                groups.sort();
                groups.dedup();

                let tags: Vec<String> = f.classification.clone();

                let mask = f.masking.as_ref().map(|m| {
                    if m.placeholder {
                        "placeholder".to_string()
                    } else if let Some(n) = m.last_chars {
                        format!("last_chars({})", n)
                    } else {
                        "placeholder".to_string()
                    }
                });

                let trace_groups: Vec<String> = d
                    .protection_groups
                    .iter()
                    .filter(|g| g.trace && groups.contains(&g.name))
                    .map(|g| g.name.clone())
                    .collect();

                result.push(FieldProtection {
                    path: format!("{}.{}", d.name, f.name),
                    groups,
                    tags,
                    mask,
                    trace_groups,
                });
            }
        }
        result
    }

    /// The protection groups defined on a DocType.
    pub fn protection_groups(&self, doctype: &str) -> Vec<(String, String, bool)> {
        if let Some(d) = self.definition(doctype) {
            d.protection_groups
                .iter()
                .map(|g| (g.name.clone(), g.label_or_name().to_string(), g.trace))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Declassified fields: (path, reason) for every field that is deliberately declassified.
    pub fn declassifications(&self) -> &[(String, String)] {
        &self.declassifications
    }

    /// What compiling found but did not refuse. A warning never changes the metadata; it marks
    /// a definition that compiles but that its author or a client should know about.
    pub fn report(&self) -> &CompileReport {
        &self.report
    }

    /// The warnings of the compile report, in build order.
    pub fn warnings(&self) -> &[Warning] {
        self.report.warnings()
    }
}

/// Builds the effective metadata, and holds the current one as customisations change.
#[derive(Debug)]
pub struct Registry {
    definitions: Vec<Definition>,
    customisations: Customisations,
    options: BuildOptions,
    current: Arc<EffectiveMeta>,
}

impl Registry {
    /// Merges `definitions` and `customisations` and validates the result. Reports every error
    /// at once. The version is one more than the customisations' revision, so it rises with
    /// every change to them.
    pub fn build(
        definitions: Vec<Definition>,
        customisations: &Customisations,
    ) -> Result<EffectiveMeta, Vec<MetaError>> {
        Self::build_with(definitions, customisations, &BuildOptions::new())
    }

    /// As [`Registry::build`], with the extra inputs of `options`: with `known_seams` set, a
    /// lookup naming a seam outside the set is an error naming the DocType, field and seam
    /// (custom fields and site overrides included).
    pub fn build_with(
        definitions: Vec<Definition>,
        customisations: &Customisations,
        options: &BuildOptions,
    ) -> Result<EffectiveMeta, Vec<MetaError>> {
        let authored = definitions.clone();
        let mut merged = definitions;
        let mut errors = Vec::new();
        // The lint judges what apps author. A site later putting an input into a group is not
        // an authoring error: computed inheritance protects the derived field (rule 5).
        errors.extend(derive::derived_unprotected(&authored, &options.derivations));
        apply_custom_fields(&mut merged, customisations.fields(), &mut errors);
        for o in customisations.overrides() {
            apply_override(&mut merged, o, &mut errors);
        }

        // Validate before alias migration so we can catch conflicts in the TOML
        errors.extend(validate_with_seams(&merged, options.known_seams.as_ref()));
        if !errors.is_empty() {
            return Err(errors);
        }

        // Handle permlevel alias (ADR-068 amendment A1): migrate permlevel >= 1 to level_n group
        handle_permlevel_alias(&mut merged);
        merged.sort_by(|a, b| a.name.cmp(&b.name));
        for d in &mut merged {
            if d.is_posted {
                add_engine_fields(d);
            }
        }
        let inheritance =
            derive::compute_inheritance(&merged, &options.derivations).map_err(|e| vec![e])?;
        let protected = crate::validate::check_search_inheritance(&merged, &inheritance);
        if !protected.is_empty() {
            return Err(protected);
        }
        let version = customisations.revision() + 1;
        let mut views = BTreeMap::new();
        let mut declassifications = Vec::new();
        let mut report = CompileReport::new();

        for d in &merged {
            let (view, warnings) =
                compile::doctype(version, d, &merged, &inheritance).map_err(|e| vec![e])?;
            report.extend(warnings);
            views.insert(d.name.clone(), Arc::new(view));

            // Collect declassified fields
            for f in &d.fields {
                if let Some(reason) = &f.declassified {
                    declassifications.push((format!("{}.{}", d.name, f.name), reason.clone()));
                }
            }
        }

        Ok(EffectiveMeta {
            version,
            definitions: merged,
            views,
            declassifications,
            inheritance,
            report,
        })
    }

    /// A registry over `definitions` with no customisations.
    pub fn new(definitions: Vec<Definition>) -> Result<Self, Vec<MetaError>> {
        Self::with_options(definitions, BuildOptions::new())
    }

    /// As [`Registry::new`], keeping `options` for every later build, so a change a site makes
    /// at runtime is checked against the same known seams.
    pub fn with_options(
        definitions: Vec<Definition>,
        options: BuildOptions,
    ) -> Result<Self, Vec<MetaError>> {
        let customisations = Customisations::new();
        let current = Arc::new(Self::build_with(
            definitions.clone(),
            &customisations,
            &options,
        )?);
        Ok(Self {
            definitions,
            customisations,
            options,
            current,
        })
    }

    /// The current effective metadata.
    pub fn effective(&self) -> Arc<EffectiveMeta> {
        Arc::clone(&self.current)
    }

    /// The customisations in force.
    pub fn customisations(&self) -> &Customisations {
        &self.customisations
    }

    /// Adds a custom field. On error nothing changes. Returns the new metadata version.
    pub fn add_custom_field(
        &mut self,
        doctype: &str,
        spec: FieldSpec,
        insert_after: Option<&str>,
    ) -> Result<u64, Vec<MetaError>> {
        self.change(|c| c.add_field(doctype, spec, insert_after))
    }

    /// Sets a property. On error nothing changes. Returns the new metadata version.
    pub fn set_property(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        property: Property,
    ) -> Result<u64, Vec<MetaError>> {
        self.change(|c| c.set(doctype, field, property))
    }

    /// Removes a custom field. On error nothing changes (removing a field other things refer to
    /// is refused). Returns the new metadata version.
    pub fn remove_custom_field(
        &mut self,
        doctype: &str,
        name: &str,
    ) -> Result<u64, Vec<MetaError>> {
        if !self
            .customisations
            .fields()
            .iter()
            .any(|c| c.doctype == doctype && c.spec.name == name)
        {
            return Err(vec![
                MetaError::new(K::UnknownName, "there is no such custom field")
                    .in_doctype(doctype)
                    .in_field(name),
            ]);
        }
        self.change(|c| {
            c.remove_field(doctype, name);
        })
    }

    /// Replaces the definitions (a standard update). On error nothing changes.
    pub fn set_definitions(&mut self, definitions: Vec<Definition>) -> Result<u64, Vec<MetaError>> {
        let mut customisations = self.customisations.clone();
        customisations.touch();
        let current = Arc::new(Self::build_with(
            definitions.clone(),
            &customisations,
            &self.options,
        )?);
        let version = current.version();
        self.definitions = definitions;
        self.customisations = customisations;
        self.current = current;
        Ok(version)
    }

    fn change(&mut self, edit: impl FnOnce(&mut Customisations)) -> Result<u64, Vec<MetaError>> {
        let mut customisations = self.customisations.clone();
        edit(&mut customisations);
        let current = Arc::new(Self::build_with(
            self.definitions.clone(),
            &customisations,
            &self.options,
        )?);
        let version = current.version();
        self.customisations = customisations;
        self.current = current;
        Ok(version)
    }
}

fn add_engine_fields(d: &mut Definition) {
    let mut state = FieldSpec::new(DOC_STATE, FieldType::Text);
    state.label = Some("Document State".into());
    state.max_chars = Some(16);
    state.read_only = true;
    let mut amended = FieldSpec::new(AMENDED_FROM, FieldType::Link);
    amended.label = Some("Amended From".into());
    amended.options = Some(d.name.clone());
    amended.read_only = true;
    d.fields.push(state);
    d.fields.push(amended);
}

fn apply_custom_fields(
    defs: &mut [Definition],
    custom: &[CustomField],
    errors: &mut Vec<MetaError>,
) {
    for c in custom {
        let err = |kind, msg: String| {
            MetaError::new(kind, msg)
                .in_doctype(&c.doctype)
                .in_field(&c.spec.name)
        };
        let Some(d) = defs.iter_mut().find(|d| d.name == c.doctype) else {
            errors.push(err(
                K::UnknownName,
                "custom field is for an unknown DocType".into(),
            ));
            continue;
        };
        if !c.spec.name.starts_with(CUSTOM_PREFIX) {
            errors.push(err(
                K::BadCustomField,
                format!("custom field names must start with `{CUSTOM_PREFIX}`"),
            ));
            continue;
        }
        let at = match &c.insert_after {
            None => d.fields.len(),
            Some(after) => match d.fields.iter().position(|f| &f.name == after) {
                Some(i) => i + 1,
                None => {
                    errors.push(err(
                        K::UnknownName,
                        format!("`insert_after` names unknown field `{after}`"),
                    ));
                    continue;
                }
            },
        };
        d.fields.insert(at, c.spec.clone());
    }
}

fn apply_override(defs: &mut [Definition], o: &Override, errors: &mut Vec<MetaError>) {
    let mk = |kind, msg: String| {
        let e = MetaError::new(kind, msg).in_doctype(&o.doctype);
        match &o.field {
            Some(f) => e.in_field(f),
            None => e,
        }
    };
    let bad = |msg: String| mk(K::BadOverride, msg);
    let Some(d) = defs.iter_mut().find(|d| d.name == o.doctype) else {
        errors.push(mk(
            K::UnknownName,
            "override is for an unknown DocType".into(),
        ));
        return;
    };
    let key = o.property.key();
    let Some(field) = &o.field else {
        let Some(desc) = doctype_descriptor(key) else {
            errors.push(bad(format!("`{key}` is not a property of a DocType")));
            return;
        };
        if let Some(why) = desc.refusal() {
            errors.push(bad(why));
            return;
        }
        match &o.property {
            Property::Label(l) => d.label = Some(l.clone()),
            Property::TitleField(t) => d.title_field = Some(t.clone()),
            _ => errors.push(bad(format!("`{key}` is not a property of a DocType"))),
        }
        return;
    };
    let Some(f) = d.fields.iter_mut().find(|f| &f.name == field) else {
        errors.push(mk(
            K::UnknownName,
            "override is for an unknown field".into(),
        ));
        return;
    };
    let Some(desc) = descriptor(key) else {
        let why = if doctype_descriptor(key).is_some() {
            format!("`{key}` is a property of a DocType, not a field")
        } else {
            format!("`{key}` is not an overridable property")
        };
        errors.push(bad(why));
        return;
    };
    if let Some(why) = desc.refusal() {
        errors.push(bad(why));
        return;
    }
    if !desc.applies_to.accepts(f.ty, f.format.as_deref()) {
        errors.push(bad(format!(
            "`{key}` does not apply to a {:?} field: it is for {}",
            f.ty,
            desc.applies_to.describe()
        )));
        return;
    }
    let site_owned = f.name.starts_with(CUSTOM_PREFIX);
    let mut new_group: Option<String> = None;
    match &o.property {
        Property::Label(l) => f.label = Some(l.clone()),
        Property::Description(v) => f.description.clone_from(v),
        Property::Hidden(b) => f.hidden = *b,
        Property::InListView(b) => f.in_list_view = *b,
        Property::InStandardFilter(b) => f.in_standard_filter = *b,
        Property::NoCopy(b) => f.no_copy = *b,
        Property::Control(v) => f.control.clone_from(v),
        Property::Permlevel(n) => {
            // Check for conflict: if protection_group is already set, can't set permlevel
            if f.protection_group.is_some() {
                errors.push(bad(
                    "cannot set both permlevel and protection_group (meta.permlevel_conflict)"
                        .into(),
                ));
                return;
            }
            f.permlevel = *n;
        }
        // The whole spec is replaced, never merged; `None` restores the platform default. The
        // merged definition is validated afterwards, so the new spec gets the same checks.
        Property::Lookup(spec) => f.lookup = spec.clone().map(FieldLookup::Spec),
        Property::Required(b) => f.required = *b,
        Property::ReadOnly(b) => f.read_only = *b,
        Property::AllowOnPosted(b) => f.allow_on_posted = *b,
        Property::Default(v) => f.default.clone_from(v),
        Property::DependsOn(v) => f.depends_on.clone_from(v),
        Property::MandatoryDependsOn(v) => f.mandatory_depends_on.clone_from(v),
        Property::ReadOnlyDependsOn(v) => f.read_only_depends_on.clone_from(v),
        Property::FetchFrom(v) => f.fetch_from.clone_from(v),
        Property::Options(new) => {
            if f.ty != FieldType::Select {
                errors.push(bad(
                    "`options` can only be overridden on a select field".into()
                ));
                return;
            }
            let old = f.select_values();
            if let Some(missing) = old.iter().find(|v| !new.contains(v)) {
                errors.push(bad(format!(
                    "options can only be added to, but `{missing}` is missing"
                )));
                return;
            }
            f.options = Some(new.join("\n"));
        }
        Property::MaxChars(new) => match (f.ty, f.max_chars) {
            (FieldType::Text, Some(old)) if *new >= old => f.max_chars = Some(*new),
            (FieldType::Text, Some(old)) => errors.push(bad(format!(
                "`max_chars` can only be raised: {new} is below {old}"
            ))),
            (FieldType::Text, None) => errors.push(bad(
                "the text field has no limit; setting one would lower it".into(),
            )),
            _ => errors.push(bad("`max_chars` is only for text fields".into())),
        },
        Property::TitleField(_) => {
            errors.push(bad(
                "`title_field` is a property of a DocType, not a field".into()
            ));
        }
        Property::ProtectionGroup(g) => {
            // Upward-only: a site may put an unprotected field into a group, never remove or
            // change the group an app declared. A field the site created itself is free.
            match (&f.protection_group, g) {
                (Some(app_group), None) if !site_owned => {
                    errors.push(bad(format!(
                        "cannot remove protection group `{app_group}` (protection declared by an app is part of the standard)"
                    )));
                }
                (Some(app_group), Some(new_group)) if app_group != new_group && !site_owned => {
                    errors.push(bad(format!(
                        "cannot change protection group from `{app_group}` to `{new_group}` (protection declared by an app is part of the standard)"
                    )));
                }
                _ => {
                    f.protection_group.clone_from(g);
                    // A site may define its own groups, named `custom_*`.
                    if let Some(name) = g
                        && name.starts_with("custom_")
                    {
                        new_group = Some(name.clone());
                    }
                }
            }
        }
        Property::Classification(tags) => {
            // Upward-only: a site may add tags, never remove one an app declared.
            let removed: Vec<&String> = if site_owned {
                Vec::new()
            } else {
                f.classification
                    .iter()
                    .filter(|t| !tags.contains(t))
                    .collect()
            };
            if removed.is_empty() {
                f.classification.clone_from(tags);
            } else {
                for old_tag in removed {
                    errors.push(bad(format!(
                        "cannot remove classification tag `{old_tag}` (protection declared by an app is part of the standard)"
                    )));
                }
            }
        }
        Property::Masking(m) => {
            // Upward-only: a site may set a mask where there is none, or replace one with a
            // mask that reveals no more (`placeholder` reveals 0, `last_chars(n)` reveals n).
            let reveals = |m: &MaskSpec| {
                if m.placeholder {
                    0
                } else {
                    m.last_chars.map_or(0, u32::from)
                }
            };
            match (&f.masking, m) {
                (Some(_), None) if !site_owned => errors.push(bad(
                    "cannot remove masking (protection declared by an app is part of the standard)"
                        .into(),
                )),
                (Some(old), Some(new)) if !site_owned && reveals(new) > reveals(old) => {
                    errors.push(bad(format!(
                        "cannot widen masking: the app's mask reveals {} characters, the override {} (protection declared by an app is part of the standard)",
                        reveals(old),
                        reveals(new)
                    )));
                }
                _ => f.masking.clone_from(m),
            }
        }
    }
    if let Some(name) = new_group
        && !d.protection_groups.iter().any(|g| g.name == name)
    {
        d.protection_groups
            .push(crate::definition::ProtectionGroupSpec::new(&name, &name));
    }
}

/// The name of the protection group a `permlevel` of `level` migrates to (ADR-068 A1).
fn permlevel_group_name(level: u8) -> String {
    format!("level_{level}")
}

/// Whether `permlevel` with `group` is exactly the pair the alias migration produces. The
/// migration keeps `permlevel` readable on the effective definition (the compatibility alias),
/// so its own output must stay valid input: validation accepts this pair and refuses any other
/// group beside a `permlevel >= 1` (`meta.permlevel_conflict`). Nothing is cleared, so the
/// default build is unchanged.
pub(crate) fn is_migrated_permlevel(permlevel: u8, group: Option<&str>) -> bool {
    permlevel > 0 && group == Some(permlevel_group_name(permlevel).as_str())
}

/// Handles permlevel alias: migrates permlevel >= 1 to level_n protection groups (ADR-068 A1).
/// Idempotent: a field already in the migrated form is left alone, and an existing `level_N`
/// group is reused, never duplicated.
fn handle_permlevel_alias(defs: &mut [Definition]) {
    use crate::definition::ProtectionGroupSpec;

    for d in defs {
        for f in &mut d.fields {
            let migrated = is_migrated_permlevel(f.permlevel, f.protection_group.as_deref());
            if f.permlevel > 0 && (f.protection_group.is_none() || migrated) {
                let perm_level = f.permlevel;
                let level_n = permlevel_group_name(perm_level);

                f.protection_group = Some(level_n.clone());

                // Create the group if it doesn't exist
                if !d.protection_groups.iter().any(|g| g.name == level_n) {
                    d.protection_groups.push(ProtectionGroupSpec {
                        name: level_n.clone(),
                        label: Some(format!("Level {}", perm_level)),
                        trace: false,
                        sync_class_hint: None, // defaults to everywhere
                    });
                }
            }
        }
    }
}
