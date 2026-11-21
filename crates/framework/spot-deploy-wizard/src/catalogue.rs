//! The operations the wizard adds to the management catalogue.
//!
//! Every plan step is one catalogued operation (WP-ADM-01.T1), so the console can show the
//! equivalent command for it and the agent executes only operations it resolves in its
//! catalogue. The runtime already owns `node.role.set`, `runtime.profile.apply`,
//! `runtime.service.start`/`.stop` and `db.replica.create`; this module registers the six
//! operations the wizard itself introduces — enrol, harden, install, initialise the
//! database, join the cluster and verify health — with the exact parameter names the plan
//! sets, so [`crate::plan::DeployPlan::validate_catalogue`] proves the two cannot drift.

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OperationModule, OperationSpec, ParamSpec,
    ParamType, ParamValue, Risk, TargetKind,
};

use crate::ids::{
    DEPLOY_AGENT_ENROL, DEPLOY_CLUSTER_JOIN, DEPLOY_DATABASE_INIT, DEPLOY_HARDENING_APPLY,
    DEPLOY_HEALTH_VERIFY, DEPLOY_RUNTIME_INSTALL, POSTGRES_PORT,
};

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "spot-deploy-wizard";

/// A host parameter's whole-value pattern, the same shape the runtime's operations accept.
const HOST_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9.:-]{0,253}";
/// A data directory's whole-value pattern: absolute, safe characters.
const DATA_DIR_PATTERN: &str = "/[A-Za-z0-9._/-]{0,255}";

/// A required text parameter.
fn text_required(name: &str, max_len: usize, pattern: Option<&str>, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: pattern.map(str::to_owned),
        },
        required: true,
        default: None,
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An optional boolean parameter with a default.
fn boolean(name: &str, default: bool, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Bool,
        required: false,
        default: Some(ParamValue::Bool(default)),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An optional bounded integer parameter with a default.
fn integer(name: &str, min: i64, max: i64, default: i64, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Int { min, max },
        required: false,
        default: Some(ParamValue::Int(default)),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// An enum parameter with a default.
fn enum_with_default(name: &str, choices: &[&str], default: &str, doc: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Enum(choices.iter().map(|choice| (*choice).to_owned()).collect()),
        required: false,
        default: Some(ParamValue::Text(default.to_owned())),
        doc: doc.to_owned(),
        secret: false,
    }
}

/// One operation spec, targeting a single server.
fn spec(id: &str, title: &str, summary: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: op_id(id),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Server,
    }
}

/// The operation id, unchecked: the constants are proven valid by [`crate::ids`]'s tests.
fn op_id(text: &str) -> spot_mgmt_ops::OpId {
    match crate::ids::op_id(text) {
        Ok(id) => id,
        // Unreachable: every constant is validated by `ids::tests::every_id_is_valid`.
        Err(error) => panic!("invalid operation id {text}: {error}"),
    }
}

/// The operations this module registers, in id order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    vec![
        spec(
            DEPLOY_AGENT_ENROL,
            "Enrol a server's agent",
            "Exchange the one-time enrolment token and pin the server's identity before anything else runs.",
            Risk::High,
            vec![text_required(
                "host",
                255,
                Some(HOST_PATTERN),
                "The server being enrolled.",
            )],
        ),
        spec(
            DEPLOY_HARDENING_APPLY,
            "Apply the hardening baseline",
            "Apply the hardening baseline for the server's operating system and report every control.",
            Risk::High,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The server to harden."),
                enum_with_default(
                    "baseline",
                    &["auto", "linux", "windows"],
                    "auto",
                    "The baseline to apply; auto follows the server's operating system.",
                ),
                enum_with_default(
                    "mode",
                    &["enforce", "audit"],
                    "enforce",
                    "Whether to apply the baseline or only report deviations.",
                ),
            ],
        ),
        spec(
            DEPLOY_RUNTIME_INSTALL,
            "Install the runtime bundle",
            "Unpack the signed runtime bundle and its services on the server.",
            Risk::High,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The server to install on."),
                text_required(
                    "bundle",
                    128,
                    None,
                    "The signed bundle to install, for example spot:runtime.",
                ),
            ],
        ),
        spec(
            DEPLOY_DATABASE_INIT,
            "Create the primary database",
            "Initialise the PostgreSQL cluster and its Spotledger schema on the primary.",
            Risk::High,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The primary server."),
                integer(
                    "port",
                    1,
                    65_535,
                    i64::from(POSTGRES_PORT),
                    "The port PostgreSQL listens on.",
                ),
                text_required(
                    "data_dir",
                    256,
                    Some(DATA_DIR_PATTERN),
                    "The absolute data directory to initialise.",
                ),
            ],
        ),
        spec(
            DEPLOY_CLUSTER_JOIN,
            "Join the cluster",
            "Register the server with the cluster the primary database serves and adopt its cluster id.",
            Risk::Medium,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The server joining."),
                text_required(
                    "primary",
                    255,
                    Some(HOST_PATTERN),
                    "The cluster's primary database server.",
                ),
            ],
        ),
        spec(
            DEPLOY_HEALTH_VERIFY,
            "Verify a server's health",
            "Read the server's health endpoints and prove the roles it claims are serving.",
            Risk::Read,
            vec![
                text_required("host", 255, Some(HOST_PATTERN), "The server to check."),
                boolean(
                    "deep",
                    false,
                    "Whether to run the deep checks as well as the liveness probe.",
                ),
            ],
        ),
    ]
}

/// The module that registers the wizard's operations on the catalogue.
#[derive(Clone, Copy, Debug, Default)]
pub struct WizardCatalogue;

impl OperationModule for WizardCatalogue {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for spec in specs() {
            builder.add(MODULE_NAME, spec);
        }
    }
}

/// Builds the wizard's catalogue alone (the host merges it with the runtime's and the OS
/// modules').
///
/// # Errors
/// The build errors the catalogue reports (a duplicate id, an invalid parameter); proven
/// absent by the tests.
pub fn build_catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let mut builder = CatalogueBuilder::new();
    WizardCatalogue.register(&mut builder);
    builder.build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_mgmt_ops::{Invocation, OperationModule};

    #[test]
    fn the_catalogue_builds() {
        let catalogue = build_catalogue().unwrap();
        assert_eq!(catalogue.len(), 6);
        for spec in specs() {
            assert!(catalogue.get(&spec.id).is_some());
        }
    }

    #[test]
    fn the_plan_satisfies_the_catalogue() {
        use crate::topology::{NodeSpec, Profile, Role, Topology};
        let node = |host: &str, roles: &[Role]| NodeSpec::new(host, roles).unwrap();
        let topology = Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
            ],
            Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com");
        let plan = crate::plan::plan(&topology).unwrap();
        let mut builder = CatalogueBuilder::new();
        WizardCatalogue.register(&mut builder);
        spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
        let catalogue = builder.build().unwrap();
        plan.validate_catalogue(&catalogue).unwrap();
        for step in plan.steps() {
            let mut invocation = Invocation::new(step.operation.id.clone());
            for (name, value) in &step.operation.params {
                invocation = invocation.with(name.clone(), value.clone());
            }
            catalogue
                .validate(&invocation)
                .unwrap_or_else(|errors| panic!("step {} is invalid: {errors:?}", step.index));
        }
    }

    #[test]
    fn every_operation_targets_one_server() {
        for spec in specs() {
            assert_eq!(spec.target, TargetKind::Server, "{}", spec.id.as_str());
            assert!(!spec.summary.is_empty());
            assert!(!spec.title.is_empty());
        }
    }

    #[test]
    fn the_read_only_operation_is_verification() {
        let catalogue = build_catalogue().unwrap();
        let verify = spot_mgmt_ops::OpId::new(crate::ids::DEPLOY_HEALTH_VERIFY).unwrap();
        assert_eq!(catalogue.get(&verify).unwrap().risk, Risk::Read);
        for spec in specs() {
            if spec.id != verify {
                assert!(spec.risk >= Risk::Low, "{}", spec.id.as_str());
            }
        }
    }
}
