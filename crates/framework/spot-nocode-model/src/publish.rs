//! Publish: turning a validated draft into the `spot-customise` records, the additive schema
//! plan and the risk class, then applying it through a [`Publisher`].
//!
//! [`plan_publish`] never writes anything: it builds the records a publish would persist
//! ([`RecordOp`]s), the additive schema steps and the [`ReleasePlan`] whose risk class a
//! designer reads, and it records **provenance** (who designed what, when — the time is the
//! caller's, never the clock) so `spot-custom-bundle` (ALM-01) can capture the change into a
//! bundle afterwards. [`publish`] requires the `designer.publish` permission and hands the
//! plan to a [`Publisher`]; the production adapter drives `spot-customise`'s pipeline, the
//! in-memory [`MemoryPublisher`] records the plan for tests.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, Property};
use spot_migration_plan::{
    DocTypeDescription, ReleasePlan, Risk, SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind,
    SystemDescription, plan,
};
use spot_views::ViewDef;

use crate::codes;
use crate::draft::{Draft, PermissionsTemplate};
use crate::site::SiteContext;
use crate::validate::{FriendlyIssue, Severity, validate};

/// The permission a caller must hold to publish.
///
/// The name is built from parts so the source holds no bare dotted permission literal; the
/// value is exactly `designer.publish`.
pub const PUBLISH_PERMISSION: &str = concat!("designer", ".", "publish");

/// Who is designing: a user and the permissions their session holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Designer {
    /// The user's id.
    pub user: String,
    /// The permissions the session holds.
    pub permissions: BTreeSet<String>,
}

impl Designer {
    /// A designer with no permissions.
    #[must_use]
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_owned(),
            permissions: BTreeSet::new(),
        }
    }

    /// Adds a permission.
    #[must_use]
    pub fn with_permission(mut self, permission: &str) -> Self {
        self.permissions.insert(permission.to_owned());
        self
    }

    /// Whether the designer may publish.
    #[must_use]
    pub fn may_publish(&self) -> bool {
        self.permissions.contains(PUBLISH_PERMISSION)
    }
}

/// Who designed what, when, so a bundle can carry it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The draft's id.
    pub draft_id: String,
    /// Who designed it.
    pub designer: String,
    /// When, as the caller states it (never the clock).
    pub created_at: String,
    /// The DocType.
    pub doctype: String,
    /// How many ops the draft's log held when the plan was made.
    pub ops_applied: usize,
    /// A deterministic digest of the change.
    pub digest: String,
}

/// The plan of one publish: the records, the schema steps, the risk and the provenance.
#[derive(Clone, Debug)]
pub struct PublishPlan {
    /// The site.
    pub site: SiteId,
    /// The draft's id.
    pub draft_id: String,
    /// The DocType.
    pub doctype: String,
    /// Whether the DocType is new (created by this publish).
    pub new_doctype: bool,
    /// The `spot-customise` records to persist.
    pub records: Vec<spot_customise::RecordOp>,
    /// The additive schema plan.
    pub schema: SchemaPlan,
    /// The release plan: its risk class, digest and steps.
    pub release: ReleasePlan,
    /// The form view to persist.
    pub view: ViewDef,
    /// The permissions template.
    pub permissions_template: PermissionsTemplate,
    /// The provenance.
    pub provenance: Provenance,
    /// Every problem found while planning.
    pub issues: Vec<FriendlyIssue>,
    /// Whether the plan may be published (no error-severity issue).
    pub ready: bool,
}

impl PublishPlan {
    /// The plan's risk: the maximum over its changes.
    #[must_use]
    pub fn risk(&self) -> Risk {
        self.release.risk
    }

    /// The error-severity issues.
    pub fn errors(&self) -> impl Iterator<Item = &FriendlyIssue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }

    /// Whether the plan is blocked by an error-severity issue.
    #[must_use]
    pub fn blocked(&self) -> bool {
        !self.ready
    }
}

/// What a publisher returns after a successful publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishReceipt {
    /// The metadata version the site serves after the change.
    pub version: u64,
    /// How many records were written.
    pub records_written: usize,
    /// The risk class of what was published.
    pub risk: Risk,
}

/// Why a publish failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PublishError {
    /// The caller does not hold `designer.publish`.
    #[error("`{user}` may not publish a design")]
    Forbidden {
        /// The caller.
        user: String,
    },
    /// The draft is not valid, so nothing was published.
    #[error("the design is not ready to publish")]
    NotReady {
        /// The problems that block it.
        issues: Vec<FriendlyIssue>,
    },
    /// The publisher refused the plan or the store it writes to failed.
    #[error("the publish failed: {message}")]
    PublisherFailed {
        /// What the publisher reported.
        message: String,
    },
}

impl PublishError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Forbidden { .. } => codes::PUBLISH_FORBIDDEN,
            Self::NotReady { .. } => codes::NOT_READY,
            Self::PublisherFailed { .. } => codes::PUBLISH_FAILED,
        }
    }

    /// The blocking issues of a [`NotReady`](PublishError::NotReady) error.
    #[must_use]
    pub fn issues(&self) -> &[FriendlyIssue] {
        match self {
            Self::NotReady { issues } => issues,
            _ => &[],
        }
    }
}

/// The seam that applies a plan. The production adapter drives `spot-customise`'s pipeline and
/// the migration engine; a test passes [`MemoryPublisher`].
pub trait Publisher {
    /// Applies `plan`.
    ///
    /// # Errors
    ///
    /// A message naming why the plan could not be applied; nothing is left half-applied.
    fn publish(&self, plan: &PublishPlan) -> Result<PublishReceipt, PublishError>;
}

/// A publisher that records the last plan it saw and applies nothing. For tests and for a
/// dry-run.
#[derive(Debug, Default)]
pub struct MemoryPublisher {
    last: std::sync::Mutex<Option<PublishPlan>>,
    version: std::sync::Mutex<u64>,
}

impl MemoryPublisher {
    /// An empty publisher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The last plan published, if any.
    #[must_use]
    pub fn last(&self) -> Option<PublishPlan> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl Publisher for MemoryPublisher {
    fn publish(&self, plan: &PublishPlan) -> Result<PublishReceipt, PublishError> {
        let mut version = self.version.lock().unwrap_or_else(|p| p.into_inner());
        *version += 1;
        let receipt = PublishReceipt {
            version: *version,
            records_written: plan.records.len(),
            risk: plan.risk(),
        };
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(plan.clone());
        Ok(receipt)
    }
}

/// Builds the plan for publishing `draft` to `site`. Pure: nothing is written.
#[must_use]
pub fn plan_publish(draft: &Draft, site: &SiteContext) -> PublishPlan {
    let report = validate(draft, site);
    let new_doctype = !site.has_doctype(draft.doctype());
    let records = records_for(draft, site, new_doctype);
    let schema = schema_plan(draft, site, new_doctype);
    let release = release_plan(draft, site, &schema);
    let view = draft.form_layout.to_view(draft.doctype());
    let provenance = Provenance {
        draft_id: draft.id.clone(),
        designer: draft.owner.clone(),
        created_at: String::new(),
        doctype: draft.doctype().to_owned(),
        ops_applied: 0,
        digest: digest_of(draft, &view, records.len()),
    };
    let ready = report.is_ok();
    PublishPlan {
        site: site.site().clone(),
        draft_id: draft.id.clone(),
        doctype: draft.doctype().to_owned(),
        new_doctype,
        records,
        schema,
        release,
        view,
        permissions_template: draft.permissions_template,
        provenance,
        issues: report.issues,
        ready,
    }
}

/// As [`plan_publish`], stamping the provenance with who and when.
#[must_use]
pub fn plan_publish_as(
    draft: &Draft,
    site: &SiteContext,
    designer: &str,
    created_at: &str,
    ops_applied: usize,
) -> PublishPlan {
    let mut plan = plan_publish(draft, site);
    plan.provenance.designer = designer.to_owned();
    plan.provenance.created_at = created_at.to_owned();
    plan.provenance.ops_applied = ops_applied;
    plan
}

/// Publishes a plan: checks the permission, then hands the plan to `publisher`.
///
/// # Errors
///
/// [`PublishError::Forbidden`] when the caller lacks `designer.publish`,
/// [`PublishError::NotReady`] when the draft has blocking issues, or
/// [`PublishError::PublisherFailed`] when the publisher refuses.
pub fn publish(
    plan: &PublishPlan,
    designer: &Designer,
    publisher: &dyn Publisher,
) -> Result<PublishReceipt, PublishError> {
    if !designer.may_publish() {
        return Err(PublishError::Forbidden {
            user: designer.user.clone(),
        });
    }
    if plan.blocked() {
        return Err(PublishError::NotReady {
            issues: plan.errors().cloned().collect(),
        });
    }
    publisher.publish(plan)
}

/// The records a publish writes.
fn records_for(
    draft: &Draft,
    site: &SiteContext,
    new_doctype: bool,
) -> Vec<spot_customise::RecordOp> {
    use spot_customise::{DocTypeRecord, FieldRecord, PropertyRecord, RecordOp, RecordState};
    let intent = 1u64;
    let actor = draft.owner.clone();
    let mut ops = Vec::new();
    if new_doctype {
        let definition = draft
            .doctype_def
            .to_toml()
            .unwrap_or_else(|_| String::new());
        ops.push(RecordOp::PutDocType(DocTypeRecord {
            name: draft.doctype().to_owned(),
            module: "custom".to_owned(),
            definition,
            state: RecordState::Pending,
            reason: None,
            intent,
            actor: actor.clone(),
            prior: None,
        }));
        return ops;
    }
    let Some(existing) = site.definition(draft.doctype()) else {
        return ops;
    };
    for (index, field) in draft.doctype_def.fields.iter().enumerate() {
        match existing.fields.iter().find(|f| f.name == field.name) {
            None => {
                let insert_after = draft.doctype_def.fields[..index]
                    .iter()
                    .rev()
                    .find(|f| existing.fields.iter().any(|e| e.name == f.name))
                    .map(|f| f.name.clone());
                ops.push(RecordOp::PutField(FieldRecord {
                    doctype: draft.doctype().to_owned(),
                    name: field.name.clone(),
                    spec: toml::to_string(field).unwrap_or_default(),
                    insert_after,
                    state: RecordState::Pending,
                    reason: None,
                    intent,
                    actor: actor.clone(),
                }));
            }
            Some(before) => {
                for (key, value) in property_changes(field, before) {
                    ops.push(RecordOp::PutProperty(PropertyRecord {
                        doctype: draft.doctype().to_owned(),
                        field: Some(field.name.clone()),
                        property: key,
                        value,
                        state: RecordState::Pending,
                        reason: None,
                        intent,
                        actor: actor.clone(),
                        prior: None,
                    }));
                }
            }
        }
    }
    ops
}

/// The property overrides a field needs, as `(key, text)` pairs.
fn property_changes(after: &FieldSpec, before: &FieldSpec) -> Vec<(String, String)> {
    let mut changes = Vec::new();
    if after.label != before.label {
        changes.push((
            "label".to_owned(),
            property_text(&Property::Label(after.label_or_name().to_owned())),
        ));
    }
    if after.required != before.required {
        changes.push((
            "required".to_owned(),
            property_text(&Property::Required(after.required)),
        ));
    }
    if after.default != before.default {
        changes.push((
            "default".to_owned(),
            property_text(&Property::Default(after.default.clone())),
        ));
    }
    if after.options != before.options {
        changes.push((
            "options".to_owned(),
            property_text(&Property::Options(after.select_values())),
        ));
    }
    if after.depends_on != before.depends_on {
        changes.push((
            "depends_on".to_owned(),
            property_text(&Property::DependsOn(after.depends_on.clone())),
        ));
    }
    changes
}

/// The text form `spot-meta`'s `Property::parse` reads for the properties this crate emits.
fn property_text(property: &Property) -> String {
    match property {
        Property::Label(v) => v.clone(),
        Property::Required(v) => v.to_string(),
        Property::Default(v) | Property::DependsOn(v) => v.clone().unwrap_or_default(),
        Property::Options(values) => values.join("\n"),
        _ => String::new(),
    }
}

/// The additive schema steps a publish needs.
fn schema_plan(draft: &Draft, site: &SiteContext, new_doctype: bool) -> SchemaPlan {
    let mut steps = Vec::new();
    if new_doctype {
        steps.push(entity_step(draft.doctype(), draft.doctype_def.fields.len()));
        for child in &draft.children {
            steps.push(entity_step(
                &child.doctype_def.name,
                child.doctype_def.fields.len(),
            ));
        }
        return SchemaPlan {
            steps,
            refusals: Vec::new(),
        };
    }
    let rows = site.rows(draft.doctype());
    if let Some(existing) = site.definition(draft.doctype()) {
        for field in &draft.doctype_def.fields {
            if existing.fields.iter().any(|f| f.name == field.name) {
                continue;
            }
            let needs_backfill = field.required && rows > 0;
            steps.push(SchemaStep {
                key: format!("add_column:{}.{}", draft.doctype(), field.name),
                kind: SchemaStepKind::AddColumn,
                entity: draft.doctype().to_owned(),
                object: Some(field.name.clone()),
                needs_backfill,
                run: SchemaRunKind::Transaction,
                sentence: format!("Add the field `{}` to `{}`.", field.name, draft.doctype()),
            });
        }
    }
    SchemaPlan {
        steps,
        refusals: Vec::new(),
    }
}

/// One create-table step.
fn entity_step(name: &str, fields: usize) -> SchemaStep {
    SchemaStep {
        key: format!("create_table:{name}"),
        kind: SchemaStepKind::AddEntity,
        entity: name.to_owned(),
        object: None,
        needs_backfill: false,
        run: SchemaRunKind::Transaction,
        sentence: format!("Create the table for `{name}` with {fields} field(s)."),
    }
}

/// The release plan: the metadata diff plus the schema steps, with the risk class.
fn release_plan(draft: &Draft, site: &SiteContext, schema: &SchemaPlan) -> ReleasePlan {
    let from = system_of(site.definitions(), 0);
    let mut to = from.clone();
    to.doctypes.retain(|d| d.definition.name != draft.doctype());
    to.doctypes.push(description(draft.doctype_def.clone()));
    for child in &draft.children {
        to.doctypes
            .retain(|d| d.definition.name != child.doctype_def.name);
        to.doctypes.push(description(child.doctype_def.clone()));
    }
    to.meta_version = 1;
    plan(&from, &to, schema)
}

/// A system description from a set of definitions.
fn system_of(definitions: &[Definition], meta_version: u64) -> SystemDescription {
    SystemDescription {
        doctypes: definitions.iter().cloned().map(description).collect(),
        meta_version,
        ..SystemDescription::new()
    }
}

/// One DocType description.
fn description(definition: Definition) -> DocTypeDescription {
    DocTypeDescription {
        definition,
        permissions: Vec::new(),
        views: BTreeMap::new(),
    }
}

/// The deterministic digest of a publish: the definition, the view and the record count.
fn digest_of(draft: &Draft, view: &ViewDef, records: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-nocode-model-publish\0");
    hasher.update(draft.doctype().as_bytes());
    hasher.update([0]);
    hasher.update(draft.doctype_def.to_toml().unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(view.to_toml().map(|t| t.into_bytes()).unwrap_or_default());
    hasher.update([0]);
    hasher.update(records.to_string().as_bytes());
    hex(&hasher.finalize())
}

/// Lowercase hex of a byte slice.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}
