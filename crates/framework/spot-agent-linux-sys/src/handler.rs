//! The handlers that plug the modules into the privileged executor: one
//! [`OperationHandler`] registered for every catalogue operation this crate owns.
//!
//! The executor runs only registered handlers keyed by operation id, so this is where a
//! verified [`ExecutionPlan`] becomes a machine change. The handler reads the validated
//! parameters, calls the module, and turns a [`SysError`] into a short, secret-free failure
//! detail. It owns `Arc` handles to the command runner, the filesystem and the clock, so a
//! host builds one handler and registers it against the whole catalogue.

use std::path::PathBuf;
use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_core::Clock;
use spot_mgmt_ops::{OpId, ParamValue};

use crate::catalogue::operations;
use crate::certs::{CertificatesModule, CsrRequest};
use crate::chrony::TimeModule;
use crate::error::{SysError, SysResult};
use crate::mac::{MacMode, MacModule};
use crate::network::{Addressing, NetworkChange, NetworkModule, Recovery, RevertRun};
use crate::preview::Outcome;
use crate::revert::{DEFAULT_STATE_DIR, SystemdRunScheduler};
use crate::storage::{SnapshotBackend, SnapshotRequest, StorageModule};
use crate::sys::{CommandRunner, FileSystem};
use crate::sysctl::SysctlModule;

/// One handler for every operation this crate registers.
pub struct LinuxSysHandler {
    runner: Arc<dyn CommandRunner>,
    fs: Arc<dyn FileSystem>,
    clock: Arc<dyn Clock>,
    state_dir: PathBuf,
    revert_command: Option<Vec<String>>,
}

impl LinuxSysHandler {
    /// Builds a handler over the three seams. The network revert ticket is persisted under
    /// [`DEFAULT_STATE_DIR`] (see [`Self::with_state_dir`]).
    #[must_use]
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        fs: Arc<dyn FileSystem>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            runner,
            fs,
            clock,
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            revert_command: None,
        }
    }

    /// Sets the agent state directory the network revert ticket is persisted under.
    #[must_use]
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = dir.into();
        self
    }

    /// Sets the command an independent systemd timer runs to revert a network change when
    /// the agent is down (program and fixed arguments; the ticket id is appended). It must
    /// call [`NetworkModule::revert_ticket`]. Without it only the in-agent sweep reverts.
    #[must_use]
    pub fn with_revert_command(mut self, command: Vec<String>) -> Self {
        self.revert_command = Some(command);
        self
    }

    /// Runs `body` with a network module wired to the state directory and, when a revert
    /// command is configured, the independent revert timer.
    fn with_network<R>(
        &self,
        body: impl FnOnce(&NetworkModule<'_>) -> SysResult<R>,
    ) -> SysResult<R> {
        let scheduler = match &self.revert_command {
            Some(command) => Some(SystemdRunScheduler::new(
                self.runner.as_ref(),
                command.clone(),
            )?),
            None => None,
        };
        let mut module = NetworkModule::new(
            self.runner.as_ref(),
            self.fs.as_ref(),
            Arc::clone(&self.clock),
        )
        .with_state_dir(self.state_dir.clone());
        if let Some(scheduler) = &scheduler {
            module = module.with_scheduler(scheduler);
        }
        body(&module)
    }

    /// Deals with a network change left pending by a previous run of the agent (reverts it,
    /// or re-arms its timer). The host calls this once at agent start.
    ///
    /// # Errors
    /// See [`NetworkModule::recover`].
    pub fn recover_network(&self) -> SysResult<Recovery> {
        self.with_network(|module| module.recover())
    }

    /// Reverts network change `id` now; the entry point the independent timer runs.
    ///
    /// # Errors
    /// See [`NetworkModule::revert_ticket`].
    pub fn revert_network_ticket(&self, id: u64) -> SysResult<Option<Outcome>> {
        self.with_network(|module| module.revert_ticket(id))
    }

    /// [`Self::revert_network_ticket`] telling "nothing pending" from "another ticket
    /// pending"; the host's `network-revert` entry point uses it.
    ///
    /// # Errors
    /// See [`NetworkModule::revert_ticket_checked`].
    pub fn revert_network_ticket_checked(&self, id: u64) -> SysResult<RevertRun> {
        self.with_network(|module| module.revert_ticket_checked(id))
    }

    /// Registers `handler` for every operation in this crate's catalogue.
    pub fn register(handler: &Arc<Self>, registry: &mut HandlerRegistry) {
        for spec in operations() {
            let registered: Arc<dyn OperationHandler> = handler.clone();
            registry.register(spec.id.clone(), registered);
        }
    }

    /// Runs the operation the plan names.
    fn dispatch(&self, plan: &ExecutionPlan) -> SysResult<()> {
        let segments: Vec<&str> = plan.op().segments().collect();
        let runner = self.runner.as_ref();
        let fs = self.fs.as_ref();
        match segments.as_slice() {
            ["os", "storage", "facts"] => {
                StorageModule::new(runner, fs).facts()?;
            }
            ["os", "storage", "free"] => {
                let path = text(plan, "path")?;
                StorageModule::new(runner, fs).free_space(std::path::Path::new(&path))?;
            }
            ["os", "snapshot", "list"] => {
                let request = snapshot_request(plan)?;
                StorageModule::new(runner, fs).list(request.backend, &request.source)?;
            }
            ["os", "snapshot", "create"] => {
                let request = snapshot_request(plan)?;
                StorageModule::new(runner, fs).apply_create(&request)?;
            }
            ["os", "snapshot", "delete"] => {
                let request = snapshot_request(plan)?;
                StorageModule::new(runner, fs).apply_delete(&request)?;
            }
            ["os", "snapshot", "restore"] => {
                let request = snapshot_request(plan)?;
                StorageModule::new(runner, fs).apply_restore(&request, plan.issued_at())?;
            }
            ["os", "network", "facts"] => {
                self.with_network(|module| module.facts())?;
            }
            ["os", "network", "set"] => {
                let change = network_change(plan)?;
                let window = duration(plan, "window_secs").unwrap_or(120);
                self.with_network(|module| module.apply(&change, window))?;
            }
            ["os", "network", "confirm"] => {
                let ticket = u64::try_from(int(plan, "ticket")?).unwrap_or(0);
                self.with_network(|module| module.confirm(ticket))?;
            }
            ["os", "network", "revert"] => {
                self.with_network(|module| module.sweep())?;
            }
            ["os", "cert", "list"] => {
                CertificatesModule::new(runner, fs).list()?;
            }
            ["os", "cert", "check"] => {
                let name = text(plan, "name")?;
                let within = duration(plan, "within_secs").unwrap_or(0);
                let module = CertificatesModule::new(runner, fs);
                module.check_expiry(&name, within)?;
                if boolean(plan, "chain").unwrap_or(false) {
                    let path = module.anchor_path(&name)?;
                    module.check_chain(&path.display().to_string())?;
                }
            }
            ["os", "cert", "install"] => {
                let name = text(plan, "name")?;
                let pem = text(plan, "pem")?;
                CertificatesModule::new(runner, fs).apply_install(&name, &pem)?;
            }
            ["os", "cert", "remove"] => {
                let name = text(plan, "name")?;
                CertificatesModule::new(runner, fs).apply_remove(&name)?;
            }
            ["os", "cert", "csr"] => {
                let request = CsrRequest {
                    subject: text(plan, "subject")?,
                    key_path: text(plan, "key_path")?,
                    csr_path: text(plan, "csr_path")?,
                    key_bits: u32::try_from(int(plan, "key_bits")?).unwrap_or(2048),
                };
                CertificatesModule::new(runner, fs).apply_csr(&request)?;
            }
            ["os", "time", "status"] => {
                TimeModule::new(runner, fs).status()?;
            }
            ["os", "time", "sources"] => {
                TimeModule::new(runner, fs).sources()?;
            }
            ["os", "time", "servers"] => {
                let servers = list(plan, "servers")?;
                TimeModule::new(runner, fs).apply_servers(&servers)?;
            }
            ["os", "time", "step"] => {
                let max_secs = int(plan, "max_jump_secs")?.max(0);
                let guard = max_secs.saturating_mul(1_000_000_000);
                TimeModule::new(runner, fs).force_step(plan.issued_at(), guard)?;
            }
            ["os", "sysctl", "list"] => {
                SysctlModule::new(runner, fs).list()?;
            }
            ["os", "sysctl", "get"] => {
                let key = text(plan, "key")?;
                SysctlModule::new(runner, fs).get(&key)?;
            }
            ["os", "sysctl", "set"] => {
                let key = text(plan, "key")?;
                let value = int(plan, "value")?;
                SysctlModule::new(runner, fs).apply_set(&key, value)?;
            }
            ["os", "mac", "status"] => {
                MacModule::new(runner, fs).status()?;
            }
            ["os", "mac", "denials"] => {
                MacModule::new(runner, fs).denials()?;
            }
            ["os", "mac", "mode"] => {
                let mode = mac_mode(&text(plan, "mode")?)?;
                let critical = boolean(plan, "critical").unwrap_or(false);
                MacModule::new(runner, fs).apply_mode(mode, critical)?;
            }
            ["os", "mac", "policy"] => {
                let name = text(plan, "name")?;
                let path = text(plan, "path")?;
                MacModule::new(runner, fs).install_policy(&name, &path)?;
            }
            other => {
                return Err(SysError::unsupported(format!(
                    "no handler is implemented for {other:?}"
                )));
            }
        }
        Ok(())
    }
}

impl OperationHandler for LinuxSysHandler {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        match self.dispatch(plan) {
            Ok(()) => HandlerOutcome::Succeeded,
            Err(error) => HandlerOutcome::Failed {
                detail: format!("{}: {}", error.code, error.message),
            },
        }
    }
}

/// Reads a text parameter.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] when it is missing or not text.
fn text(plan: &ExecutionPlan, name: &str) -> SysResult<String> {
    match value(plan, name) {
        Some(ParamValue::Text(text)) => Ok(text.clone()),
        _ => Err(missing(name)),
    }
}

/// Reads an integer parameter.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] when it is missing or not an integer.
fn int(plan: &ExecutionPlan, name: &str) -> SysResult<i64> {
    match value(plan, name) {
        Some(ParamValue::Int(value)) => Ok(*value),
        _ => Err(missing(name)),
    }
}

/// Reads a boolean parameter.
fn boolean(plan: &ExecutionPlan, name: &str) -> Option<bool> {
    match value(plan, name) {
        Some(ParamValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

/// Reads a duration parameter in whole seconds.
fn duration(plan: &ExecutionPlan, name: &str) -> Option<u64> {
    match value(plan, name) {
        Some(ParamValue::Duration(value)) => Some(*value),
        _ => None,
    }
}

/// Reads a list-of-text parameter.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] when it is missing or not a list of text.
fn list(plan: &ExecutionPlan, name: &str) -> SysResult<Vec<String>> {
    match value(plan, name) {
        Some(ParamValue::List(items)) => items
            .iter()
            .map(|item| match item {
                ParamValue::Text(text) => Ok(text.clone()),
                _ => Err(missing(name)),
            })
            .collect(),
        _ => Err(missing(name)),
    }
}

/// Finds a validated parameter by name.
fn value<'a>(plan: &'a ExecutionPlan, name: &str) -> Option<&'a ParamValue> {
    plan.args()
        .params
        .iter()
        .find(|param| param.name == name)
        .map(|param| &param.value)
}

/// The refusal for a parameter the handler needed but did not find.
fn missing(name: &str) -> SysError {
    SysError::invalid(format!("the plan is missing the {name} parameter"))
}

/// Builds a snapshot request from the plan's parameters.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] for a bad backend, source or name.
fn snapshot_request(plan: &ExecutionPlan) -> SysResult<SnapshotRequest> {
    let backend = SnapshotBackend::from_name(&text(plan, "backend")?)
        .ok_or_else(|| SysError::invalid("the snapshot backend is not one of btrfs, lvm or zfs"))?;
    let source = text(plan, "source")?;
    let name = text(plan, "name")?;
    let size = int(plan, "size_bytes")
        .ok()
        .and_then(|value| u64::try_from(value).ok());
    SnapshotRequest::new(backend, source, name, size)
}

/// Builds a network change from the plan's parameters.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] for a bad interface or addressing.
fn network_change(plan: &ExecutionPlan) -> SysResult<NetworkChange> {
    let interface = text(plan, "interface")?;
    let addressing = match text(plan, "mode")?.as_str() {
        "dhcp" => Addressing::Dhcp,
        "static" => {
            let address = text(plan, "address")?;
            let prefix =
                u8::try_from(int(plan, "prefix").unwrap_or(24).clamp(0, 128)).unwrap_or(24);
            let gateway = text(plan, "gateway").ok();
            let dns = list(plan, "dns").unwrap_or_default();
            Addressing::Static {
                address,
                prefix,
                gateway,
                dns,
            }
        }
        other => {
            return Err(SysError::invalid(format!(
                "the addressing mode {other:?} is neither dhcp nor static"
            )));
        }
    };
    NetworkChange::new(interface, addressing)
}

/// Parses the mandatory access control mode.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] for an unknown mode.
fn mac_mode(text: &str) -> SysResult<MacMode> {
    match text {
        "enforcing" => Ok(MacMode::Enforcing),
        "permissive" => Ok(MacMode::Permissive),
        "disabled" => Ok(MacMode::Disabled),
        other => Err(SysError::invalid(format!("{other:?} is not a MAC mode"))),
    }
}

/// The operation ids this crate registers, for a host that wants them without building the
/// catalogue.
#[must_use]
pub fn operation_ids() -> Vec<OpId> {
    operations().into_iter().map(|spec| spec.id).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};
    use spot_agent_core::{ExecutionPlan, PlanParts};
    use spot_core::ManualClock;
    use spot_mgmt_ops::{OpId, ParamValue, Risk, Validated};

    fn plan(op: &str, params: Vec<(&str, ParamValue)>) -> ExecutionPlan {
        let op = OpId::new(op).unwrap();
        let validated = Validated {
            op: op.clone(),
            params: params
                .into_iter()
                .map(|(name, value)| spot_mgmt_ops::AcceptedParam {
                    name: name.to_owned(),
                    ty: spot_mgmt_ops::ParamType::Text {
                        max_len: 4096,
                        pattern: None,
                    },
                    value,
                    from_default: false,
                    secret: false,
                })
                .collect(),
        };
        ExecutionPlan::new(PlanParts {
            agent_id: "agent-1".to_owned(),
            command_id: "c1".to_owned(),
            command_digest: [1u8; 32],
            op,
            args: validated,
            risk: Risk::Medium,
            signers: Vec::new(),
            issued_at: 1_000,
        })
    }

    fn handler(runner: &Arc<FakeRunner>, fs: &Arc<FakeFs>) -> Arc<LinuxSysHandler> {
        let runner: Arc<dyn CommandRunner> = runner.clone();
        let fs: Arc<dyn FileSystem> = fs.clone();
        Arc::new(LinuxSysHandler::new(
            runner,
            fs,
            Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH)),
        ))
    }

    #[test]
    fn a_read_operation_succeeds() {
        let runner = Arc::new(
            FakeRunner::new()
                .with_response("getenforce", ok_output("Enforcing\n"))
                .with_response("semodule -l", ok_output("")),
        );
        let fs = Arc::new(FakeFs::new());
        let handler = handler(&runner, &fs);
        let outcome = handler.handle(&plan("os.mac.status", vec![]));
        assert_eq!(outcome, HandlerOutcome::Succeeded);
    }

    #[test]
    fn a_failing_operation_reports_its_code() {
        let runner = Arc::new(
            FakeRunner::new()
                .with_response(
                    "getenforce",
                    crate::sys::CommandOutput::new(127, "", "not found"),
                )
                .with_response(
                    "aa-status --json",
                    crate::sys::CommandOutput::new(127, "", "not found"),
                ),
        );
        let fs = Arc::new(FakeFs::new());
        let handler = handler(&runner, &fs);
        let outcome = handler.handle(&plan("os.mac.status", vec![]));
        match outcome {
            HandlerOutcome::Failed { detail } => {
                assert!(detail.contains("agent_linux_sys.mac_unknown"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    fn network_runner() -> Arc<FakeRunner> {
        Arc::new(
            FakeRunner::new()
                .with_response(
                    "ip -j address show",
                    ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
                )
                .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
                .with_response(
                    "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ",
                    ok_output(""),
                )
                .with_response("nmcli connection up eth0", ok_output("")),
        )
    }

    fn set_dhcp() -> ExecutionPlan {
        plan(
            "os.network.set",
            vec![
                ("interface", ParamValue::Text("eth0".to_owned())),
                ("mode", ParamValue::Text("dhcp".to_owned())),
            ],
        )
    }

    #[test]
    fn a_pending_network_change_survives_from_set_to_confirm_and_a_restart() {
        // The handler builds a fresh module for every operation, so the ticket must live on
        // disk: `set` then `confirm` in separate calls is the production path.
        let runner = network_runner();
        let fs = Arc::new(FakeFs::new());
        let first = handler(&runner, &fs);
        assert_eq!(first.handle(&set_dhcp()), HandlerOutcome::Succeeded);
        assert!(fs.has_file("/var/lib/spot-agent/network-revert.ticket.json"));

        // A restarted agent finds it and re-arms; it is not lost.
        let restarted = handler(&runner, &fs);
        assert!(matches!(
            restarted.recover_network().unwrap(),
            crate::Recovery::Rearmed { .. }
        ));

        // Confirm arrives through yet another handler. The first id of a fresh counter is
        // the clock's second (1 at the epoch).
        let confirm = plan("os.network.confirm", vec![("ticket", ParamValue::Int(1))]);
        assert_eq!(restarted.handle(&confirm), HandlerOutcome::Succeeded);
        assert!(!fs.has_file("/var/lib/spot-agent/network-revert.ticket.json"));
    }

    #[test]
    fn the_revert_timer_is_scheduled_when_a_revert_command_is_configured() {
        let runner = network_runner();
        runner.set_response(
            "systemd-run --unit=spot-net-revert-1 --on-active=120s --timer-property=AccuracySec=1s -- /usr/libexec/spot-agent network-revert 1",
            ok_output(""),
        );
        let fs = Arc::new(FakeFs::new());
        let runner_dyn: Arc<dyn CommandRunner> = runner.clone();
        let fs_dyn: Arc<dyn FileSystem> = fs.clone();
        let handler = LinuxSysHandler::new(
            runner_dyn,
            fs_dyn,
            Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH)),
        )
        .with_state_dir("/state")
        .with_revert_command(vec![
            "/usr/libexec/spot-agent".to_owned(),
            "network-revert".to_owned(),
        ]);
        assert_eq!(handler.handle(&set_dhcp()), HandlerOutcome::Succeeded);
        assert!(fs.has_file("/state/network-revert.ticket.json"));
        assert!(
            runner
                .call_keys()
                .iter()
                .any(|key| key.starts_with("systemd-run --unit=spot-net-revert-1"))
        );
    }

    #[test]
    fn registering_covers_the_whole_catalogue() {
        let runner = Arc::new(FakeRunner::new());
        let fs = Arc::new(FakeFs::new());
        let handler = handler(&runner, &fs);
        let mut registry = HandlerRegistry::new();
        LinuxSysHandler::register(&handler, &mut registry);
        assert_eq!(registry.len(), operations().len());
        assert!(registry.contains(&OpId::new("os.network.set").unwrap()));
        assert!(registry.contains(&OpId::new("os.snapshot.restore").unwrap()));
    }
}
