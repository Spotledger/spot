#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Seeded scenarios: each file describes authoritative source records and the exact `why`
//! paths they must produce. Every path is asserted exactly, and every step after the subject
//! must carry the evidence that proves it.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use spot_agent_tiers::Tier;
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Property, Registry};
use spot_migration_audit::{
    ArtifactKind, ArtifactRef, MigrationRun, Outcome, PlanRef, PlanSummary, RunId,
};
use spot_permissions::{Model, Right, Rights, Role, RoleGrant, Snapshot};
use spot_seams::{OverrideReport, OverrideRow, OverrideRowKind};
use spot_self_model::adapters::{
    AssignmentRecord, HealthAdapter, HealthSignalRecord, MetadataAdapter, MigrationImpact,
    MigrationsAdapter, PermissionsAdapter, ScriptRecord, ScriptsAdapter, SeamsAdapter,
    SettingRecord, SettingsAdapter,
};
use spot_self_model::{
    AppInstallRecord, Caller, ChangeKind, EvidenceRef, EvidenceSource, MemoryAudit, NodeKind,
    NodeRef, Provenance, QueryOutcome, SelfModel, SelfModelService, SourceAdapter,
};
use spot_ts::ScriptId;

#[derive(Deserialize)]
struct Scenario {
    name: String,
    #[allow(dead_code)]
    description: String,
    epoch: u64,
    caller: CallerSpec,
    #[serde(default)]
    doctypes: Vec<DoctypeSpec>,
    #[serde(default)]
    customisations: Vec<CustomisationSpec>,
    #[serde(default)]
    migrations: Vec<MigrationSpec>,
    #[serde(default)]
    overrides: Vec<OverrideSpec>,
    #[serde(default)]
    grants: Vec<GrantSpec>,
    #[serde(default)]
    assignments: Vec<AssignmentSpec>,
    #[serde(default)]
    scripts: Vec<ScriptSpec>,
    #[serde(default)]
    settings: Vec<SettingSpec>,
    #[serde(default)]
    signals: Vec<SignalSpec>,
    #[serde(default)]
    expect: Vec<ExpectSpec>,
}

#[derive(Deserialize)]
struct CallerSpec {
    id: String,
    tier: Tier,
}

#[derive(Deserialize)]
struct DoctypeSpec {
    name: String,
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    fields: Vec<FieldSpecDef>,
}

#[derive(Deserialize)]
struct FieldSpecDef {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    options: Option<String>,
}

#[derive(Deserialize)]
struct CustomisationSpec {
    doctype: String,
    #[serde(default)]
    field: Option<String>,
    property: String,
    value: String,
    at: i64,
    actor: String,
    record: String,
    #[serde(default)]
    cause_run: Option<String>,
}

#[derive(Deserialize)]
struct MigrationSpec {
    id: String,
    actor: String,
    started: i64,
    #[serde(default)]
    ended: Option<i64>,
    #[serde(default = "verified")]
    outcome: String,
    #[serde(default = "additive")]
    risk: String,
    #[serde(default)]
    impacts: Vec<ImpactSpec>,
    #[serde(default)]
    installs: Vec<InstallSpec>,
}

fn verified() -> String {
    "verified".to_owned()
}

fn additive() -> String {
    "additive".to_owned()
}

#[derive(Deserialize)]
struct ImpactSpec {
    kind: String,
    name: String,
    change: String,
    change_kind: String,
}

#[derive(Deserialize)]
struct InstallSpec {
    app: String,
    version: String,
}

#[derive(Deserialize)]
struct OverrideSpec {
    doctype: String,
    hook_point: String,
    target_step: String,
    app: String,
    kind: String,
    reason: String,
    order: usize,
    at: i64,
    actor: String,
    record: String,
    #[serde(default)]
    cause_run: Option<String>,
}

#[derive(Deserialize)]
struct GrantSpec {
    role: String,
    doctype: String,
    rights: Vec<String>,
}

#[derive(Deserialize)]
struct AssignmentSpec {
    role: String,
    doctype: String,
    #[serde(default)]
    field: Option<String>,
    right: String,
    principal: String,
    at: i64,
    #[serde(default)]
    expires_at: Option<i64>,
    change: String,
    record: String,
}

#[derive(Deserialize)]
struct ScriptSpec {
    app: String,
    name: String,
    version: u64,
    doctype: String,
    #[serde(default)]
    hook: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    at: Option<i64>,
    #[serde(default)]
    installed_by: Option<String>,
    record: String,
}

#[derive(Deserialize)]
struct SettingSpec {
    key: String,
    scope: String,
    at: i64,
    change: String,
    #[serde(default = "updated")]
    change_kind: String,
    record: String,
}

fn updated() -> String {
    "updated".to_owned()
}

#[derive(Deserialize)]
struct SignalSpec {
    name: String,
    status: String,
    target: NodeRefSpec,
    #[serde(default)]
    cause: Option<NodeRefSpec>,
    at: i64,
    detail: String,
    record: String,
}

#[derive(Deserialize)]
struct NodeRefSpec {
    kind: String,
    name: String,
}

impl NodeRefSpec {
    fn to_ref(&self) -> NodeRef {
        NodeRef::new(
            NodeKind::parse(&self.kind).expect("the scenario names a known node kind"),
            self.name.clone(),
        )
    }
}

#[derive(Deserialize)]
struct ExpectSpec {
    subject: NodeRefSpec,
    paths: Vec<Vec<String>>,
}

#[test]
fn seeded_scenarios_answer_why_exactly() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("the scenario directory exists")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 4, "the four seeded scenarios are present");

    for file in files {
        let text = std::fs::read_to_string(&file).expect("the scenario file reads");
        let scenario: Scenario = toml::from_str(&text).expect("the scenario parses");
        run_scenario(&scenario, &file);
    }
}

fn run_scenario(scenario: &Scenario, file: &std::path::Path) {
    let mut customisations = Customisations::new();
    for custom in &scenario.customisations {
        customisations.set(
            &custom.doctype,
            custom.field.as_deref(),
            property(&custom.property, &custom.value),
        );
    }
    let definitions: Vec<Definition> = scenario
        .doctypes
        .iter()
        .map(|doctype| {
            let mut definition = Definition::new(&doctype.name);
            definition.app = doctype.app.clone();
            definition.fields = doctype
                .fields
                .iter()
                .map(|field| {
                    let ty = field_type(&field.ty);
                    let mut spec = FieldSpec::new(&field.name, ty);
                    if ty == FieldType::Decimal {
                        spec.scale = Some(2);
                    }
                    spec.required = field.required;
                    spec.read_only = field.read_only;
                    spec.options = field.options.clone();
                    spec
                })
                .collect();
            definition
        })
        .collect();
    let meta = Registry::build(definitions, &customisations).expect("the scenario metadata builds");

    let mut metadata = MetadataAdapter::new(Arc::new(meta.clone()), customisations.clone());
    for custom in &scenario.customisations {
        let field = custom.field.as_deref().unwrap_or("-");
        let key = format!(
            "override:{}.{}.{}",
            custom.doctype,
            field,
            property(&custom.property, &custom.value).key()
        );
        let mut provenance =
            Provenance::new(custom.actor.clone(), custom.at, custom.record.clone());
        if let Some(run) = &custom.cause_run {
            provenance = provenance.caused_by(NodeRef::new(NodeKind::Migration, run.clone()));
        }
        metadata = metadata.with_provenance(key, provenance);
    }

    let mut migrations = MigrationsAdapter::new();
    for migration in &scenario.migrations {
        migrations = migrations.with_run(migration_run(migration));
        for impact in &migration.impacts {
            migrations = migrations.with_impact(MigrationImpact {
                run: migration.id.clone(),
                target: NodeRef::new(
                    NodeKind::parse(&impact.kind).expect("a known impact kind"),
                    &impact.name,
                ),
                change: impact.change.clone(),
                change_kind: change_kind(&impact.change_kind),
            });
        }
        for install in &migration.installs {
            migrations = migrations.with_install(AppInstallRecord {
                run: migration.id.clone(),
                app: install.app.clone(),
                version: install.version.clone(),
            });
        }
    }

    let mut seams = SeamsAdapter::new(OverrideReport {
        overrides: scenario
            .overrides
            .iter()
            .map(|item| OverrideRow {
                app: item.app.clone(),
                kind: override_kind(&item.kind),
                target_step: item.target_step.clone(),
                doctype: item.doctype.clone(),
                hook_point: item.hook_point.clone(),
                reason: item.reason.clone(),
                effective_order: item.order,
            })
            .collect(),
        seams: Vec::new(),
    });
    for item in &scenario.overrides {
        let key = format!(
            "override:pipeline:{}.{}.{}.{}",
            item.doctype, item.hook_point, item.app, item.target_step
        );
        let mut provenance = Provenance::new(item.actor.clone(), item.at, item.record.clone());
        if let Some(run) = &item.cause_run {
            provenance = provenance.caused_by(NodeRef::new(NodeKind::Migration, run.clone()));
        }
        seams = seams.with_provenance(key, provenance);
    }

    let model = Model {
        grants: scenario
            .grants
            .iter()
            .map(|grant| RoleGrant {
                doctype: grant.doctype.clone(),
                role: Role(grant.role.clone()),
                rights: Rights::new(grant.rights.iter().filter_map(|right| Right::parse(right))),
                if_owner: false,
            })
            .collect(),
        ..Model::default()
    };
    let snapshot = Snapshot::build(&meta, &model).expect("the scenario permission snapshot builds");
    let mut permissions = PermissionsAdapter::new(snapshot);
    for assignment in &scenario.assignments {
        permissions = permissions.with_assignment(AssignmentRecord {
            role: assignment.role.clone(),
            doctype: assignment.doctype.clone(),
            field: assignment.field.clone(),
            right: assignment.right.clone(),
            principal: assignment.principal.clone(),
            at: assignment.at,
            expires_at: assignment.expires_at,
            change: change_kind(&assignment.change),
            evidence: EvidenceRef::at(
                EvidenceSource::PermissionSnapshot,
                assignment.record.clone(),
                assignment.at,
            ),
        });
    }

    let mut scripts = ScriptsAdapter::new();
    for script in &scenario.scripts {
        let mut record = ScriptRecord::new(
            ScriptId::new(&script.app, &script.name, script.version),
            &script.doctype,
        )
        .references(script.fields.clone())
        .evidence(EvidenceRef::at(
            EvidenceSource::ScriptIndex,
            script.record.clone(),
            script.at.unwrap_or(0),
        ));
        if let Some(hook) = &script.hook {
            record = record.hooks_into(hook.clone());
        }
        if let Some(step) = &script.step {
            record = record.as_step(step.clone());
        }
        if let Some(app) = &script.installed_by {
            record = record.installed_by(app.clone());
        }
        if let Some(at) = script.at {
            record = record.at(at);
        }
        scripts = scripts.with_script(record);
    }

    let mut settings = SettingsAdapter::new();
    for setting in &scenario.settings {
        settings = settings.with_setting(SettingRecord {
            key: setting.key.clone(),
            scope: setting.scope.clone(),
            at: setting.at,
            change: setting.change.clone(),
            change_kind: change_kind(&setting.change_kind),
            cause: None,
            evidence: EvidenceRef::at(EvidenceSource::Settings, setting.record.clone(), setting.at),
        });
    }

    let mut health = HealthAdapter::new();
    for signal in &scenario.signals {
        health = health.with_signal(HealthSignalRecord {
            name: signal.name.clone(),
            status: signal.status.clone(),
            target: signal.target.to_ref(),
            cause: signal.cause.as_ref().map(NodeRefSpec::to_ref),
            at: signal.at,
            detail: signal.detail.clone(),
            evidence: EvidenceRef::at(EvidenceSource::Telemetry, signal.record.clone(), signal.at),
        });
    }

    let sources: Vec<&dyn SourceAdapter> = vec![
        &metadata,
        &migrations,
        &seams,
        &permissions,
        &scripts,
        &settings,
        &health,
    ];
    let built = SelfModel::build(scenario.epoch, &sources)
        .unwrap_or_else(|error| panic!("{}: the model builds: {error}", scenario.name));
    let caller = Caller::new(&scenario.caller.id, scenario.caller.tier);
    let queried_at = i64::try_from(scenario.epoch).unwrap_or(i64::MAX);
    let mut audit = MemoryAudit::new();
    let mut service = SelfModelService::new(&built, &mut audit);

    for expect in &scenario.expect {
        let subject = expect.subject.to_ref();
        let explanation = service
            .why(&caller, &subject, queried_at)
            .unwrap_or_else(|error| {
                panic!(
                    "{}: why {} failed: {error} ({file:?})",
                    scenario.name,
                    subject.key()
                )
            });
        let mut got: Vec<Vec<String>> = explanation
            .paths
            .iter()
            .map(|path| {
                path.steps
                    .iter()
                    .map(|step| format!("{}:{}", step.kind.as_str(), step.node.name))
                    .collect()
            })
            .collect();
        let mut want = expect.paths.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want, "{}: exact why paths ({file:?})", scenario.name);
        for path in &explanation.paths {
            assert!(!path.steps.is_empty());
            assert!(!path.steps[0].evidence.is_empty());
            for step in path.steps.iter().skip(1) {
                assert!(
                    step.via.is_some() && !step.evidence.is_empty(),
                    "{}: every cause step has an edge and evidence",
                    scenario.name
                );
            }
        }
    }
    assert_eq!(audit.records().len(), scenario.expect.len());
    assert!(
        audit
            .records()
            .iter()
            .all(|record| record.outcome == QueryOutcome::Allowed)
    );
}

fn property(name: &str, value: &str) -> Property {
    match name {
        "required" => Property::Required(value == "true"),
        "read_only" => Property::ReadOnly(value == "true"),
        "label" => Property::Label(value.to_owned()),
        "options" => Property::Options(
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        other => panic!("scenario property `{other}` is not supported"),
    }
}

fn field_type(name: &str) -> FieldType {
    match name {
        "decimal" => FieldType::Decimal,
        "text" => FieldType::Text,
        "int" => FieldType::Int,
        "bool" => FieldType::Bool,
        "link" => FieldType::Link,
        "select" => FieldType::Select,
        other => panic!("scenario field type `{other}` is not supported"),
    }
}

fn change_kind(name: &str) -> ChangeKind {
    ChangeKind::parse(name).expect("the scenario names a known change kind")
}

fn override_kind(name: &str) -> OverrideRowKind {
    match name {
        "replace" => OverrideRowKind::Replace,
        "wrap" => OverrideRowKind::Wrap,
        "disable" => OverrideRowKind::Disable,
        "reorder" => OverrideRowKind::Reorder,
        other => panic!("scenario override kind `{other}` is not supported"),
    }
}

fn migration_run(spec: &MigrationSpec) -> MigrationRun {
    MigrationRun {
        id: RunId::new(&spec.id).expect("the scenario run id is valid"),
        plan_digest: "scenario-digest".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(
                ArtifactKind::Plan,
                format!("plans/{}.json", spec.id),
                spec.id.as_bytes(),
            ),
            summary: PlanSummary {
                risk: spec.risk.clone(),
                changes: u64::try_from(spec.impacts.len()).unwrap_or(u64::MAX),
                ..PlanSummary::default()
            },
        },
        approvals: Vec::new(),
        rehearsal: None,
        apply_journal_ref: None,
        outcome: Outcome::parse(&spec.outcome).expect("the scenario outcome is known"),
        started: spec.started,
        ended: spec.ended,
        actor: spec.actor.clone(),
    }
}
