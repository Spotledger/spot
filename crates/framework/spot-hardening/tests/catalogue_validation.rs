//! Remedy validation: every remedy in a baseline must name an operation and a parameter set
//! the live catalogue accepts. The compiled-in baselines are checked against a catalogue that
//! contains the operations they use, and a baseline naming an unknown operation is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_hardening::{Baseline, HardeningError};
use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OpId, OperationModule, OperationSpec, ParamSpec,
    ParamType, Risk, TargetKind,
};

/// The operation module the tests register: the four operations the baselines remedy with.
struct RemedyOps;

impl OperationModule for RemedyOps {
    fn name(&self) -> &str {
        "remedy-ops"
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        builder
            .add(
                "remedy-ops",
                spec("os.service.disable", vec![text_required("unit", 128)]),
            )
            .add(
                "remedy-ops",
                spec("os.service.enable", vec![text_required("unit", 128)]),
            )
            .add(
                "remedy-ops",
                spec("os.service.start", vec![text_required("name", 256)]),
            )
            .add(
                "remedy-ops",
                spec(
                    "os.service.set_start_type",
                    vec![
                        text_required("name", 256),
                        enumeration(
                            "start_type",
                            &[
                                "boot",
                                "system",
                                "automatic",
                                "automatic_delayed",
                                "manual",
                                "disabled",
                            ],
                        ),
                    ],
                ),
            );
    }
}

fn spec(id: &str, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: OpId::new(id).unwrap(),
        title: "A remedy operation".to_owned(),
        summary: "An operation the baseline's remedies name.".to_owned(),
        params,
        risk: Risk::Medium,
        target: TargetKind::Server,
    }
}

fn text_required(name: &str, max_len: usize) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: None,
        },
        required: true,
        default: None,
        doc: "A required text parameter.".to_owned(),
        secret: false,
    }
}

fn enumeration(name: &str, choices: &[&str]) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices.iter().map(|choice| (*choice).to_owned()).collect()),
        required: true,
        default: None,
        doc: "A required choice parameter.".to_owned(),
        secret: false,
    }
}

fn catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let modules: Vec<Box<RemedyOps>> = vec![Box::new(RemedyOps)];
    Catalogue::from_modules(&modules)
}

#[test]
fn every_compiled_in_remedy_validates_against_the_catalogue() {
    let catalogue = catalogue().unwrap();
    Baseline::linux()
        .unwrap()
        .validate_against(&catalogue)
        .unwrap();
    Baseline::windows()
        .unwrap()
        .validate_against(&catalogue)
        .unwrap();
}

#[test]
fn a_remedy_naming_an_unknown_operation_is_refused() {
    let text = r#"
os_family = "linux"

[[control]]
id = "a.one"
title = "One"
os_family = "linux"
severity = "low"
reversible = true
[control.check]
kind = "bool"
fact = "a.flag"
expect = true
[control.remedy]
op = "os.nothing.here"
"#;
    let baseline = Baseline::parse(text).unwrap();
    let catalogue = catalogue().unwrap();
    let problems = baseline.validate_against(&catalogue).unwrap_err();
    assert!(
        problems
            .iter()
            .all(|problem| problem.code() == spot_hardening::error::REMEDY_INVALID)
    );
    assert!(matches!(problems[0], HardeningError::RemedyInvalid { .. }));
}

#[test]
fn a_remedy_missing_a_required_parameter_is_refused() {
    let text = r#"
os_family = "linux"

[[control]]
id = "a.one"
title = "One"
os_family = "linux"
severity = "low"
reversible = true
[control.check]
kind = "bool"
fact = "a.flag"
expect = true
[control.remedy]
op = "os.service.disable"
"#;
    let baseline = Baseline::parse(text).unwrap();
    let catalogue = catalogue().unwrap();
    assert!(baseline.validate_against(&catalogue).is_err());
}
