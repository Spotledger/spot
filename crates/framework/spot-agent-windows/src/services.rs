//! Services: list, status, start/stop/restart with timeouts, set start type and recovery.
//!
//! The module reads state before it acts, so `plan` never touches the machine and `apply`
//! is idempotent for every state-setting operation: starting a running service, stopping a
//! stopped one, setting the start type it already has and writing the recovery policy it
//! already has all plan as "no change" and apply nothing. A **restart** is an action rather
//! than a state, so it always plans a change.
//!
//! Stopping or restarting a service named in [`AgentConfig`] — the agent itself, the
//! runtime, or an extra protected service — is refused unless the operation carries
//! `allow_critical`.

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, OperationHandler};
use spot_mgmt_ops::Validated;

use crate::api::{RecoveryAction, RecoveryConfig, ServiceInfo, ServiceState, StartType, WinApi};
use crate::args;
use crate::catalogue::{
    SERVICE_LIST, SERVICE_RESTART, SERVICE_SET_RECOVERY, SERVICE_SET_START_TYPE, SERVICE_START,
    SERVICE_STATUS, SERVICE_STOP,
};
use crate::error::WindowsError;
use crate::guards::AgentConfig;
use crate::handlers::handler_outcome;
use crate::plan::{Change, Check, Outcome, Preview};

/// The Windows services module.
pub struct Services {
    api: Arc<dyn WinApi>,
    config: AgentConfig,
}

impl Services {
    /// A services module over `api` and the guards in `config`.
    #[must_use]
    pub fn new(api: Arc<dyn WinApi>, config: AgentConfig) -> Self {
        Self { api, config }
    }

    /// Every service, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`] when the enumeration fails.
    pub fn list(&self) -> Result<Vec<ServiceInfo>, WindowsError> {
        let mut services = self.api.list_services()?;
        services.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(services)
    }

    /// One service by name.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::Api`].
    pub fn status(&self, name: &str) -> Result<ServiceInfo, WindowsError> {
        self.api.service_status(name)
    }

    /// Plans the operation in `args` without touching the machine.
    ///
    /// # Errors
    /// The guard refusal for a protected service, or any read failure.
    pub fn plan(&self, args: &Validated) -> Result<Preview, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            SERVICE_LIST => {
                self.api.list_services()?;
                Ok(Preview::no_change(op))
            }
            SERVICE_STATUS => {
                let name = args::text(args, "name")?;
                self.api.service_status(name)?;
                Ok(Preview::no_change(op))
            }
            SERVICE_START => {
                let name = args::text(args, "name")?;
                let info = self.api.service_status(name)?;
                if info.state == ServiceState::Running {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        resource(name),
                        "start",
                        format!("start {} (currently {})", info.name, info.state.as_str()),
                    )],
                ))
            }
            SERVICE_STOP => {
                let name = args::text(args, "name")?;
                self.config
                    .guard_service(name, args::boolean(args, "allow_critical")?)?;
                let info = self.api.service_status(name)?;
                if info.state == ServiceState::Stopped {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        resource(name),
                        "stop",
                        format!("stop {} (currently {})", info.name, info.state.as_str()),
                    )],
                ))
            }
            SERVICE_RESTART => {
                let name = args::text(args, "name")?;
                self.config
                    .guard_service(name, args::boolean(args, "allow_critical")?)?;
                let info = self.api.service_status(name)?;
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        resource(name),
                        "restart",
                        format!("restart {} (currently {})", info.name, info.state.as_str()),
                    )],
                ))
            }
            SERVICE_SET_START_TYPE => {
                let name = args::text(args, "name")?;
                let start_type = start_type_arg(args)?;
                let info = self.api.service_status(name)?;
                if info.start_type == start_type {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        resource(name),
                        "set_start_type",
                        format!(
                            "set {} start type {} -> {}",
                            info.name,
                            info.start_type.as_str(),
                            start_type.as_str()
                        ),
                    )],
                ))
            }
            SERVICE_SET_RECOVERY => {
                let name = args::text(args, "name")?;
                let desired = recovery_arg(args)?;
                let current = self.api.service_recovery(name)?;
                if current == desired {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        resource(name),
                        "set_recovery",
                        format!("set {} recovery actions", name),
                    )],
                ))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("service operation {other}"),
            }),
        }
    }

    /// Applies the plan `preview`; a plan with no changes does nothing.
    ///
    /// # Errors
    /// The guard refusal for a protected service, or any control failure.
    pub fn apply(&self, args: &Validated, preview: &Preview) -> Result<Outcome, WindowsError> {
        let op = args.op.clone();
        if preview.is_no_change() {
            return Ok(Outcome::already_current(op));
        }
        match args.op.as_str() {
            SERVICE_START => {
                let name = args::text(args, "name")?;
                let wait = args::duration_or(args, "wait", 30)?;
                self.api.start_service(name, wait)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            SERVICE_STOP => {
                let name = args::text(args, "name")?;
                let wait = args::duration_or(args, "wait", 30)?;
                self.api.stop_service(name, wait)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            SERVICE_RESTART => {
                let name = args::text(args, "name")?;
                let wait = args::duration_or(args, "wait", 30)?;
                self.api.stop_service(name, wait)?;
                self.api.start_service(name, wait)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            SERVICE_SET_START_TYPE => {
                let name = args::text(args, "name")?;
                self.api
                    .set_service_start_type(name, start_type_arg(args)?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            SERVICE_SET_RECOVERY => {
                let name = args::text(args, "name")?;
                self.api.set_service_recovery(name, &recovery_arg(args)?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("service operation {other}"),
            }),
        }
    }

    /// Verifies that the desired state holds after an apply.
    ///
    /// # Errors
    /// Any read failure.
    pub fn verify(&self, args: &Validated) -> Result<Check, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            SERVICE_LIST => {
                let count = self.api.list_services()?.len();
                Ok(Check::satisfied(op, format!("{count} services listed")))
            }
            SERVICE_STATUS => {
                let name = args::text(args, "name")?;
                let info = self.api.service_status(name)?;
                Ok(Check::satisfied(
                    op,
                    format!("{} is {}", info.name, info.state.as_str()),
                ))
            }
            SERVICE_START => {
                let name = args::text(args, "name")?;
                let info = self.api.service_status(name)?;
                if info.state == ServiceState::Running {
                    Ok(Check::satisfied(op, format!("{name} is running")))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} is {}", info.state.as_str()),
                    ))
                }
            }
            SERVICE_STOP => {
                let name = args::text(args, "name")?;
                let info = self.api.service_status(name)?;
                if info.state == ServiceState::Stopped {
                    Ok(Check::satisfied(op, format!("{name} is stopped")))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} is {}", info.state.as_str()),
                    ))
                }
            }
            SERVICE_RESTART => {
                let name = args::text(args, "name")?;
                let info = self.api.service_status(name)?;
                if info.state == ServiceState::Running {
                    Ok(Check::satisfied(op, format!("{name} is running")))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} is {}", info.state.as_str()),
                    ))
                }
            }
            SERVICE_SET_START_TYPE => {
                let name = args::text(args, "name")?;
                let wanted = start_type_arg(args)?;
                let info = self.api.service_status(name)?;
                if info.start_type == wanted {
                    Ok(Check::satisfied(
                        op,
                        format!("{name} starts {}", wanted.as_str()),
                    ))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} starts {}", info.start_type.as_str()),
                    ))
                }
            }
            SERVICE_SET_RECOVERY => {
                let name = args::text(args, "name")?;
                let wanted = recovery_arg(args)?;
                let current = self.api.service_recovery(name)?;
                if current == wanted {
                    Ok(Check::satisfied(op, format!("{name} recovery is set")))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} recovery does not match"),
                    ))
                }
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("service operation {other}"),
            }),
        }
    }

    /// Plans, applies and verifies `args`, refusing when verification does not confirm.
    ///
    /// # Errors
    /// Any plan/apply failure, or [`WindowsError::VerifyFailed`].
    pub fn execute(&self, args: &Validated) -> Result<Outcome, WindowsError> {
        let preview = self.plan(args)?;
        let outcome = self.apply(args, &preview)?;
        let check = self.verify(args)?;
        if !check.satisfied {
            return Err(WindowsError::VerifyFailed {
                op: args.op.as_str().to_owned(),
                detail: check.detail,
            });
        }
        Ok(outcome)
    }
}

impl OperationHandler for Services {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        handler_outcome(self.execute(plan.args()))
    }
}

/// The `service:<name>` resource label.
fn resource(name: &str) -> String {
    format!("service:{name}")
}

/// Reads and parses the `start_type` argument.
fn start_type_arg(args: &Validated) -> Result<StartType, WindowsError> {
    let text = args::choice(args, "start_type")?;
    StartType::parse(text).ok_or_else(|| WindowsError::InvalidArgument {
        param: "start_type".to_owned(),
        detail: "unknown start type".to_owned(),
    })
}

/// Reads the recovery arguments into a [`RecoveryConfig`].
fn recovery_arg(args: &Validated) -> Result<RecoveryConfig, WindowsError> {
    let mut actions = Vec::with_capacity(3);
    for name in ["first_action", "second_action", "third_action"] {
        let text = args::choice(args, name)?;
        actions.push(
            RecoveryAction::parse(text).ok_or_else(|| WindowsError::InvalidArgument {
                param: name.to_owned(),
                detail: "unknown recovery action".to_owned(),
            })?,
        );
    }
    let restart_delay = args::duration_or(args, "restart_delay", 60)?;
    let reset_period = args::duration_or(args, "reset_period", 86_400)?;
    Ok(RecoveryConfig {
        actions,
        restart_delay_seconds: u32::try_from(restart_delay).unwrap_or(u32::MAX),
        reset_period_seconds: u32::try_from(reset_period).unwrap_or(u32::MAX),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::fake::FakeWin;

    use super::*;

    fn module() -> (Arc<FakeWin>, Services) {
        let api = Arc::new(FakeWin::with_stock_services());
        let services = Services::new(api.clone(), AgentConfig::default());
        (api, services)
    }

    fn args(op: &str, params: Vec<(&str, spot_mgmt_ops::ParamValue)>) -> Validated {
        let mut validated = Validated {
            op: spot_mgmt_ops::OpId::new(op).unwrap(),
            params: Vec::new(),
        };
        for (name, value) in params {
            validated.params.push(spot_mgmt_ops::AcceptedParam {
                name: name.to_owned(),
                ty: spot_mgmt_ops::ParamType::Bool,
                value,
                from_default: false,
                secret: false,
            });
        }
        validated
    }

    #[test]
    fn stopping_a_protected_service_needs_the_flag() {
        let (_api, services) = module();
        let refused = services
            .plan(&args(
                SERVICE_STOP,
                vec![(
                    "name",
                    spot_mgmt_ops::ParamValue::Text("spot-runtime".to_owned()),
                )],
            ))
            .unwrap_err();
        assert_eq!(refused.code(), crate::error::SERVICE_PROTECTED);
        assert!(
            services
                .plan(&args(
                    SERVICE_STOP,
                    vec![
                        (
                            "name",
                            spot_mgmt_ops::ParamValue::Text("spot-runtime".to_owned()),
                        ),
                        ("allow_critical", spot_mgmt_ops::ParamValue::Bool(true)),
                    ],
                ))
                .is_ok()
        );
    }

    #[test]
    fn starting_a_running_service_plans_no_change() {
        let (_api, services) = module();
        let preview = services
            .plan(&args(
                SERVICE_START,
                vec![(
                    "name",
                    spot_mgmt_ops::ParamValue::Text("wuauserv".to_owned()),
                )],
            ))
            .unwrap();
        assert!(preview.is_no_change());
    }
}
