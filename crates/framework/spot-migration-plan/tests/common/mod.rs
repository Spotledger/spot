//! Shared builders for the tests: small [`SystemDescription`]s and the twelve release
//! scenarios the golden plans cover.
#![allow(dead_code)]

use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec};
use spot_migration_plan::{
    AppVersion, CapabilitiesSummary, DocTypeDescription, ManifestSummary, MethodDescription,
    PatchDecl, PlannedStep, RenameHint, SchemaPlan, SchemaRefusal, SchemaRunKind, SchemaStep,
    SchemaStepKind, SystemDescription, TableStat, TableStats,
};

/// A DocType with the given fields, no permissions or views.
pub fn dt(name: &str, fields: Vec<FieldSpec>) -> DocTypeDescription {
    let mut definition = Definition::new(name);
    definition.fields = fields;
    DocTypeDescription {
        definition,
        permissions: Vec::new(),
        views: Default::default(),
    }
}

/// A field of a base type.
pub fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

/// A decimal field.
pub fn decimal(name: &str, precision: u8, scale: u8) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Decimal);
    f.precision = Some(precision);
    f.scale = Some(scale);
    f
}

/// A system with the given DocTypes and nothing else.
pub fn system(doctypes: Vec<DocTypeDescription>) -> SystemDescription {
    SystemDescription {
        doctypes,
        ..SystemDescription::new()
    }
}

/// A schema plan with the given steps.
pub fn schema_steps(steps: Vec<SchemaStep>) -> SchemaPlan {
    SchemaPlan {
        steps,
        refusals: Vec::new(),
    }
}

/// A schema step.
pub fn step(key: &str, kind: SchemaStepKind, entity: &str, object: Option<&str>) -> SchemaStep {
    SchemaStep {
        key: key.to_owned(),
        kind,
        entity: entity.to_owned(),
        object: object.map(str::to_owned),
        needs_backfill: false,
        run: SchemaRunKind::Transaction,
        sentence: format!("Do `{key}`."),
    }
}

/// A refused schema change.
pub fn refusal(entity: &str, field: Option<&str>, reason: &str) -> SchemaRefusal {
    SchemaRefusal {
        entity: entity.to_owned(),
        field: field.map(str::to_owned),
        code: "migrate.refused".to_owned(),
        reason: reason.to_owned(),
    }
}

/// The table statistics of a system: every named entity gets the row count.
pub fn stats(rows: &[(&str, u64)]) -> TableStats {
    let mut tables = std::collections::BTreeMap::new();
    for (name, count) in rows {
        tables.insert(
            (*name).to_owned(),
            TableStat {
                rows: Some(*count),
                bytes: None,
            },
        );
    }
    TableStats { tables }
}

/// A `[[patches]]` row.
pub fn patch(id: &str, intent: Option<&str>) -> PatchDecl {
    PatchDecl {
        id: id.to_owned(),
        intent: intent.map(str::to_owned),
        reversible: false,
        idempotent: true,
        deps: Vec::new(),
    }
}

/// One release scenario: the two systems and the schema plan between them.
pub struct Scenario {
    /// The source system.
    pub from: SystemDescription,
    /// The target system.
    pub to: SystemDescription,
    /// The schema diff.
    pub schema: SchemaPlan,
}

/// The names of the scenarios with golden plans.
pub const SCENARIOS: &[&str] = &[
    "new_app",
    "add_field",
    "widen_decimal",
    "narrow_blocked",
    "rename_hint",
    "rename_no_hint",
    "remove_field",
    "make_required",
    "weaken_mask",
    "add_step",
    "breaking_method",
    "widened_capability",
    "downgrade",
];

/// Builds a scenario by name.
pub fn scenario(name: &str) -> Scenario {
    match name {
        "new_app" => new_app(),
        "add_field" => add_field(),
        "widen_decimal" => widen_decimal(),
        "narrow_blocked" => narrow_blocked(),
        "rename_hint" => rename(true),
        "rename_no_hint" => rename(false),
        "remove_field" => remove_field(),
        "make_required" => make_required(),
        "weaken_mask" => weaken_mask(),
        "add_step" => add_step(),
        "breaking_method" => breaking_method(),
        "widened_capability" => widened_capability(),
        "downgrade" => downgrade(),
        other => panic!("unknown scenario {other}"),
    }
}

fn new_app() -> Scenario {
    let mut to = system(vec![dt(
        "sales_invoice",
        vec![
            field("customer", FieldType::Text),
            field("total", FieldType::Int),
        ],
    )]);
    to.apps = vec![AppVersion {
        name: "erp_selling".to_owned(),
        version: "0.2.0".to_owned(),
        manifest: ManifestSummary::default(),
    }];
    Scenario {
        from: SystemDescription::new(),
        to,
        schema: schema_steps(vec![
            step(
                "create_table:sales_invoice",
                SchemaStepKind::AddEntity,
                "sales_invoice",
                None,
            ),
            step(
                "create_index:sales_invoice.idx_customer",
                SchemaStepKind::AddIndex,
                "sales_invoice",
                Some("idx_customer"),
            ),
        ]),
    }
}

fn add_field() -> Scenario {
    let from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let to = system(vec![dt(
        "party",
        vec![
            field("name", FieldType::Text),
            field("note", FieldType::Text),
        ],
    )]);
    Scenario {
        from,
        to,
        schema: schema_steps(vec![step(
            "add_column:party.note",
            SchemaStepKind::AddColumn,
            "party",
            Some("note"),
        )]),
    }
}

fn widen_decimal() -> Scenario {
    let from = system(vec![dt("party", vec![decimal("amount", 18, 2)])]);
    let to = system(vec![dt("party", vec![decimal("amount", 18, 4)])]);
    Scenario {
        from,
        to,
        schema: schema_steps(vec![step(
            "widen_numeric:party.amount",
            SchemaStepKind::WidenType,
            "party",
            Some("amount"),
        )]),
    }
}

fn narrow_blocked() -> Scenario {
    let from = system(vec![dt("party", vec![decimal("amount", 18, 4)])]);
    let to = system(vec![dt("party", vec![decimal("amount", 18, 2)])]);
    let schema = SchemaPlan {
        steps: Vec::new(),
        refusals: vec![refusal(
            "party",
            Some("amount"),
            "`party.amount` would narrow from numeric(18,4) to numeric(18,2); narrowing can lose digits",
        )],
    };
    Scenario { from, to, schema }
}

fn rename(with_hint: bool) -> Scenario {
    let from = system(vec![dt("party", vec![field("old_name", FieldType::Text)])]);
    let mut to = system(vec![dt("party", vec![field("new_name", FieldType::Text)])]);
    if with_hint {
        to.rename_hints = vec![RenameHint {
            doctype: "party".to_owned(),
            from: "old_name".to_owned(),
            to: "new_name".to_owned(),
        }];
    }
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn remove_field() -> Scenario {
    let from = system(vec![dt(
        "party",
        vec![
            field("name", FieldType::Text),
            field("note", FieldType::Text),
        ],
    )]);
    let to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn make_required() -> Scenario {
    let from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let mut required = field("name", FieldType::Text);
    required.required = true;
    let to = system(vec![dt("party", vec![required])]);
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn weaken_mask() -> Scenario {
    let mut from_field = field("secret", FieldType::Text);
    from_field.masking = Some(MaskSpec::last_chars(4));
    let mut to_field = field("secret", FieldType::Text);
    to_field.masking = Some(MaskSpec::last_chars(8));
    let from = system(vec![dt("party", vec![from_field])]);
    let to = system(vec![dt("party", vec![to_field])]);
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn add_step() -> Scenario {
    let from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let mut to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    to.seam_steps = vec![PlannedStep {
        doctype: "party".to_owned(),
        point: "validate_save".to_owned(),
        step: "erp_selling:check_credit_limit".to_owned(),
        priority: 500,
        overridden: None,
    }];
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn breaking_method() -> Scenario {
    let mut from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let mut to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    from.methods = vec![MethodDescription {
        doctype: "party".to_owned(),
        name: "make_payment".to_owned(),
        mutates: true,
        args: vec![spot_migration_plan::ArgSpec {
            name: "amount".to_owned(),
            ty: "int".to_owned(),
            required: true,
        }],
    }];
    to.methods = vec![MethodDescription {
        doctype: "party".to_owned(),
        name: "make_payment".to_owned(),
        mutates: true,
        args: vec![spot_migration_plan::ArgSpec {
            name: "amount".to_owned(),
            ty: "text".to_owned(),
            required: true,
        }],
    }];
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn widened_capability() -> Scenario {
    let mut from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let mut to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    from.apps = vec![AppVersion {
        name: "erp_selling".to_owned(),
        version: "0.2.0".to_owned(),
        manifest: ManifestSummary::default(),
    }];
    let caps = CapabilitiesSummary {
        network: vec!["api.example.com:443".to_owned()],
        email: true,
        ..CapabilitiesSummary::default()
    };
    to.apps = vec![AppVersion {
        name: "erp_selling".to_owned(),
        version: "0.3.0".to_owned(),
        manifest: ManifestSummary {
            capabilities: caps,
            ..ManifestSummary::default()
        },
    }];
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}

fn downgrade() -> Scenario {
    let from = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    let mut to = system(vec![dt("party", vec![field("name", FieldType::Text)])]);
    to.applied_patches = vec!["2026_01_rename_price_list".to_owned()];
    to.apps = vec![AppVersion {
        name: "erp_selling".to_owned(),
        version: "0.2.0".to_owned(),
        manifest: ManifestSummary {
            patches: vec![patch("2026_02_add_note", Some("schema"))],
            ..ManifestSummary::default()
        },
    }];
    Scenario {
        from,
        to,
        schema: SchemaPlan::default(),
    }
}
