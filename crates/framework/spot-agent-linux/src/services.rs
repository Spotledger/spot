//! systemd services: status, start, stop, restart, enable, disable, mask and the failed-unit
//! list, all through `systemctl` with `--no-pager`.
//!
//! The unit's state is read with `systemctl show` (`ActiveState`, `SubState`, `UnitFileState`)
//! so a plan can decide whether anything would change. Stop, restart, disable and mask refuse
//! to touch the agent's own unit or the runtime's without the operation's `critical` flag: a
//! compromised or mistaken console must not be able to take the agent offline by accident.

use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};

use crate::args::Args;
use crate::command::{CommandRequest, CommandRunner};
use crate::context::LinuxContext;
use crate::error::LinuxError;
use crate::ids::{
    self, SERVICE_DISABLE, SERVICE_ENABLE, SERVICE_FAILED, SERVICE_MASK, SERVICE_RESTART,
    SERVICE_START, SERVICE_STATUS, SERVICE_STOP,
};
use crate::preview::{Check, LinuxOperation, Outcome, Preview, Step, first_line, run_steps};

/// The `systemctl show` properties the module reads.
const SHOW_PROPERTIES: &str = "--property=ActiveState,SubState,UnitFileState";

/// What a `systemctl show` reported about a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitState {
    /// Whether the unit is active (`active`, `activating` or `reloading`).
    pub active: bool,
    /// Whether the unit is enabled at boot.
    pub enabled: bool,
    /// Whether the unit is masked.
    pub masked: bool,
    /// The unit's sub-state, as reported.
    pub sub: String,
}

/// Reads a unit's state.
fn read_unit(runner: &dyn CommandRunner, unit: &str) -> Result<UnitState, LinuxError> {
    let request = CommandRequest::new("systemctl")
        .args(["--no-pager", "show", SHOW_PROPERTIES])
        .arg(unit);
    let output = runner.run(&request)?;
    if !output.success() {
        return Err(LinuxError::CommandFailed {
            program: "systemctl".to_owned(),
            status: output.status,
            detail: first_line(&output.stderr),
        });
    }
    let mut state = UnitState {
        active: false,
        enabled: false,
        masked: false,
        sub: String::new(),
    };
    for line in output.stdout_lines() {
        if let Some(value) = line.strip_prefix("ActiveState=") {
            state.active = matches!(value, "active" | "activating" | "reloading");
        } else if let Some(value) = line.strip_prefix("SubState=") {
            state.sub = value.to_owned();
        } else if let Some(value) = line.strip_prefix("UnitFileState=") {
            state.enabled = value == "enabled";
            state.masked = value == "masked";
        }
    }
    Ok(state)
}

/// What a service operation does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceEffect {
    /// Start the unit.
    Start,
    /// Stop the unit.
    Stop,
    /// Restart the unit.
    Restart,
    /// Enable the unit at boot.
    Enable,
    /// Disable the unit at boot.
    Disable,
    /// Mask the unit.
    Mask,
}

impl ServiceEffect {
    /// The `systemctl` verb.
    #[must_use]
    pub fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Mask => "mask",
        }
    }

    /// The operation id.
    #[must_use]
    pub fn op(self) -> &'static str {
        match self {
            Self::Start => SERVICE_START,
            Self::Stop => SERVICE_STOP,
            Self::Restart => SERVICE_RESTART,
            Self::Enable => SERVICE_ENABLE,
            Self::Disable => SERVICE_DISABLE,
            Self::Mask => SERVICE_MASK,
        }
    }

    /// Whether the effect needs the `critical` flag to touch a protected unit.
    #[must_use]
    pub fn is_guarded(self) -> bool {
        matches!(
            self,
            Self::Stop | Self::Restart | Self::Disable | Self::Mask
        )
    }

    /// Whether the unit already satisfies the effect.
    #[must_use]
    pub fn satisfied(self, state: &UnitState) -> bool {
        match self {
            Self::Start | Self::Restart => state.active,
            Self::Stop => !state.active,
            Self::Enable => state.enabled,
            Self::Disable => !state.enabled,
            Self::Mask => state.masked,
        }
    }

    /// What the effect does, in one sentence.
    #[must_use]
    pub fn description(self, unit: &str) -> String {
        match self {
            Self::Start => format!("start {unit}"),
            Self::Stop => format!("stop {unit}"),
            Self::Restart => format!("restart {unit}"),
            Self::Enable => format!("enable {unit} at boot"),
            Self::Disable => format!("disable {unit} at boot"),
            Self::Mask => format!("mask {unit}"),
        }
    }
}

/// One service-control operation (start, stop, restart, enable, disable or mask).
pub struct ServiceControl {
    context: Arc<LinuxContext>,
    op: OpId,
    effect: ServiceEffect,
}

impl ServiceControl {
    /// A control module for `effect`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>, effect: ServiceEffect) -> Self {
        Self {
            context,
            op: ids::op_id(effect.op()),
            effect,
        }
    }

    /// Whether the guard blocks this call: a protected unit without the critical flag.
    fn blocked(&self, unit: &str, critical: bool) -> bool {
        self.effect.is_guarded() && self.context.is_protected_unit(unit) && !critical
    }
}

impl LinuxOperation for ServiceControl {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let parsed = Args::new(args);
        let unit = parsed.unit_name("unit")?;
        let critical = parsed.flag("critical")?;
        let state = read_unit(&*self.context.runner, unit)?;
        if self.effect.satisfied(&state) {
            return Ok(Preview::new(
                self.op.clone(),
                format!("{unit} is already in the desired state"),
                Vec::new(),
                Vec::new(),
            ));
        }
        let step = Step::new(
            self.effect.description(unit),
            CommandRequest::new("systemctl")
                .args(["--no-pager", self.effect.verb()])
                .arg(unit),
        );
        let mut preview = Preview::new(
            self.op.clone(),
            self.effect.description(unit),
            vec![step],
            Vec::new(),
        );
        if self.blocked(unit, critical) {
            preview = preview.with_guard(format!(
                "{unit} is a protected unit: applying this needs the critical flag"
            ));
        }
        Ok(preview)
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let parsed = Args::new(args);
        let unit = parsed.unit_name("unit")?;
        let critical = parsed.flag("critical")?;
        if self.blocked(unit, critical) {
            return Err(LinuxError::ProtectedUnit {
                unit: unit.to_owned(),
            });
        }
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.context.runner, &preview)?;
        let check = self.verify(args)?;
        if !check.ok {
            return Err(LinuxError::VerifyFailed {
                op: self.op.as_str().to_owned(),
                detail: check.detail,
            });
        }
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![self.effect.description(unit)],
        ))
    }

    fn verify(&self, args: &Validated) -> Result<Check, LinuxError> {
        let args = Args::new(args);
        let unit = args.unit_name("unit")?;
        let state = read_unit(&*self.context.runner, unit)?;
        if self.effect.satisfied(&state) {
            Ok(Check::ok(
                &self.op,
                format!("{unit} is in the desired state"),
            ))
        } else {
            Ok(Check::failed(
                &self.op,
                format!(
                    "{unit} is not in the desired state (sub-state {})",
                    state.sub
                ),
            ))
        }
    }
}

/// `os.service.status`: report a unit's state.
pub struct ServiceStatus {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl ServiceStatus {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(SERVICE_STATUS),
        }
    }
}

impl LinuxOperation for ServiceStatus {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let unit = args.unit_name("unit")?;
        let step = Step::new(
            format!("read the state of {unit}"),
            CommandRequest::new("systemctl")
                .args(["--no-pager", "show", SHOW_PROPERTIES])
                .arg(unit),
        );
        Ok(Preview::new(
            self.op.clone(),
            format!("read the state of {unit}"),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.context.runner, &preview)?;
        let args = Args::new(args);
        let unit = args.unit_name("unit")?;
        let state = read_unit(&*self.context.runner, unit)?;
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![format!(
                "{unit} is {} ({})",
                if state.active { "active" } else { "inactive" },
                state.sub
            )],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the unit state is readable"))
    }
}

/// `os.service.failed`: list the failed units.
pub struct ServiceFailed {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl ServiceFailed {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(SERVICE_FAILED),
        }
    }
}

impl LinuxOperation for ServiceFailed {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        let step = Step::new("list failed units", failed_request());
        Ok(Preview::new(
            self.op.clone(),
            "list failed systemd units",
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.context.runner, &preview)?;
        let output = self.context.runner.run(&failed_request())?;
        let count = output.stdout_lines().len();
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![format!("{count} failed unit(s)")],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the failed-unit list is readable"))
    }
}

/// The request that lists failed units.
fn failed_request() -> CommandRequest {
    CommandRequest::new("systemctl").args([
        "--no-pager",
        "--failed",
        "--plain",
        "--no-legend",
        "list-units",
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{Invocation, OpId, ParamValue};

    use super::*;
    use crate::testing::FakeSystem;

    fn context(system: &FakeSystem) -> Arc<LinuxContext> {
        Arc::new(LinuxContext::new(system.runner(), system.filesystem()))
    }

    fn args(op: &str, unit: &str, critical: bool) -> Validated {
        let catalogue = crate::catalogue::build_catalogue().unwrap();
        let mut invocation =
            Invocation::new(OpId::new(op).unwrap()).with("unit", ParamValue::Text(unit.to_owned()));
        if critical {
            invocation = invocation.with("critical", ParamValue::Bool(true));
        }
        catalogue.validate(&invocation).unwrap()
    }

    #[test]
    fn start_is_idempotent() {
        let system = FakeSystem::fedora();
        system.set_service_active("nginx.service", false);
        let module = ServiceControl::new(context(&system), ServiceEffect::Start);
        let args = args(SERVICE_START, "nginx.service", false);
        assert!(!module.plan(&args).unwrap().is_empty());
        module.apply(&args).unwrap();
        assert!(system.snapshot().services["nginx.service"].active);
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn stop_of_a_protected_unit_needs_the_critical_flag() {
        let system = FakeSystem::fedora();
        let module = ServiceControl::new(context(&system), ServiceEffect::Stop);
        let blocked = args(SERVICE_STOP, "spot-agent.service", false);
        // The plan shows the change but warns; apply refuses.
        let preview = module.plan(&blocked).unwrap();
        assert_eq!(preview.guards.len(), 1);
        assert_eq!(
            module.apply(&blocked).unwrap_err().code(),
            crate::error::PROTECTED_UNIT
        );
        // With the flag it proceeds.
        let allowed = args(SERVICE_STOP, "spot-agent.service", true);
        module.apply(&allowed).unwrap();
        assert!(!system.snapshot().services["spot-agent.service"].active);
    }

    #[test]
    fn mask_and_disable_are_verified() {
        let system = FakeSystem::fedora();
        let mask = ServiceControl::new(context(&system), ServiceEffect::Mask);
        let args = args(SERVICE_MASK, "nginx.service", false);
        module_apply(&mask, &args);
        assert!(system.snapshot().services["nginx.service"].masked);
        assert!(mask.plan(&args).unwrap().is_empty());
    }

    fn module_apply(module: &ServiceControl, args: &Validated) {
        module.apply(args).unwrap();
    }

    fn no_params(op: &str) -> Validated {
        let catalogue = crate::catalogue::build_catalogue().unwrap();
        catalogue
            .validate(&Invocation::new(OpId::new(op).unwrap()))
            .unwrap()
    }

    #[test]
    fn failed_units_are_listed() {
        let system = FakeSystem::fedora();
        system.set_service_failed("nginx.service", true);
        let module = ServiceFailed::new(context(&system));
        let outcome = module.apply(&no_params(SERVICE_FAILED)).unwrap();
        assert!(outcome.notes[0].contains("1 failed"));
    }

    #[test]
    fn status_reads_the_unit() {
        let system = FakeSystem::ubuntu();
        let module = ServiceStatus::new(context(&system));
        let outcome = module
            .apply(&args(SERVICE_STATUS, "ssh.service", false))
            .unwrap();
        assert!(outcome.notes[0].contains("active"));
    }
}
