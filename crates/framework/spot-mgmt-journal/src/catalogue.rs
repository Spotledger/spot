//! The catalogue: the sixteen `journal.*` operations, as data.
//!
//! Every range, enum and pattern lives here, not in the executor: the console builds its
//! screens and the CLI its flags from these specs, and [`crate::dispatch`] only maps the
//! validated result onto typed arguments. All operations target one tenant. No operation
//! takes a path, a URL or a shell fragment, and none has a secret parameter: the rebuild and
//! the snapshot use the site's configured journal only.

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OperationModule, OperationSpec, ParamSpec,
    ParamType, ParamValue, Risk, TargetKind,
};

use crate::args::{OnUnreachable, PolicySetting, RebuildMode, RebuildTarget};
use crate::ids;

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "spot-mgmt-journal";

/// The literal `journal.subject.erase` requires in its `confirm` parameter.
pub const ERASE_CONFIRM: &str = "erase";

/// A grantee or subject: an identifier, never a path or a shell fragment.
const SUBJECT_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._@:-]{0,127}";
/// A plan hash: lower-case hex, 16 to 128 digits.
const HASH_PATTERN: &str = "[0-9a-f]{16,128}";

fn param(
    name: &str,
    ty: ParamType,
    required: bool,
    default: Option<ParamValue>,
    doc: &str,
) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty,
        required,
        default,
        doc: doc.to_owned(),
        secret: false,
    }
}

fn text(name: &str, max_len: usize, pattern: &str, doc: &str) -> ParamSpec {
    param(
        name,
        ParamType::Text {
            max_len,
            pattern: Some(pattern.to_owned()),
        },
        true,
        None,
        doc,
    )
}

fn choice(names: &[&str]) -> ParamType {
    ParamType::Enum(names.iter().map(|n| (*n).to_owned()).collect())
}

fn spec(id: &str, title: &str, summary: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        // Unreachable failure: every id is proven valid by `ids::tests`.
        id: ids::op_id(id).unwrap_or_else(|error| panic!("invalid operation id {id}: {error}")),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Tenant,
    }
}

/// The `target` and `mode` parameters a rebuild plan and a rebuild run share.
fn rebuild_params() -> Vec<ParamSpec> {
    vec![
        text(
            "target",
            64,
            RebuildTarget::PATTERN,
            "Where to stop: `latest`, `at <generation>:<seq>` or `before <rfc3339>`.",
        ),
        param(
            "mode",
            choice(&RebuildMode::NAMES),
            true,
            None,
            "full, roll-forward or point-in-time.",
        ),
    ]
}

/// Every operation this crate registers, in a stable order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    let mut run_params = rebuild_params();
    run_params.push(text(
        "confirm_plan_hash",
        128,
        HASH_PATTERN,
        "The hash of the plan you have seen (from the rebuild plan); a run executes only that plan.",
    ));
    vec![
        spec(
            ids::STATUS,
            "Journal status",
            "Read the journal's head, what is off site, how many segments are unshipped and its health.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::RECOVERY_REPORT,
            "Recovery report",
            "Read the recovery report: target, policy, last verified position, oldest snapshot and key backup state.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::SNAPSHOT_LIST,
            "List snapshots",
            "List the journal's snapshots, newest first.",
            Risk::Read,
            vec![param(
                "limit",
                ParamType::Int { min: 1, max: 200 },
                false,
                Some(ParamValue::Int(20)),
                "How many snapshots to list.",
            )],
        ),
        spec(
            ids::SNAPSHOT_TAKE,
            "Take a snapshot",
            "Take a snapshot of the tenant now and record it in the manifest.",
            Risk::Medium,
            Vec::new(),
        ),
        spec(
            ids::VERIFY,
            "Verify the journal",
            "Check the journal's hash chain, signatures and segments against the manifest.",
            Risk::Low,
            Vec::new(),
        ),
        spec(
            ids::AUDIT_LAST,
            "Last audit result",
            "Read the result of the last journal audit.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::AUDIT_RUN,
            "Run the journal audit",
            "Rebuild the latest snapshot plus the journal in a scratch database and compare it with the live data.",
            Risk::Medium,
            Vec::new(),
        ),
        spec(
            ids::POLICY_GET,
            "Read the durability policy",
            "Read the tenant's durability policy.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::POLICY_SET,
            "Set the durability policy",
            "Change the tenant's durability policy; on_unreachable applies to remote-sync only.",
            Risk::High,
            vec![
                param(
                    "policy",
                    choice(&PolicySetting::NAMES),
                    true,
                    None,
                    "local, replica-sync or remote-sync.",
                ),
                param(
                    "on_unreachable",
                    choice(&[OnUnreachable::Degrade.name(), OnUnreachable::Halt.name()]),
                    false,
                    Some(ParamValue::Text(OnUnreachable::Degrade.name().to_owned())),
                    "What remote-sync does when the object store cannot be reached.",
                ),
            ],
        ),
        spec(
            ids::RESHIP,
            "Re-ship segments",
            "Queue every sealed segment that is not yet off site for shipping.",
            Risk::Medium,
            Vec::new(),
        ),
        spec(
            ids::REBUILD_PLAN,
            "Plan a rebuild",
            "Show what a rebuild would read and apply, and the hash a run must confirm. Changes nothing.",
            Risk::Read,
            rebuild_params(),
        ),
        spec(
            ids::REBUILD_RUN,
            "Run a rebuild",
            "Rebuild the tenant from the journal. Executes only the plan whose hash you confirm.",
            Risk::Critical,
            run_params,
        ),
        spec(
            ids::READER_LIST,
            "List journal readers",
            "List who may read the journal and when each grant expires.",
            Risk::Read,
            Vec::new(),
        ),
        spec(
            ids::READER_GRANT,
            "Grant journal read access",
            "Grant an auditor read access to the journal for a number of days.",
            Risk::High,
            vec![
                text(
                    "grantee",
                    128,
                    SUBJECT_PATTERN,
                    "Who is granted read access.",
                ),
                param(
                    "days",
                    ParamType::Int { min: 1, max: 365 },
                    true,
                    None,
                    "How many days the grant lasts (1 to 365).",
                ),
                param(
                    "external",
                    ParamType::Bool,
                    false,
                    Some(ParamValue::Bool(false)),
                    "Whether the grantee is outside the tenant.",
                ),
            ],
        ),
        spec(
            ids::READER_REVOKE,
            "Revoke journal read access",
            "Revoke a reader's access at once.",
            Risk::High,
            vec![text(
                "grantee",
                128,
                SUBJECT_PATTERN,
                "Whose access is revoked.",
            )],
        ),
        spec(
            ids::SUBJECT_ERASE,
            "Erase a data subject",
            "Destroy the subject's per-subject key so their personal fields read as a placeholder. Cannot be undone.",
            Risk::Critical,
            vec![
                text(
                    "subject",
                    128,
                    SUBJECT_PATTERN,
                    "The data subject to erase.",
                ),
                text(
                    "confirm",
                    16,
                    ERASE_CONFIRM,
                    "Must be the word `erase`; this is irreversible.",
                ),
            ],
        ),
    ]
}

/// The module that registers the journal's operations on the catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct JournalModule;

impl OperationModule for JournalModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for operation in specs() {
            builder.add(MODULE_NAME, operation);
        }
    }
}

/// Builds the catalogue from just the journal module (the host merges it with the others').
///
/// # Errors
/// Every catalogue problem the specs have; a test asserts there are none.
pub fn build_catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let mut builder = CatalogueBuilder::new();
    JournalModule.register(&mut builder);
    builder.build()
}
