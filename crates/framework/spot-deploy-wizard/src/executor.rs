//! Running a plan: the console signs every step's command, an executor carries it to the
//! node, and the journal records the attempt before and after.
//!
//! The engine never runs anything itself. [`DeployExecutor`] is the seam the console (or a
//! test) implements: it receives a [`SignedCommand`] addressed to one node and returns a
//! [`Receipt`] or an [`ExecutorFailure`]. The agent on the other side verifies the
//! signature, the audience and the freshness window before running the operation
//! (WP-ADM-03), so the wizard only has to build the command and let the console's key sign
//! it — [`run`] takes a [`Signer`] and does exactly that.
//!
//! [`SimulatedExecutor`] is the test double: it records every command and can fail a chosen
//! step once or always, which is how the resume-after-failure tests drive every step index
//! without a fleet.
//!
//! Time is an argument: [`run`] is given a clock to call and the lifetime of the commands it
//! signs, and never reads a wall clock itself. Every command is signed at its own submit
//! instant.

use std::collections::BTreeMap;

use spot_signed_command::{Command, SignedCommand, Signer, sign};

use crate::audit::{AuditEntry, AuditOutcome, AuditSink};
use crate::authz::Authorised;
use crate::error::{
    APPROVAL_MISMATCH, AUDIT_FAILED, AUTHORISATION_EXPIRED, FACTS_STALE, STEP_FAILED, WizardError,
};
use crate::journal::{Journal, StepState, resume, sanitize_detail};
use crate::plan::{DeployPlan, Step, plan_with};
use crate::signed_facts::is_fresh;
use spot_mgmt_ops::Catalogue;

/// Why a step's command did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorFailure {
    /// What went wrong, in words the operator reads. Never a secret.
    pub detail: String,
}

impl ExecutorFailure {
    /// A failure with `detail`.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

/// What an executor reports back for one accepted command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// The signed command's id, for the audit trail.
    pub command_id: String,
    /// A short, secret-free note for the progress list.
    pub detail: String,
}

/// The seam that carries a signed command to a node.
pub trait DeployExecutor {
    /// Submits the command for step `step` and waits for the node's answer.
    ///
    /// # Errors
    /// [`ExecutorFailure`] when the node refused the command or reported a failed step.
    fn submit(&mut self, step: usize, command: &SignedCommand) -> Result<Receipt, ExecutorFailure>;
}

/// The longest a signed command may stay valid, in seconds. A command is signed at the
/// moment it is submitted, so a few minutes is ample; a long lifetime would only make a
/// captured command replayable for that long.
pub const MAX_COMMAND_LIFETIME_SECS: i64 = 300;

/// How long each command a run signs stays valid, counted from its own issue instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandWindow {
    /// How long each command stays valid, in whole seconds.
    pub lifetime_secs: i64,
}

impl CommandWindow {
    /// A window of `lifetime_secs` per command.
    ///
    /// # Errors
    /// [`WizardError::CommandInvalid`] when the lifetime is not positive or is longer than
    /// [`MAX_COMMAND_LIFETIME_SECS`].
    pub fn new(lifetime_secs: i64) -> Result<Self, WizardError> {
        if lifetime_secs <= 0 || lifetime_secs > MAX_COMMAND_LIFETIME_SECS {
            return Err(WizardError::CommandInvalid {
                step: 0,
                title: "the run".to_owned(),
                detail: format!(
                    "a command lifetime must be between 1 and {MAX_COMMAND_LIFETIME_SECS} seconds"
                ),
            });
        }
        Ok(Self { lifetime_secs })
    }

    /// The closing instant (exclusive) of a command issued at `issued_at`.
    #[must_use]
    pub fn expires_at(&self, issued_at: i64) -> i64 {
        issued_at.saturating_add(self.lifetime_secs)
    }
}

/// The step that stopped a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedStep {
    /// The step's index in the plan.
    pub step: usize,
    /// The step's title.
    pub title: String,
    /// The stable code ([`STEP_FAILED`] or [`COMMAND_INVALID`]).
    pub code: &'static str,
    /// What went wrong, in words (cleaned: no control characters, capped).
    pub detail: String,
    /// Whether the step cannot be repeated until its clean-up is carried out and recorded
    /// (a `RolledBack` journal entry): the deployment is waiting for an operator.
    pub cleanup_required: bool,
}

/// What one [`run`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    /// The steps that succeeded during this run, in execution order.
    pub completed: Vec<usize>,
    /// Every step that is done, including steps earlier runs finished.
    pub done: Vec<usize>,
    /// The step that stopped the run, when one did.
    pub failed: Option<FailedStep>,
    /// How many commands this run submitted.
    pub submitted: usize,
    /// How many steps the plan has.
    pub plan_len: usize,
}

impl RunReport {
    /// Whether the whole plan is done.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_none() && self.done.len() == self.plan_len
    }
}

/// Everything a run needs besides the plan and the authority to run it.
pub struct RunEnv<'a> {
    /// The operations the console may sign.
    pub catalogue: &'a Catalogue,
    /// The durable progress journal (the resumable state).
    pub journal: &'a mut dyn Journal,
    /// Carries each signed command to its node.
    pub executor: &'a mut dyn DeployExecutor,
    /// Signs each command.
    pub signer: &'a dyn Signer,
    /// How long each command stays valid.
    pub window: CommandWindow,
    /// The time source; read once per command and once per journal or audit entry.
    pub clock: &'a mut dyn FnMut() -> i64,
    /// The audit port: a start entry before and an end entry after every step.
    pub audit: &'a mut dyn AuditSink,
}

/// Runs a prepared plan from where the journal says, signing each command with the signer.
///
/// Nothing is signed, journaled or submitted until `authorised` proves the run: it must be
/// the token for **this** plan id and plan hash, it must not have lapsed, and every fact the
/// plan was built from must still be within its maximum age at the instant of the run.
///
/// Steps whose dependencies are satisfied are submitted in plan order. Each step writes an
/// audit entry before it starts (if that fails the step does not run) and one when it ends,
/// whatever the outcome. Each command is signed at the instant it is submitted (the clock is
/// read once per command and once per journal entry; the crate never reads a wall clock) and
/// lives for the window, so a deployment of half an hour never leans on long-lived commands.
/// A step's `Started` entry is written before the command goes out, so a crash between the
/// two leaves a resumable record. A failure stops the run and is written to the journal as
/// `Failed`; calling [`run`] again retries an idempotent step.
///
/// A non-idempotent step that was left `Started` or `Failed` may have half-applied its
/// change, so it is never repeated blindly: the run refuses until the console carries out
/// the step's clean-up note and appends a `RolledBack` entry for it.
///
/// Before anything is signed the plan is rebuilt from its topology and defaults and must
/// equal the one handed in, and every operation must be in the catalogue.
///
/// # Errors
/// [`WizardError::Refused`] when the token is for another plan, has lapsed, a fact is stale
/// or the audit entry could not be written; [`WizardError::PlanInvalid`] when the plan is not
/// the one its topology produces or names an operation the catalogue lacks;
/// [`WizardError::JournalMismatch`] when the journal belongs to another plan, is
/// inconsistent or the interrupted step needs clean-up; and [`WizardError::CommandInvalid`]
/// when a step's parameters have no command form.
pub fn run(
    prepared: &crate::Prepared,
    authorised: &Authorised,
    env: &mut RunEnv<'_>,
) -> Result<RunReport, WizardError> {
    let now = (env.clock)();
    if authorised.plan_id() != prepared.plan().id()
        || authorised.plan_hash() != prepared.plan_hash()
    {
        return Err(WizardError::refused(
            APPROVAL_MISMATCH,
            "the authorisation is for a different plan, or the plan changed",
        ));
    }
    if now >= authorised.expires_at() {
        return Err(WizardError::refused(
            AUTHORISATION_EXPIRED,
            "the authorisation has lapsed; authenticate, step up and approve again",
        ));
    }
    if prepared
        .fact_stamps()
        .iter()
        .any(|stamp| !is_fresh(stamp.observed_at, now, stamp.max_age_secs))
    {
        return Err(WizardError::refused(
            FACTS_STALE,
            "the server facts this plan was built from have aged out; collect them again",
        ));
    }
    run_plan(prepared.plan(), authorised.actor(), env)
}

/// Proves `plan` is exactly what its topology and defaults produce and that the catalogue
/// knows every operation in it. A plan can arrive as JSON, so its id and steps are never
/// taken on trust.
fn verify_plan(plan: &DeployPlan, catalogue: &Catalogue) -> Result<(), WizardError> {
    plan.validate()?;
    let rebuilt = plan_with(plan.topology(), plan.defaults())?;
    if &rebuilt != plan {
        return Err(WizardError::PlanInvalid {
            detail: "the plan is not the one its topology produces; refusing to sign it".to_owned(),
        });
    }
    plan.validate_catalogue(catalogue)
}

/// What one step did, before the audit entry for it is written.
enum StepResult {
    Succeeded,
    Failed(String),
}

pub(crate) fn run_plan(
    plan: &DeployPlan,
    actor: &str,
    env: &mut RunEnv<'_>,
) -> Result<RunReport, WizardError> {
    verify_plan(plan, env.catalogue)?;
    let point = resume(plan, env.journal)?;
    if point.cleanup_required {
        let step = point.interrupted.unwrap_or_default();
        return Err(WizardError::JournalMismatch {
            detail: format!(
                "step {step} was interrupted or failed while applying a change it cannot repeat; carry out its clean-up note, record the rollback, and run again"
            ),
        });
    }

    let mut done: std::collections::BTreeSet<usize> = point.completed.iter().copied().collect();
    let mut completed = Vec::new();
    let mut submitted = 0;
    let plan_len = plan.len();
    loop {
        let ready = plan.ready(&done);
        if ready.is_empty() {
            break;
        }
        let mut progressed = false;
        for index in ready {
            let step = plan
                .step(index)
                .ok_or_else(|| WizardError::PlanInvalid {
                    detail: format!("step {index} is missing from the plan"),
                })?
                .clone();
            write_audit(env, plan, &step, actor, AuditOutcome::Started)?;
            let result = execute_step(plan, &step, env, &mut submitted);
            let outcome = match &result {
                Ok(StepResult::Succeeded) => AuditOutcome::Succeeded,
                Ok(StepResult::Failed(_)) => AuditOutcome::Failed,
                Err(_) => AuditOutcome::Errored,
            };
            // The evidence is written whatever happened; if it cannot be, the run stops
            // with the audit error, which outranks the step's own result.
            write_audit(env, plan, &step, actor, outcome)?;
            match result? {
                StepResult::Succeeded => {
                    done.insert(step.index);
                    completed.push(step.index);
                    progressed = true;
                }
                StepResult::Failed(detail) => {
                    return Ok(RunReport {
                        completed,
                        done: done.into_iter().collect(),
                        failed: Some(FailedStep {
                            step: step.index,
                            title: step.title,
                            code: STEP_FAILED,
                            detail,
                            cleanup_required: !step.idempotent,
                        }),
                        submitted,
                        plan_len,
                    });
                }
            }
        }
        if !progressed {
            break;
        }
    }
    Ok(RunReport {
        completed,
        done: done.into_iter().collect(),
        failed: None,
        submitted,
        plan_len,
    })
}

/// Writes one audit entry for `step`; a sink that cannot take it stops the run.
fn write_audit(
    env: &mut RunEnv<'_>,
    plan: &DeployPlan,
    step: &Step,
    actor: &str,
    outcome: AuditOutcome,
) -> Result<(), WizardError> {
    let entry = AuditEntry {
        plan_id: plan.id().as_str().to_owned(),
        step: step.index,
        step_id: step.operation.id.as_str().to_owned(),
        actor: actor.to_owned(),
        outcome,
        at: (env.clock)(),
    };
    env.audit.record(&entry).map_err(|_| {
        WizardError::refused(
            AUDIT_FAILED,
            "the audit entry could not be written, so the run stopped",
        )
    })
}

/// Signs, journals and submits one step.
fn execute_step(
    plan: &DeployPlan,
    step: &Step,
    env: &mut RunEnv<'_>,
    submitted: &mut usize,
) -> Result<StepResult, WizardError> {
    let params = step.operation.params_json()?;
    let audience = step
        .host
        .as_ref()
        .map_or_else(|| "cluster".to_owned(), |host| host.as_str().to_owned());
    let issued_at = (env.clock)();
    let command = Command::new(
        step.operation.id.as_str(),
        params,
        audience,
        env.signer.key_id(),
        issued_at,
        env.window.expires_at(issued_at),
    );
    let signed = sign(&command, env.signer).map_err(|error| WizardError::CommandInvalid {
        step: step.index,
        title: step.title.clone(),
        detail: format!("the console could not sign the command: {error}"),
    })?;
    let at = (env.clock)();
    let attempt = journal_attempt(env.journal, plan.id().clone(), step.index, at)?;
    *submitted += 1;
    match env.executor.submit(step.index, &signed) {
        Ok(receipt) => {
            let at = (env.clock)();
            env.journal.append(
                crate::journal::JournalEntry::new(
                    plan.id().clone(),
                    step.index,
                    attempt,
                    StepState::Succeeded,
                    at,
                )
                .with_detail(receipt.detail),
            )?;
            Ok(StepResult::Succeeded)
        }
        Err(failure) => {
            let detail = sanitize_detail(&failure.detail);
            let at = (env.clock)();
            env.journal.append(
                crate::journal::JournalEntry::new(
                    plan.id().clone(),
                    step.index,
                    attempt,
                    StepState::Failed,
                    at,
                )
                .with_detail(&detail),
            )?;
            Ok(StepResult::Failed(detail))
        }
    }
}

/// Writes the `Started` entry for `step` and returns the attempt number.
fn journal_attempt(
    journal: &mut dyn Journal,
    plan: crate::plan::PlanId,
    step: usize,
    at: i64,
) -> Result<u16, WizardError> {
    let attempt = journal
        .entries()
        .iter()
        .filter(|entry| entry.step == step)
        .map(|entry| entry.attempt)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    journal.append(crate::journal::JournalEntry::new(
        plan,
        step,
        attempt,
        StepState::Started,
        at,
    ))?;
    Ok(attempt)
}

/// One command the simulated executor saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submitted {
    /// The step the command was for.
    pub step: usize,
    /// The command's id.
    pub command_id: String,
    /// The operation the command names.
    pub op: String,
    /// The node the command was addressed to.
    pub audience: String,
    /// How many signatures the command carried.
    pub signatures: usize,
}

/// A test double that records commands and can fail chosen steps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SimulatedExecutor {
    failures: BTreeMap<usize, (String, u8)>,
    submitted: Vec<Submitted>,
}

impl SimulatedExecutor {
    /// An executor that accepts every signed command.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fails `step` the next `times` times it is submitted, with `detail`.
    pub fn fail(&mut self, step: usize, times: u8, detail: &str) {
        self.failures
            .insert(step, (detail.to_owned(), times.max(1)));
    }

    /// Fails `step` once, then accepts it: the shape of a transient refusal a retry fixes.
    pub fn fail_once(&mut self, step: usize, detail: &str) {
        self.fail(step, 1, detail);
    }

    /// The commands submitted so far, in order.
    #[must_use]
    pub fn submitted(&self) -> &[Submitted] {
        &self.submitted
    }
}

impl DeployExecutor for SimulatedExecutor {
    fn submit(&mut self, step: usize, command: &SignedCommand) -> Result<Receipt, ExecutorFailure> {
        if command.signatures.is_empty() {
            return Err(ExecutorFailure::new(
                "the command reached the node unsigned; the console must sign every step",
            ));
        }
        if let Some((detail, remaining)) = self.failures.get_mut(&step)
            && *remaining > 0
        {
            *remaining -= 1;
            return Err(ExecutorFailure::new(detail.clone()));
        }
        self.submitted.push(Submitted {
            step,
            command_id: command.command.id().to_owned(),
            op: command.command.op().to_owned(),
            audience: command.command.audience().to_owned(),
            signatures: command.signatures.len(),
        });
        Ok(Receipt {
            command_id: command.command.id().to_owned(),
            detail: format!("step {step} accepted"),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::journal::MemoryJournal;
    use crate::topology::{NodeSpec, Profile, Role, Topology};
    use spot_signed_command::SoftwareSigner;

    fn node(host: &str, roles: &[Role]) -> NodeSpec {
        NodeSpec::new(host, roles).unwrap()
    }

    fn reference() -> Topology {
        Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
                node("db2.example.com", &[Role::Database]),
            ],
            Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com")
    }

    fn signer() -> SoftwareSigner {
        SoftwareSigner::from_seed(&[7_u8; 32]).unwrap()
    }

    fn window() -> CommandWindow {
        CommandWindow::new(120).unwrap()
    }

    fn catalogue() -> Catalogue {
        use spot_mgmt_ops::{CatalogueBuilder, OperationModule};
        let mut builder = CatalogueBuilder::new();
        crate::catalogue::WizardCatalogue.register(&mut builder);
        spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
        builder.build().unwrap()
    }

    /// A clock that advances 7 seconds per reading.
    fn ticking() -> impl FnMut() -> i64 {
        let mut now = 1_700_000_000;
        move || {
            now += 7;
            now
        }
    }

    fn go(
        plan: &DeployPlan,
        journal: &mut MemoryJournal,
        executor: &mut SimulatedExecutor,
    ) -> Result<RunReport, WizardError> {
        let mut audit = crate::audit::MemoryAudit::new();
        run_plan(
            plan,
            "admin@example.com",
            &mut RunEnv {
                catalogue: &catalogue(),
                journal,
                executor,
                signer: &signer(),
                window: window(),
                clock: &mut ticking(),
                audit: &mut audit,
            },
        )
    }

    #[test]
    fn a_run_submits_every_step_signed() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        let report = go(&plan, &mut journal, &mut executor).unwrap();
        assert!(report.failed.is_none());
        assert!(report.is_complete());
        assert_eq!(report.submitted, plan.len());
        assert_eq!(executor.submitted().len(), plan.len());
        assert!(
            executor
                .submitted()
                .iter()
                .all(|command| command.signatures == 1)
        );
        assert!(
            executor
                .submitted()
                .iter()
                .all(|command| command.op.contains('.'))
        );
        assert_eq!(journal.entries().len(), plan.len() * 2);
    }

    #[test]
    fn a_second_run_has_nothing_to_do() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        go(&plan, &mut journal, &mut executor).unwrap();
        let report = go(&plan, &mut journal, &mut executor).unwrap();
        assert_eq!(report.submitted, 0);
        assert!(report.is_complete());
    }

    #[test]
    fn a_failure_stops_the_run_and_a_retry_finishes_it() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        executor.fail_once(4, "the node refused the install");
        let first = go(&plan, &mut journal, &mut executor).unwrap();
        assert_eq!(first.failed.as_ref().map(|failure| failure.step), Some(4));
        assert_eq!(
            first.failed.as_ref().map(|failure| failure.code),
            Some(STEP_FAILED)
        );
        assert!(!first.done.contains(&4));

        let second = go(&plan, &mut journal, &mut executor).unwrap();
        assert!(second.failed.is_none());
        assert!(second.is_complete());
        assert!(second.done.contains(&4));
    }

    #[test]
    fn an_unsigned_command_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut executor = SimulatedExecutor::new();
        let command = Command::new(
            plan.step(0).unwrap().operation.id.as_str(),
            serde_json::json!({}),
            "app1.example.com",
            signer().key_id(),
            1_700_000_000,
            1_700_000_300,
        );
        let unsigned = SignedCommand {
            command,
            signatures: Vec::new(),
        };
        let failure = executor.submit(0, &unsigned).unwrap_err();
        assert!(failure.detail.contains("unsigned"));
    }

    #[test]
    fn a_negative_window_is_refused() {
        let error = CommandWindow::new(0).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_INVALID);
        let error = CommandWindow::new(MAX_COMMAND_LIFETIME_SECS + 1).unwrap_err();
        assert_eq!(error.code(), crate::error::COMMAND_INVALID);
    }

    #[test]
    fn a_changed_plan_refuses_the_journal() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        go(&plan, &mut journal, &mut executor).unwrap();

        let mut changed = reference();
        changed.web_address = Some("erp2.example.com".to_owned());
        let other = crate::plan::plan(&changed).unwrap();
        let error = go(&other, &mut journal, &mut executor).unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
    }

    /// A recording executor that keeps each command's validity window.
    #[derive(Default)]
    struct Windows(Vec<(i64, i64)>);

    impl DeployExecutor for Windows {
        fn submit(
            &mut self,
            _: usize,
            command: &SignedCommand,
        ) -> Result<Receipt, ExecutorFailure> {
            self.0
                .push((command.command.issued_at(), command.command.expires_at()));
            Ok(Receipt {
                command_id: command.command.id().to_owned(),
                detail: "ok".to_owned(),
            })
        }
    }

    #[test]
    fn every_command_is_issued_at_its_own_time_with_a_short_lifetime() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = Windows::default();
        let mut audit = crate::audit::MemoryAudit::new();
        run_plan(
            &plan,
            "admin@example.com",
            &mut RunEnv {
                catalogue: &catalogue(),
                journal: &mut journal,
                executor: &mut executor,
                signer: &signer(),
                window: window(),
                clock: &mut ticking(),
                audit: &mut audit,
            },
        )
        .unwrap();
        assert_eq!(executor.0.len(), plan.len());
        for pair in executor.0.windows(2) {
            assert!(pair[1].0 > pair[0].0, "issue instants must advance");
        }
        assert!(executor.0.iter().all(|(from, to)| to - from == 120));
        let (first, last) = (executor.0[0], executor.0[executor.0.len() - 1]);
        assert!(
            last.0 > first.1 || plan.len() < 20,
            "later commands are not tied to the first window"
        );
    }

    fn tampered(edit: impl FnOnce(&mut serde_json::Value)) -> DeployPlan {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut json = serde_json::to_value(&plan).unwrap();
        edit(&mut json);
        serde_json::from_value(json).unwrap()
    }

    fn refused(plan: &DeployPlan) {
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        let error = go(plan, &mut journal, &mut executor).unwrap_err();
        assert_eq!(error.code(), crate::error::PLAN_INVALID, "{error}");
        assert!(
            executor.submitted().is_empty(),
            "nothing may be signed or submitted"
        );
        assert!(journal.entries().is_empty());
    }

    #[test]
    fn a_plan_with_an_edited_operation_is_refused() {
        let plan = tampered(|json| {
            json["steps"][1]["operation"]["id"] = serde_json::json!("db.replica.create");
        });
        refused(&plan);
    }

    #[test]
    fn a_plan_with_an_edited_parameter_is_refused() {
        let plan = tampered(|json| {
            let params = json["steps"][0]["operation"]["params"]
                .as_object_mut()
                .unwrap();
            let key = params.keys().next().unwrap().clone();
            params.insert(key, serde_json::json!({"Text": "evil.example.com"}));
        });
        refused(&plan);
    }

    #[test]
    fn a_plan_with_an_edited_id_is_refused() {
        let plan = tampered(|json| {
            json["id"] = serde_json::json!("0000");
        });
        refused(&plan);
    }

    #[test]
    fn an_operation_missing_from_the_catalogue_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut builder = spot_mgmt_ops::CatalogueBuilder::new();
        spot_mgmt_ops::OperationModule::register(&crate::catalogue::WizardCatalogue, &mut builder);
        let narrow = builder.build().unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        let mut audit = crate::audit::MemoryAudit::new();
        let error = run_plan(
            &plan,
            "admin@example.com",
            &mut RunEnv {
                catalogue: &narrow,
                journal: &mut journal,
                executor: &mut executor,
                signer: &signer(),
                window: window(),
                clock: &mut ticking(),
                audit: &mut audit,
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), crate::error::PLAN_INVALID);
        assert!(executor.submitted().is_empty());
    }

    #[test]
    fn a_forged_journal_cannot_skip_the_hardening_step() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let install = plan
            .steps()
            .iter()
            .find(|step| step.kind == crate::plan::StepKind::InstallRuntime)
            .unwrap()
            .index;
        let mut journal = MemoryJournal::new(plan.id().clone());
        journal.started(install, 1).unwrap();
        journal.succeeded(install, 2).unwrap();
        let mut executor = SimulatedExecutor::new();
        let error = go(&plan, &mut journal, &mut executor).unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
        assert!(executor.submitted().is_empty());
    }

    #[test]
    fn a_failed_non_idempotent_step_stops_for_the_operator_until_rolled_back() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let index = plan
            .steps()
            .iter()
            .find(|step| step.kind == crate::plan::StepKind::InitialiseDatabase)
            .unwrap()
            .index;
        assert!(!plan.step(index).unwrap().idempotent);
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        executor.fail_once(index, "init half done");
        let first = go(&plan, &mut journal, &mut executor).unwrap();
        let failed = first.failed.unwrap();
        assert_eq!(failed.step, index);
        assert!(failed.cleanup_required);

        let before = executor.submitted().len();
        let error = go(&plan, &mut journal, &mut executor).unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
        assert_eq!(executor.submitted().len(), before, "no blind retry");

        journal.rolled_back(index, 99).unwrap();
        let resumed = go(&plan, &mut journal, &mut executor).unwrap();
        assert!(resumed.is_complete());
    }

    #[test]
    fn a_failed_idempotent_step_is_retried_at_once() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let index = plan
            .steps()
            .iter()
            .find(|step| step.idempotent)
            .unwrap()
            .index;
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        executor.fail_once(index, "transient");
        let first = go(&plan, &mut journal, &mut executor).unwrap();
        assert!(!first.failed.unwrap().cleanup_required);
        assert!(
            go(&plan, &mut journal, &mut executor)
                .unwrap()
                .is_complete()
        );
    }

    #[test]
    fn executor_failure_text_is_cleaned_before_it_is_kept() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        let noisy = format!("boom\u{1b}[0m\nsecond line {}", "y".repeat(2_000));
        executor.fail_once(0, &noisy);
        let report = go(&plan, &mut journal, &mut executor).unwrap();
        let detail = report.failed.unwrap().detail;
        assert!(detail.chars().all(|ch| !ch.is_control()));
        assert!(detail.chars().count() <= crate::journal::MAX_DETAIL_CHARS);
        let kept = journal.entries().last().unwrap().detail.clone().unwrap();
        assert_eq!(kept, detail);
    }
}
