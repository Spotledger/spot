//! Turning the records into the inputs of a metadata build, and the build into issues.
//!
//! The base definitions (what the installed apps ship) plus the custom DocType records give the
//! definitions; the custom field and property records give the [`Customisations`]. Which records
//! count depends on the [`View`]: the *wanted* view includes `pending` ones (what the change in
//! flight would make true), the *current* view only what is already in the schema.

use spot_meta::{BuildOptions, Customisations, Definition, FieldSpec, Property};
use spot_meta_live::{LiveError, Snapshot, Version};

use crate::error::{Issue, codes};
use crate::records::{RecordState, Records};

/// The module a custom DocType lives in unless a platform administrator picks another.
pub const CUSTOM_MODULE: &str = "custom";

/// Names no custom DocType may take, besides every name an app DocType already uses.
const RESERVED_NAMES: [&str; 6] = [
    "custom_doctype",
    "custom_field",
    "property_setter",
    "doctype",
    "docfield",
    "migrate_progress",
];

/// Prefixes reserved for the platform's own tables.
const RESERVED_PREFIXES: [&str; 4] = ["spot_", "meta_", "pg_", "sql_"];

/// Which records count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    /// `pending` and `applied`, minus anything `removing`: what the site is meant to look like.
    Wanted,
    /// `applied` and `removing`: what the live snapshot holds now.
    Current,
    /// Everything that may have a column: `pending`, `applied`, `removing`, `removed` and, because
    /// a failed apply may have left some of its columns behind, `failed`. The metadata of this
    /// view is never served; its catalogue is what the schema must hold.
    Schema,
    /// As [`View::Schema`] without the `failed` records: the fallback when a failed record no
    /// longer builds (it failed validation, so it never reached the schema).
    SchemaStrict,
}

impl View {
    fn counts(self, state: RecordState) -> bool {
        match self {
            Self::Wanted => matches!(state, RecordState::Pending | RecordState::Applied),
            Self::Current => matches!(state, RecordState::Applied | RecordState::Removing),
            Self::Schema => true,
            Self::SchemaStrict => !matches!(state, RecordState::Failed),
        }
    }
}

/// The inputs of one metadata build, and the problems found assembling them.
#[derive(Debug)]
pub(crate) struct Assembly {
    pub(crate) definitions: Vec<Definition>,
    pub(crate) customisations: Customisations,
    pub(crate) issues: Vec<Issue>,
}

/// Whether `name` may not be taken by a custom DocType.
fn reserved(name: &str) -> bool {
    RESERVED_NAMES.contains(&name) || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Assembles `base` plus the records of `view`.
pub(crate) fn assemble(base: &[Definition], records: &Records, view: View) -> Assembly {
    let mut definitions: Vec<Definition> = base.to_vec();
    let mut issues = Vec::new();

    for record in records.doctypes.iter().filter(|r| view.counts(r.state)) {
        let path = record.name.as_str();
        let mut definition = match Definition::parse(&format!("{path}.toml"), &record.definition) {
            Ok(definition) => definition,
            Err(error) => {
                issues.push(Issue::new(codes::BAD_DEFINITION, path, error.to_string()));
                continue;
            }
        };
        if definition.name != record.name {
            issues.push(Issue::new(
                codes::BAD_DEFINITION,
                path,
                format!(
                    "the definition names `{}` but the record is `{}`",
                    definition.name, record.name
                ),
            ));
            continue;
        }
        if reserved(&record.name) {
            issues.push(Issue::new(
                codes::RESERVED_NAME,
                path,
                format!("`{}` is reserved by the platform", record.name),
            ));
            continue;
        }
        if base.iter().any(|d| d.name == record.name) {
            issues.push(Issue::new(
                codes::NAME_COLLISION,
                path,
                format!("`{}` is already a DocType of an installed app", record.name),
            ));
            continue;
        }
        definition.module = Some(record.module.clone());
        if definition.app.is_none() {
            definition.app = Some(CUSTOM_MODULE.to_owned());
        }
        definitions.push(definition);
    }

    let mut customisations = Customisations::new();
    for record in records.fields.iter().filter(|r| view.counts(r.state)) {
        let path = format!("{}.{}", record.doctype, record.name);
        let spec: FieldSpec = match toml::from_str(&record.spec) {
            Ok(spec) => spec,
            Err(error) => {
                issues.push(Issue::new(
                    codes::BAD_DEFINITION,
                    path,
                    format!(
                        "the field spec is not valid: {}",
                        first_line(&error.to_string())
                    ),
                ));
                continue;
            }
        };
        if spec.name != record.name {
            issues.push(Issue::new(
                codes::BAD_DEFINITION,
                path,
                format!(
                    "the spec names `{}` but the record is `{}`",
                    spec.name, record.name
                ),
            ));
            continue;
        }
        if let Some(target) = definitions.iter().find(|d| d.name == record.doctype)
            && target.fields.iter().any(|f| f.name == record.name)
        {
            issues.push(Issue::new(
                codes::FIELD_COLLISION,
                path,
                format!(
                    "`{}` already has a field named `{}`",
                    record.doctype, record.name
                ),
            ));
            continue;
        }
        customisations.add_field(&record.doctype, spec, record.insert_after.as_deref());
    }
    for record in records.properties.iter().filter(|r| view.counts(r.state)) {
        let path = match &record.field {
            Some(field) => format!("{}.{field}", record.doctype),
            None => record.doctype.clone(),
        };
        match Property::parse(&record.property, &record.value) {
            Ok(property) => customisations.set(&record.doctype, record.field.as_deref(), property),
            Err(error) => issues.push(Issue::new(codes::BAD_DEFINITION, path, error.message)),
        }
    }
    Assembly {
        definitions,
        customisations,
        issues,
    }
}

/// Builds the candidate snapshot. Every metadata error, or every plan error, comes back as an
/// [`Issue`] with a path; the assembly's own issues come back first and stop the build (a
/// skipped definition would only breed follow-up noise).
pub(crate) fn build(
    assembly: Assembly,
    options: &BuildOptions,
    version: Version,
) -> Result<Snapshot, Vec<Issue>> {
    if !assembly.issues.is_empty() {
        return Err(assembly.issues);
    }
    match Snapshot::build(
        assembly.definitions,
        &assembly.customisations,
        options,
        version,
    ) {
        Ok(snapshot) => Ok(snapshot),
        Err(LiveError::Build { meta, plans, .. }) => {
            let mut issues: Vec<Issue> = meta
                .iter()
                .map(|e| {
                    Issue::new(
                        codes::METADATA_INVALID,
                        path_of(e.doctype.as_deref(), e.field.as_deref()),
                        e.message.clone(),
                    )
                })
                .collect();
            issues.extend(plans.iter().map(|e| {
                Issue::new(
                    codes::PLAN_INVALID,
                    String::new(),
                    format!("{} ({})", e, e.code()),
                )
            }));
            Err(issues)
        }
        Err(other) => Err(vec![Issue::new(
            codes::METADATA_INVALID,
            String::new(),
            other.to_string(),
        )]),
    }
}

fn path_of(doctype: Option<&str>, field: Option<&str>) -> String {
    match (doctype, field) {
        (Some(d), Some(f)) => format!("{d}.{f}"),
        (Some(d), None) => d.to_owned(),
        (None, Some(f)) => f.to_owned(),
        (None, None) => String::new(),
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim_end().to_owned()
}

/// The effective definitions in a form `LiveMeta::adopt` can rebuild.
///
/// `spot-meta` migrates a field's `permlevel` to a generated protection group `level_<n>` and
/// keeps the number on the field. Rebuilding those effective definitions fails the registry's own
/// "not both" rule (`meta.permlevel_conflict`), so a node could never adopt a snapshot of a site
/// that uses permlevels. This undoes exactly the migration (the field keeps its `permlevel`,
/// loses the generated group and the group declaration); rebuilding re-applies it and yields the
/// same effective metadata and the same content hash.
pub(crate) fn rebuildable(definitions: &[Definition]) -> Vec<Definition> {
    let mut out = definitions.to_vec();
    for definition in &mut out {
        let mut generated = Vec::new();
        for field in &mut definition.fields {
            let group = format!("level_{}", field.permlevel);
            if field.permlevel >= 1 && field.protection_group.as_deref() == Some(group.as_str()) {
                field.protection_group = None;
                generated.push((group, format!("Level {}", field.permlevel)));
            }
        }
        definition.protection_groups.retain(|g| {
            !generated.iter().any(|(name, label)| {
                &g.name == name
                    && g.label.as_deref() == Some(label.as_str())
                    && !g.trace
                    && g.sync_class_hint.is_none()
            })
        });
    }
    out
}
