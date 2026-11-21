#![allow(clippy::expect_used, clippy::unwrap_used)]

//! One fixture per source adapter: each proves the nodes, edges, history and evidence the
//! adapter contributes to a snapshot.

use std::str::FromStr;
use std::sync::Arc;

use semver::Version;
use spot_bundle::Digest;
use spot_fleet_health::{Finding, Health, RuleId, Subject, TenantId, Timestamp};
use spot_installer::{InstalledApp, SiteState};
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Property, Registry};
use spot_migration_audit::{
    ArtifactKind, ArtifactRef, MigrationRun, Outcome, PlanRef, PlanSummary, RunId,
};
use spot_permissions::{FieldTarget, Model, Right, Rights, Role, RoleGrant, Snapshot};
use spot_release_meta::PatchDecl;
use spot_seams::{OverrideReport, OverrideRow, OverrideRowKind, SeamRow};
use spot_self_model::adapters::{
    AppsAdapter, AssignmentRecord, FleetFinding, HealthAdapter, HealthSignalRecord,
    MetadataAdapter, MigrationImpact, MigrationsAdapter, PatchRecord, PermissionsAdapter,
    ScriptRecord, ScriptsAdapter, SeamsAdapter, SettingRecord, SettingsAdapter,
};
use spot_self_model::model::node_id;
use spot_self_model::{
    AppInstallRecord, ChangeKind, EvidenceRef, EvidenceSource, Node, NodeKind, NodeRef, Provenance,
    SelfModel, SourceAdapter, SourceData,
};
use spot_ts::ScriptId;

fn digest() -> Digest {
    Digest::from_str(&"ab".repeat(32)).expect("a valid digest")
}

fn node<'a>(data: &'a SourceData, kind: NodeKind, name: &str) -> &'a Node {
    data.nodes
        .iter()
        .find(|node| node.kind == kind && node.name == name)
        .unwrap_or_else(|| panic!("{}:{name} is a node", kind.as_str()))
}

fn has_edge(
    data: &SourceData,
    from: NodeRef,
    kind: spot_self_model::EdgeKind,
    to: NodeRef,
) -> bool {
    data.edges
        .iter()
        .any(|edge| edge.from == from.id() && edge.kind == kind && edge.to == to.id())
}

fn metadata() -> (Arc<spot_meta::EffectiveMeta>, Customisations) {
    let mut invoice = Definition::new("sales_invoice");
    invoice.app = Some("selling".to_owned());
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    let mut customer = FieldSpec::new("customer", FieldType::Link);
    customer.options = Some("customer".to_owned());
    invoice.fields = vec![total, customer];
    let mut customer_doctype = Definition::new("customer");
    customer_doctype.app = Some("selling".to_owned());
    customer_doctype.fields = vec![FieldSpec::new("name", FieldType::Text)];

    let mut customisations = Customisations::new();
    customisations.add_field(
        "sales_invoice",
        FieldSpec::new("custom_note", FieldType::Text),
        None,
    );
    customisations.set("sales_invoice", Some("total"), Property::Required(true));
    let meta = Registry::build(vec![invoice, customer_doctype], &customisations)
        .expect("the fixture metadata builds");
    (Arc::new(meta), customisations)
}

#[test]
fn metadata_builds_doctypes_fields_references_and_overrides() {
    let (meta, customisations) = metadata();
    let adapter = MetadataAdapter::new(Arc::clone(&meta), customisations.clone())
        .with_provenance(
            "override:sales_invoice.total.required",
            Provenance::new("user:alice", 100, "promotion:7")
                .caused_by(NodeRef::new(NodeKind::Migration, "upgrade-1")),
        )
        .with_provenance(
            "custom_field:sales_invoice.custom_note",
            Provenance::new("user:alice", 90, "promotion:7"),
        );
    let data = adapter.snapshot().expect("the metadata snapshot builds");

    assert_eq!(
        node(&data, NodeKind::DocType, "sales_invoice")
            .app
            .as_deref(),
        Some("selling")
    );
    let field = node(&data, NodeKind::Field, "sales_invoice.total");
    assert_eq!(field.attribute("type"), Some("decimal"));
    assert_eq!(field.attribute("required"), Some("true"));
    assert_eq!(
        node(&data, NodeKind::Field, "sales_invoice.custom_note").attribute("type"),
        Some("text")
    );

    let invoice = NodeRef::new(NodeKind::DocType, "sales_invoice");
    let total = NodeRef::new(NodeKind::Field, "sales_invoice.total");
    assert!(has_edge(
        &data,
        invoice.clone(),
        spot_self_model::EdgeKind::Defines,
        total.clone()
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Field, "sales_invoice.customer"),
        spot_self_model::EdgeKind::References,
        NodeRef::new(NodeKind::DocType, "customer")
    ));

    let override_node = NodeRef::new(
        NodeKind::Customisation,
        "override:sales_invoice.total.required",
    );
    assert!(has_edge(
        &data,
        override_node.clone(),
        spot_self_model::EdgeKind::Overrides,
        total
    ));
    assert!(has_edge(
        &data,
        override_node,
        spot_self_model::EdgeKind::ChangedBy,
        NodeRef::new(NodeKind::Migration, "upgrade-1")
    ));
    assert!(data.history.iter().any(|entry| {
        entry.node == node_id(NodeKind::Field, "sales_invoice.total")
            && entry.change == ChangeKind::Updated
            && entry.detail.contains("required")
            && entry.evidence.record == "promotion:7"
    }));
    assert_eq!(adapter.epoch(), meta.version());
}

/// One DocType at one version: `fields` decides what its content is.
fn invoice_meta(fields: Vec<FieldSpec>) -> Arc<spot_meta::EffectiveMeta> {
    let mut invoice = Definition::new("sales_invoice");
    invoice.fields = fields;
    Arc::new(
        Registry::build(vec![invoice], &Customisations::new())
            .expect("the fixture metadata builds"),
    )
}

#[test]
fn metadata_evidence_is_content_bound_not_version_bound() {
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    let bare = invoice_meta(vec![total.clone()]);
    let extended = invoice_meta(vec![total, FieldSpec::new("memo", FieldType::Text)]);
    assert_eq!(
        bare.version(),
        extended.version(),
        "two sites can serve different DocTypes at one metadata version"
    );

    let first = MetadataAdapter::new(Arc::clone(&bare), Customisations::new());
    let second = MetadataAdapter::new(Arc::clone(&extended), Customisations::new());
    let first_data = first.snapshot().expect("the metadata snapshot builds");
    let second_data = second.snapshot().expect("the metadata snapshot builds");

    let evidence = |data: &SourceData| {
        node(data, NodeKind::DocType, "sales_invoice")
            .evidence
            .first()
            .expect("the DocType carries evidence")
            .record
            .clone()
    };
    assert!(
        evidence(&first_data).starts_with(&format!("meta:{}:", bare.version())),
        "the id keeps the version, for reading"
    );
    assert_ne!(
        evidence(&first_data),
        evidence(&second_data),
        "different content at one version must not share an evidence id"
    );

    let first_model = SelfModel::build(1, &[&first]).expect("the model builds");
    let second_model = SelfModel::build(1, &[&second]).expect("the model builds");
    assert_ne!(
        first_model, second_model,
        "the two metadata states are different models"
    );

    // Determinism: the same metadata read twice is one model, evidence included.
    let again = MetadataAdapter::new(Arc::clone(&bare), Customisations::new());
    let again_model = SelfModel::build(1, &[&again]).expect("the model builds");
    assert_eq!(first_model, again_model);
    assert_eq!(first_model.source_versions(), again_model.source_versions());
}

#[test]
fn seams_build_steps_seam_replacements_and_promotion_causes() {
    let report = OverrideReport {
        overrides: vec![OverrideRow {
            app: "acme".to_owned(),
            kind: OverrideRowKind::Reorder,
            target_step: "selling:validate".to_owned(),
            doctype: "sales_invoice".to_owned(),
            hook_point: "before_save".to_owned(),
            reason: "recheck totals after tax".to_owned(),
            effective_order: 2,
        }],
        seams: vec![SeamRow {
            name: "tax.calculate".to_owned(),
            version: 1,
            provider_app: "selling".to_owned(),
            replaced_by: Some("acme".to_owned()),
            reason: "local tax rules".to_owned(),
        }],
    };
    let adapter = SeamsAdapter::new(report)
        .with_provenance(
            "override:pipeline:sales_invoice.before_save.acme.selling:validate",
            Provenance::new("user:alice", 200, "promotion:42")
                .caused_by(NodeRef::new(NodeKind::Migration, "upgrade-1")),
        )
        .with_provenance(
            "override:seam:tax.calculate@1",
            Provenance::new("user:alice", 201, "promotion:42"),
        );
    let data = adapter.snapshot().expect("the seams snapshot builds");

    let step = NodeRef::new(
        NodeKind::PipelineStep,
        "sales_invoice.before_save.selling:validate",
    );
    assert_eq!(
        node(
            &data,
            NodeKind::PipelineStep,
            "sales_invoice.before_save.selling:validate"
        )
        .attribute("effective_order"),
        Some("2")
    );
    let custom = NodeRef::new(
        NodeKind::Customisation,
        "override:pipeline:sales_invoice.before_save.acme.selling:validate",
    );
    assert!(has_edge(
        &data,
        custom.clone(),
        spot_self_model::EdgeKind::Overrides,
        step
    ));
    assert!(has_edge(
        &data,
        custom,
        spot_self_model::EdgeKind::ChangedBy,
        NodeRef::new(NodeKind::Migration, "upgrade-1")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "selling"),
        spot_self_model::EdgeKind::Defines,
        NodeRef::new(NodeKind::Seam, "tax.calculate@1")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Customisation, "override:seam:tax.calculate@1"),
        spot_self_model::EdgeKind::Overrides,
        NodeRef::new(NodeKind::Seam, "tax.calculate@1")
    ));
    assert_eq!(adapter.epoch(), 2);
}

#[test]
fn apps_build_installed_state_and_manifest_parts() {
    let mut state = SiteState::new();
    let mut app = InstalledApp::new("acme", Version::new(2, 0, 0), digest());
    app.order = 1;
    app.applied_patches = vec!["acme_0001".to_owned()];
    state.apps.push(app);
    let manifest = spot_manifest::parse(
        r#"
[app]
name = "acme"
version = "2.0.0"
title = "Acme"

[[events]]
doctype = "sales_invoice"
event = "before_save"
step = "acme:validate"

[[schedules]]
name = "nightly"
cron = "0 0 2 * * *"
step = "acme:nightly"

[[seams]]
provide = { name = "acme:rules", version = 1 }

[[patches]]
id = "acme_0001"
file = "patches/acme_0001.toml"
intent = "schema"
"#,
        "acme/spot-app.toml",
    )
    .expect("the manifest parses");
    let data = AppsAdapter::new(state)
        .with_manifests(vec![manifest])
        .snapshot()
        .expect("the apps snapshot builds");

    let installed = node(&data, NodeKind::App, "acme");
    assert_eq!(installed.attribute("version"), Some("2.0.0"));
    assert_eq!(installed.attribute("installed"), Some("true"));
    assert_eq!(installed.attribute("title"), Some("Acme"));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "acme"),
        spot_self_model::EdgeKind::Defines,
        NodeRef::new(
            NodeKind::PipelineStep,
            "sales_invoice.before_save.acme:validate"
        )
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "acme"),
        spot_self_model::EdgeKind::Defines,
        NodeRef::new(NodeKind::Schedule, "acme:nightly")
    ));
    assert_eq!(
        node(&data, NodeKind::Schedule, "acme:nightly").attribute("cron"),
        Some("0 0 2 * * *")
    );
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "acme"),
        spot_self_model::EdgeKind::Defines,
        NodeRef::new(NodeKind::Seam, "acme:rules@1")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "acme"),
        spot_self_model::EdgeKind::Defines,
        NodeRef::new(NodeKind::Patch, "acme_0001")
    ));
}

#[test]
fn migrations_record_runs_impacts_installs_and_declarations() {
    let run = MigrationRun {
        id: RunId::new("upgrade-1").expect("a valid run id"),
        plan_digest: "digest".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/upgrade-1.json", b"plan"),
            summary: PlanSummary {
                risk: "additive".to_owned(),
                changes: 1,
                ..PlanSummary::default()
            },
        },
        approvals: Vec::new(),
        rehearsal: None,
        apply_journal_ref: None,
        outcome: Outcome::Verified,
        started: 100,
        ended: Some(110),
        actor: "user:alice".to_owned(),
    };
    let declaration = PatchDecl::parse(
        "id = \"acme_0001\"\nintent = \"add the acme column\"\nclass = \"additive\"\n\
         doctypes = [\"sales_invoice\"]\n\
         reversible = true\nidempotent = true\n",
    )
    .expect("a valid declaration");
    let data = MigrationsAdapter::new()
        .with_run(run)
        .with_impact(MigrationImpact {
            run: "upgrade-1".to_owned(),
            target: NodeRef::new(NodeKind::Field, "sales_invoice.total"),
            change: "required was set to true".to_owned(),
            change_kind: ChangeKind::Updated,
        })
        .with_install(AppInstallRecord {
            run: "upgrade-1".to_owned(),
            app: "selling".to_owned(),
            version: "2.0.0".to_owned(),
        })
        .with_patch(PatchRecord {
            run: "upgrade-1".to_owned(),
            patch: "acme_0001".to_owned(),
            app: "acme".to_owned(),
            intent: "add the acme column".to_owned(),
        })
        .with_declaration(declaration)
        .snapshot()
        .expect("the migrations snapshot builds");

    let migration = NodeRef::new(NodeKind::Migration, "upgrade-1");
    assert_eq!(
        node(&data, NodeKind::Migration, "upgrade-1").attribute("outcome"),
        Some("verified")
    );
    assert!(has_edge(
        &data,
        migration,
        spot_self_model::EdgeKind::Affects,
        NodeRef::new(NodeKind::Field, "sales_invoice.total")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::App, "selling"),
        spot_self_model::EdgeKind::InstalledBy,
        NodeRef::new(NodeKind::Migration, "upgrade-1")
    ));
    assert_eq!(
        node(&data, NodeKind::App, "selling").attribute("version"),
        Some("2.0.0")
    );
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Migration, "upgrade-1"),
        spot_self_model::EdgeKind::Affects,
        NodeRef::new(NodeKind::Patch, "acme_0001")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Patch, "acme_0001"),
        spot_self_model::EdgeKind::Affects,
        NodeRef::new(NodeKind::DocType, "sales_invoice")
    ));
    assert!(data.history.iter().any(|entry| {
        entry.node == node_id(NodeKind::Field, "sales_invoice.total")
            && entry.change == ChangeKind::Updated
            && entry.at == 110
    }));
}

#[test]
fn permissions_build_grants_and_timed_assignments() {
    let mut invoice = Definition::new("sales_invoice");
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    invoice.fields = vec![total];
    let meta = Registry::build(vec![invoice], &Customisations::new()).expect("the metadata builds");
    let model = Model {
        grants: vec![RoleGrant {
            doctype: "sales_invoice".to_owned(),
            role: Role("Clerk".to_owned()),
            rights: Rights::new([Right::Read]),
            if_owner: false,
        }],
        field_grants: vec![spot_permissions::FieldGrant {
            doctype: "sales_invoice".to_owned(),
            role: Role("Clerk".to_owned()),
            target: FieldTarget::Field("total".to_owned()),
            access: [spot_permissions::FieldRight::Read].into_iter().collect(),
            deny: false,
            condition: None,
        }],
        ..Model::default()
    };
    let snapshot = Snapshot::build(&meta, &model).expect("the permission snapshot builds");
    let data = PermissionsAdapter::new(snapshot)
        .with_assignment(AssignmentRecord {
            role: "Clerk".to_owned(),
            doctype: "sales_invoice".to_owned(),
            field: None,
            right: "read".to_owned(),
            principal: "alice".to_owned(),
            at: 300,
            expires_at: Some(250),
            change: ChangeKind::Expired,
            evidence: EvidenceRef::at(EvidenceSource::PermissionSnapshot, "assignment:1", 300),
        })
        .snapshot()
        .expect("the permissions snapshot builds");

    let permission = NodeRef::new(NodeKind::Permission, "Clerk:sales_invoice:read");
    assert!(has_edge(
        &data,
        permission.clone(),
        spot_self_model::EdgeKind::GrantedBy,
        NodeRef::new(NodeKind::Role, "Clerk")
    ));
    assert!(has_edge(
        &data,
        permission.clone(),
        spot_self_model::EdgeKind::Governs,
        NodeRef::new(NodeKind::DocType, "sales_invoice")
    ));
    let duty = NodeRef::new(NodeKind::Duty, "assignment:alice:Clerk:sales_invoice:read");
    assert!(has_edge(
        &data,
        duty,
        spot_self_model::EdgeKind::Affects,
        permission.clone()
    ));
    assert_eq!(
        node(
            &data,
            NodeKind::Duty,
            "assignment:alice:Clerk:sales_invoice:read"
        )
        .attribute("expires_at"),
        Some("250")
    );
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Permission, "Clerk:sales_invoice.total:read"),
        spot_self_model::EdgeKind::Governs,
        NodeRef::new(NodeKind::Field, "sales_invoice.total")
    ));
    assert!(data.history.iter().any(|entry| {
        entry.node == permission.id() && entry.change == ChangeKind::Expired && entry.at == 300
    }));
}

#[test]
fn scripts_build_references_hooks_and_the_installing_app() {
    let script = ScriptRecord::new(ScriptId::new("acme", "validate_totals", 3), "sales_invoice")
        .hooks_into("before_save")
        .as_step("acme:validate_totals")
        .references(vec![
            "sales_invoice.total".to_owned(),
            "sales_invoice.customer".to_owned(),
        ])
        .installed_by("acme")
        .at(210)
        .evidence(EvidenceRef::at(
            EvidenceSource::ScriptIndex,
            "script:acme:validate_totals@3",
            210,
        ));
    let data = ScriptsAdapter::new()
        .with_script(script)
        .snapshot()
        .expect("the scripts snapshot builds");

    let script_ref = NodeRef::new(NodeKind::Script, "acme:validate_totals");
    let reference = data
        .edges
        .iter()
        .find(|edge| {
            edge.from == script_ref.id()
                && edge.kind == spot_self_model::EdgeKind::References
                && edge.to == node_id(NodeKind::Field, "sales_invoice.total")
        })
        .expect("the script references the field");
    assert_eq!(reference.evidence[0].source, EvidenceSource::DtsCheck);
    assert!(has_edge(
        &data,
        script_ref.clone(),
        spot_self_model::EdgeKind::HooksInto,
        NodeRef::new(
            NodeKind::PipelineStep,
            "sales_invoice.before_save.acme:validate_totals"
        )
    ));
    assert!(has_edge(
        &data,
        script_ref,
        spot_self_model::EdgeKind::ChangedBy,
        NodeRef::new(NodeKind::App, "acme")
    ));
    assert_eq!(
        node(&data, NodeKind::Script, "acme:validate_totals").attribute("version"),
        Some("3")
    );
}

#[test]
fn health_builds_signals_and_fleet_findings() {
    let signal = HealthSignalRecord {
        name: "job failing:nightly".to_owned(),
        status: "failing".to_owned(),
        target: NodeRef::new(NodeKind::Job, "nightly"),
        cause: Some(NodeRef::new(NodeKind::Setting, "job.retry_limit")),
        at: 400,
        detail: "the nightly job is failing".to_owned(),
        evidence: EvidenceRef::at(EvidenceSource::Telemetry, "health:1", 400),
    };
    let finding = Finding {
        subject: Subject::Tenant(TenantId::try_new("acme").expect("a tenant id")),
        rule_id: RuleId::try_new("queue_lag").expect("a rule id"),
        signal: Some(spot_fleet_health::Signal::QueueLag),
        health: Health::Red,
        value: Some(900),
        since: Timestamp::from_secs(100),
        observed_at: Timestamp::from_secs(120),
        sentence: "queue lag is 15 minutes".to_owned(),
        action: "check the workers".to_owned(),
    };
    let data = HealthAdapter::new()
        .with_signal(signal)
        .with_finding(
            FleetFinding::from_finding(&finding)
                .with_cause(NodeRef::new(NodeKind::Setting, "jobs.workers")),
        )
        .snapshot()
        .expect("the health snapshot builds");

    let signal_ref = NodeRef::new(NodeKind::HealthSignal, "job failing:nightly");
    assert!(has_edge(
        &data,
        signal_ref,
        spot_self_model::EdgeKind::ObservedOn,
        NodeRef::new(NodeKind::Job, "nightly")
    ));
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::HealthSignal, "job failing:nightly"),
        spot_self_model::EdgeKind::ChangedBy,
        NodeRef::new(NodeKind::Setting, "job.retry_limit")
    ));

    let finding_ref = NodeRef::new(NodeKind::HealthSignal, "tenant acme:queue_lag");
    let finding_node = node(&data, NodeKind::HealthSignal, "tenant acme:queue_lag");
    assert_eq!(finding_node.attribute("status"), Some("red"));
    assert_eq!(finding_node.attribute("signal"), Some("queue_lag"));
    assert_eq!(finding_node.attribute("observed_at"), Some("120000000"));
    assert!(has_edge(
        &data,
        finding_ref,
        spot_self_model::EdgeKind::ChangedBy,
        NodeRef::new(NodeKind::Setting, "jobs.workers")
    ));
}

#[test]
fn settings_link_their_cause_and_never_carry_a_value() {
    let data = SettingsAdapter::new()
        .with_setting(SettingRecord {
            key: "job.retry_limit".to_owned(),
            scope: "tenant:acme".to_owned(),
            at: 390,
            change: "the retry limit was lowered".to_owned(),
            change_kind: ChangeKind::Updated,
            cause: Some(NodeRef::new(NodeKind::Migration, "upgrade-1")),
            evidence: EvidenceRef::at(EvidenceSource::Settings, "setting:1", 390),
        })
        .snapshot()
        .expect("the settings snapshot builds");

    let setting = node(&data, NodeKind::Setting, "job.retry_limit");
    assert_eq!(setting.attribute("scope"), Some("tenant:acme"));
    assert_eq!(
        setting.attribute("last_change"),
        Some("the retry limit was lowered")
    );
    assert!(has_edge(
        &data,
        NodeRef::new(NodeKind::Migration, "upgrade-1"),
        spot_self_model::EdgeKind::Affects,
        NodeRef::new(NodeKind::Setting, "job.retry_limit")
    ));
    assert_eq!(setting.min_tier, spot_agent_tiers::Tier::T1);
}

#[test]
fn a_source_record_without_evidence_is_refused() {
    let result = HealthAdapter::new()
        .with_signal(HealthSignalRecord {
            name: "signal".to_owned(),
            status: "healthy".to_owned(),
            target: NodeRef::new(NodeKind::Job, "nightly"),
            cause: None,
            at: 1,
            detail: "fine".to_owned(),
            evidence: EvidenceRef::untimed(EvidenceSource::Telemetry, "  "),
        })
        .snapshot();
    assert_eq!(
        result.expect_err("no evidence is refused").code(),
        spot_self_model::codes::EVIDENCE_MISSING
    );
}
