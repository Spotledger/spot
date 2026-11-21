//! The firewall: detect the active backend (firewalld, ufw or nftables), list the rules, and
//! open or close a port rule or a source rule.
//!
//! The lock-out guard is the point of this module. Closing the rule that carries the agent's own
//! management port, or the SSH port while SSH is enabled, would cut the very session an
//! administrator is using. The plan always *shows* that removal and adds a guard note; applying
//! it needs the operation's `critical` flag, so it can never happen by accident.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};

use crate::args::Args;
use crate::command::CommandRequest;
use crate::context::LinuxContext;
use crate::error::LinuxError;
use crate::ids::{self, FIREWALL_CLOSE, FIREWALL_LIST, FIREWALL_OPEN, FIREWALL_STATUS};
use crate::preview::{Check, LinuxOperation, Outcome, Preview, Step, first_line, run_steps};

/// The firewall backend the host is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallBackend {
    /// firewalld (`firewall-cmd`).
    Firewalld,
    /// ufw.
    Ufw,
    /// nftables (`nft`).
    Nftables,
}

impl FirewallBackend {
    /// The backend's short name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Firewalld => "firewalld",
            Self::Ufw => "ufw",
            Self::Nftables => "nftables",
        }
    }

    /// Detects the active backend: firewalld first, then ufw, then nftables.
    ///
    /// # Errors
    /// [`LinuxError::Unsupported`] when none of the three answers, or a command failure.
    pub fn detect(context: &LinuxContext) -> Result<Self, LinuxError> {
        if unit_active(context, concat!("firewalld", ".service"))? {
            return Ok(Self::Firewalld);
        }
        if command_succeeds(context, &CommandRequest::new("ufw").arg("status"))? {
            return Ok(Self::Ufw);
        }
        if command_succeeds(
            context,
            &CommandRequest::new("nft").args(["list", "ruleset"]),
        )? {
            return Ok(Self::Nftables);
        }
        Err(LinuxError::Unsupported {
            what: "firewall backend".to_owned(),
        })
    }
}

/// Whether a systemd unit is active.
fn unit_active(context: &LinuxContext, unit: &str) -> Result<bool, LinuxError> {
    let request = CommandRequest::new("systemctl")
        .args(["--no-pager", "is-active"])
        .arg(unit);
    let output = context.runner.run(&request)?;
    Ok(output.status == 0 && output.stdout.trim() == "active")
}

/// Whether a command succeeds, treating a spawn failure as "not present".
fn command_succeeds(context: &LinuxContext, request: &CommandRequest) -> Result<bool, LinuxError> {
    match context.runner.run(request) {
        Ok(output) => Ok(output.success()),
        Err(LinuxError::CommandSpawn { .. }) => Ok(false),
        Err(other) => Err(other),
    }
}

/// The rules currently open, as `port/protocol` and source strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    /// Open ports, as `port/protocol`.
    pub ports: BTreeSet<String>,
    /// Allowed sources (addresses or CIDRs).
    pub sources: BTreeSet<String>,
}

/// Reads the current rules for `backend`.
fn current_rules(context: &LinuxContext, backend: FirewallBackend) -> Result<RuleSet, LinuxError> {
    match backend {
        FirewallBackend::Firewalld => {
            let output = context
                .runner
                .run(&CommandRequest::new("firewall-cmd").arg("--list-all"))?;
            if !output.success() {
                return Err(LinuxError::CommandFailed {
                    program: "firewall-cmd".to_owned(),
                    status: output.status,
                    detail: first_line(&output.stderr),
                });
            }
            Ok(parse_firewalld(&output.stdout))
        }
        FirewallBackend::Ufw => {
            let output = context
                .runner
                .run(&CommandRequest::new("ufw").args(["status", "numbered"]))?;
            if !output.success() {
                return Err(LinuxError::CommandFailed {
                    program: "ufw".to_owned(),
                    status: output.status,
                    detail: first_line(&output.stderr),
                });
            }
            Ok(parse_ufw(&output.stdout))
        }
        FirewallBackend::Nftables => {
            let output = context
                .runner
                .run(&CommandRequest::new("nft").args(["--handle", "list", "ruleset"]))?;
            if !output.success() {
                return Err(LinuxError::CommandFailed {
                    program: "nft".to_owned(),
                    status: output.status,
                    detail: first_line(&output.stderr),
                });
            }
            Ok(parse_nft(&output.stdout))
        }
    }
}

/// Parses `firewall-cmd --list-all` for the `ports:` and `sources:` lines.
fn parse_firewalld(text: &str) -> RuleSet {
    let mut rules = RuleSet::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ports:") {
            for port in rest.split_whitespace() {
                rules.ports.insert(port.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("sources:") {
            for source in rest.split_whitespace() {
                rules.sources.insert(source.to_owned());
            }
        }
    }
    rules
}

/// Parses `ufw status numbered`.
fn parse_ufw(text: &str) -> RuleSet {
    let mut rules = RuleSet::default();
    for line in text.lines() {
        let line = line.trim();
        if !line.contains("ALLOW") {
            continue;
        }
        let rest = line.rsplit_once(']').map(|(_, rest)| rest).unwrap_or(line);
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        let to = tokens[0];
        let from = tokens[tokens.len() - 1];
        if to != "Anywhere" {
            rules.ports.insert(to.to_owned());
        } else if from != "Anywhere" {
            rules.sources.insert(from.to_owned());
        }
    }
    rules
}

/// Parses an nftables ruleset for `dport` and `saddr` rules.
fn parse_nft(text: &str) -> RuleSet {
    let mut rules = RuleSet::default();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            if *token == "dport" {
                if let Some(port) = tokens.get(index + 1) {
                    rules.ports.insert(format!("{port}/tcp"));
                }
            } else if *token == "saddr"
                && let Some(source) = tokens.get(index + 1)
            {
                rules.sources.insert((*source).to_owned());
            }
        }
    }
    rules
}

/// What an open/close call targets.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// A port rule, as `port/protocol`.
    Port(String),
    /// A source rule.
    Source(String),
}

impl Target {
    /// The rule's display text.
    fn label(&self) -> &str {
        match self {
            Self::Port(port) => port,
            Self::Source(source) => source,
        }
    }
}

/// Reads the target of an open/close call.
fn target(args: &Args<'_>) -> Result<Target, LinuxError> {
    let port = args.optional_int("port")?;
    let source = args.optional_text("source")?;
    match (port, source) {
        (Some(_), Some(_)) => Err(LinuxError::InvalidArgument {
            op: args.op().as_str().to_owned(),
            param: "port".to_owned(),
            detail: "specify a port or a source, not both".to_owned(),
        }),
        (Some(port), None) => {
            let port = u16::try_from(port)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| LinuxError::InvalidArgument {
                    op: args.op().as_str().to_owned(),
                    param: "port".to_owned(),
                    detail: "a port must be between 1 and 65535".to_owned(),
                })?;
            let protocol = args.optional_text("protocol")?.unwrap_or("tcp");
            if protocol != "tcp" && protocol != "udp" {
                return Err(LinuxError::InvalidArgument {
                    op: args.op().as_str().to_owned(),
                    param: "protocol".to_owned(),
                    detail: "the protocol must be tcp or udp".to_owned(),
                });
            }
            Ok(Target::Port(format!("{port}/{protocol}")))
        }
        (None, Some(source)) => {
            if !crate::args::is_source(source) {
                return Err(LinuxError::InvalidArgument {
                    op: args.op().as_str().to_owned(),
                    param: "source".to_owned(),
                    detail: "not a valid address or CIDR".to_owned(),
                });
            }
            Ok(Target::Source(source.to_owned()))
        }
        (None, None) => Err(LinuxError::InvalidArgument {
            op: args.op().as_str().to_owned(),
            param: "port".to_owned(),
            detail: "a port or a source is required".to_owned(),
        }),
    }
}

/// Whether a port is protected by the lock-out guard.
fn port_is_guarded(context: &LinuxContext, port: &str) -> Result<bool, LinuxError> {
    let Some((number, _)) = port.split_once('/') else {
        return Ok(false);
    };
    let Ok(number) = number.parse::<u16>() else {
        return Ok(false);
    };
    if context.management_port == Some(number) {
        return Ok(true);
    }
    if number == context.ssh_port && unit_active(context, &context.ssh_service)? {
        return Ok(true);
    }
    Ok(false)
}

/// The commands that open `target` for `backend`.
fn open_steps(backend: FirewallBackend, target: &Target) -> Result<Vec<Step>, LinuxError> {
    let label = target.label().to_owned();
    match backend {
        FirewallBackend::Firewalld => {
            let flag = match target {
                Target::Port(_) => "--add-port",
                Target::Source(_) => "--add-source",
            };
            Ok(vec![
                Step::new(
                    format!("open {label}"),
                    CommandRequest::new("firewall-cmd").args(["--permanent", flag, &label]),
                ),
                Step::new(
                    "reload firewalld",
                    CommandRequest::new("firewall-cmd").arg("--reload"),
                ),
            ])
        }
        FirewallBackend::Ufw => {
            let request = match target {
                Target::Port(_) => CommandRequest::new("ufw").args(["allow", &label]),
                Target::Source(source) => {
                    CommandRequest::new("ufw").args(["allow", "from", source])
                }
            };
            Ok(vec![Step::new(format!("open {label}"), request)])
        }
        FirewallBackend::Nftables => {
            let request = nft_add_request(target)?;
            Ok(vec![Step::new(format!("open {label}"), request)])
        }
    }
}

/// The commands that close `target` for `backend`.
fn close_steps(
    context: &LinuxContext,
    backend: FirewallBackend,
    target: &Target,
) -> Result<Vec<Step>, LinuxError> {
    let label = target.label().to_owned();
    match backend {
        FirewallBackend::Firewalld => {
            let flag = match target {
                Target::Port(_) => "--remove-port",
                Target::Source(_) => "--remove-source",
            };
            Ok(vec![
                Step::new(
                    format!("close {label}"),
                    CommandRequest::new("firewall-cmd").args(["--permanent", flag, &label]),
                ),
                Step::new(
                    "reload firewalld",
                    CommandRequest::new("firewall-cmd").arg("--reload"),
                ),
            ])
        }
        FirewallBackend::Ufw => {
            let request = match target {
                Target::Port(_) => CommandRequest::new("ufw").args(["delete", "allow", &label]),
                Target::Source(source) => {
                    CommandRequest::new("ufw").args(["delete", "allow", "from", source])
                }
            };
            Ok(vec![Step::new(format!("close {label}"), request)])
        }
        FirewallBackend::Nftables => {
            let handle = nft_find_handle(context, target)?;
            match handle {
                Some(handle) => Ok(vec![Step::new(
                    format!("close {label}"),
                    CommandRequest::new("nft")
                        .args(["delete", "rule", "inet", "spot", "input", "handle"])
                        .arg(handle.to_string()),
                )]),
                None => Ok(Vec::new()),
            }
        }
    }
}

/// The `nft add rule` request for `target`.
fn nft_add_request(target: &Target) -> Result<CommandRequest, LinuxError> {
    let expression: Vec<String> = match target {
        Target::Port(port) => {
            let (number, protocol) = port.split_once('/').unwrap_or((port.as_str(), "tcp"));
            vec![
                protocol.to_owned(),
                "dport".to_owned(),
                number.to_owned(),
                "accept".to_owned(),
            ]
        }
        Target::Source(source) => {
            let family = if source.contains(':') { "ip6" } else { "ip" };
            vec![
                family.to_owned(),
                "saddr".to_owned(),
                source.clone(),
                "accept".to_owned(),
            ]
        }
    };
    Ok(CommandRequest::new("nft")
        .args(["add", "rule", "inet", "spot", "input"])
        .args(expression))
}

/// The nftables handle of the rule matching `target`, when it exists.
fn nft_find_handle(context: &LinuxContext, target: &Target) -> Result<Option<u32>, LinuxError> {
    let output = context.runner.run(
        &CommandRequest::new("nft").args(["--handle", "list", "chain", "inet", "spot", "input"]),
    )?;
    if !output.success() {
        return Err(LinuxError::CommandFailed {
            program: "nft".to_owned(),
            status: output.status,
            detail: first_line(&output.stderr),
        });
    }
    let wanted = nft_add_request(target)?;
    let wanted_key = wanted.args.join(" ");
    let wanted_key = wanted_key
        .strip_prefix("add rule inet spot input ")
        .unwrap_or(&wanted_key);
    for line in output.stdout_lines() {
        let Some((expression, handle)) = line.split_once("# handle") else {
            continue;
        };
        if expression.trim() == wanted_key.trim() {
            return Ok(handle.trim().parse::<u32>().ok());
        }
    }
    Ok(None)
}

/// The firewall status and rule-list modules and the open/close modules.
pub struct FirewallStatus {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl FirewallStatus {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(FIREWALL_STATUS),
        }
    }
}

impl LinuxOperation for FirewallStatus {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        Ok(Preview::new(
            self.op.clone(),
            "detect the active firewall backend",
            Vec::new(),
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let _ = self.plan(args)?;
        let backend = FirewallBackend::detect(&self.context)?;
        Ok(Outcome::new(
            &self.op,
            0,
            vec![format!("active backend: {}", backend.name())],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        let backend = FirewallBackend::detect(&self.context)?;
        Ok(Check::ok(
            &self.op,
            format!("the active backend is {}", backend.name()),
        ))
    }
}

/// `os.firewall.list`: list the open rules.
pub struct FirewallList {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl FirewallList {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(FIREWALL_LIST),
        }
    }
}

impl LinuxOperation for FirewallList {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        let backend = FirewallBackend::detect(&self.context)?;
        let step = Step::new("list firewall rules", list_request(backend));
        Ok(Preview::new(
            self.op.clone(),
            format!("list rules via {}", backend.name()),
            vec![step],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.context.runner, &preview)?;
        let backend = FirewallBackend::detect(&self.context)?;
        let rules = current_rules(&self.context, backend)?;
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![format!(
                "{} port rule(s), {} source rule(s)",
                rules.ports.len(),
                rules.sources.len()
            )],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the rule list is readable"))
    }
}

/// The request that lists rules for `backend`.
fn list_request(backend: FirewallBackend) -> CommandRequest {
    match backend {
        FirewallBackend::Firewalld => CommandRequest::new("firewall-cmd").arg("--list-all"),
        FirewallBackend::Ufw => CommandRequest::new("ufw").args(["status", "numbered"]),
        FirewallBackend::Nftables => {
            CommandRequest::new("nft").args(["--handle", "list", "ruleset"])
        }
    }
}

/// `os.firewall.open` and `os.firewall.close`: open or close a port or source rule.
pub struct FirewallRule {
    context: Arc<LinuxContext>,
    op: OpId,
    open: bool,
}

impl FirewallRule {
    /// A module over `context` that opens when `open`, closes otherwise.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>, open: bool) -> Self {
        Self {
            context,
            op: ids::op_id(if open { FIREWALL_OPEN } else { FIREWALL_CLOSE }),
            open,
        }
    }

    /// Whether the rule already holds in the current ruleset.
    fn satisfied(&self, rules: &RuleSet, target: &Target) -> bool {
        match target {
            Target::Port(port) => rules.ports.contains(port),
            Target::Source(source) => rules.sources.contains(source),
        }
    }

    /// Whether the lock-out guard blocks this close.
    fn blocked(&self, target: &Target, critical: bool) -> Result<bool, LinuxError> {
        if self.open || critical {
            return Ok(false);
        }
        match target {
            Target::Port(port) => port_is_guarded(&self.context, port),
            Target::Source(_) => Ok(false),
        }
    }
}

impl LinuxOperation for FirewallRule {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let target = target(&args)?;
        let critical = args.flag("critical")?;
        let backend = FirewallBackend::detect(&self.context)?;
        let rules = current_rules(&self.context, backend)?;
        if self.satisfied(&rules, &target) == self.open {
            return Ok(Preview::new(
                self.op.clone(),
                format!("rule {} is already in the desired state", target.label()),
                Vec::new(),
                Vec::new(),
            ));
        }
        let steps = if self.open {
            open_steps(backend, &target)?
        } else {
            close_steps(&self.context, backend, &target)?
        };
        let summary = format!(
            "{} {} via {}",
            if self.open { "open" } else { "close" },
            target.label(),
            backend.name()
        );
        let mut preview = Preview::new(self.op.clone(), summary, steps, Vec::new());
        if self.blocked(&target, critical)? {
            preview = preview.with_guard(format!(
                "closing {} would cut management access: applying this needs the critical flag",
                target.label()
            ));
        }
        Ok(preview)
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let parsed = Args::new(args);
        let target = target(&parsed)?;
        let critical = parsed.flag("critical")?;
        if self.blocked(&target, critical)? {
            return Err(LinuxError::LockoutGuard {
                rule: target.label().to_owned(),
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
            vec![preview.summary.clone()],
        ))
    }

    fn verify(&self, args: &Validated) -> Result<Check, LinuxError> {
        let args = Args::new(args);
        let target = target(&args)?;
        let backend = FirewallBackend::detect(&self.context)?;
        let rules = current_rules(&self.context, backend)?;
        if self.satisfied(&rules, &target) == self.open {
            Ok(Check::ok(&self.op, "the rule is in the desired state"))
        } else {
            Ok(Check::failed(
                &self.op,
                "the rule is not in the desired state",
            ))
        }
    }
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

    fn args(op: &str, params: Vec<(&str, ParamValue)>) -> Validated {
        let catalogue = crate::catalogue::build_catalogue().unwrap();
        let mut invocation = Invocation::new(OpId::new(op).unwrap());
        for (name, value) in params {
            invocation = invocation.with(name, value);
        }
        catalogue.validate(&invocation).unwrap()
    }

    fn port(value: i64) -> ParamValue {
        ParamValue::Int(value)
    }

    fn text(value: &str) -> ParamValue {
        ParamValue::Text(value.to_owned())
    }

    #[test]
    fn detection_finds_each_backend() {
        assert_eq!(
            FirewallBackend::detect(&context(&FakeSystem::fedora())).unwrap(),
            FirewallBackend::Firewalld
        );
        assert_eq!(
            FirewallBackend::detect(&context(&FakeSystem::ubuntu())).unwrap(),
            FirewallBackend::Ufw
        );
    }

    #[test]
    fn opening_a_port_is_idempotent_on_firewalld() {
        let system = FakeSystem::fedora();
        let module = FirewallRule::new(context(&system), true);
        let args = args(FIREWALL_OPEN, vec![("port", port(8080))]);
        assert!(!module.plan(&args).unwrap().is_empty());
        module.apply(&args).unwrap();
        assert!(system.snapshot().firewall_ports.contains("8080/tcp"));
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn opening_a_source_is_idempotent_on_ufw() {
        let system = FakeSystem::ubuntu();
        let module = FirewallRule::new(context(&system), true);
        let args = args(FIREWALL_OPEN, vec![("source", text("10.0.0.0/8"))]);
        assert!(!module.plan(&args).unwrap().is_empty());
        module.apply(&args).unwrap();
        assert!(system.snapshot().firewall_sources.contains("10.0.0.0/8"));
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn closing_the_ssh_port_needs_the_critical_flag() {
        let system = FakeSystem::fedora();
        let module = FirewallRule::new(context(&system), false);
        let blocked = args(FIREWALL_CLOSE, vec![("port", port(22))]);
        let preview = module.plan(&blocked).unwrap();
        assert_eq!(preview.guards.len(), 1);
        assert_eq!(
            module.apply(&blocked).unwrap_err().code(),
            crate::error::LOCKOUT_GUARD
        );
        let allowed = args(
            FIREWALL_CLOSE,
            vec![("port", port(22)), ("critical", ParamValue::Bool(true))],
        );
        module.apply(&allowed).unwrap();
        assert!(!system.snapshot().firewall_ports.contains("22/tcp"));
    }

    #[test]
    fn closing_the_ssh_port_is_allowed_when_ssh_is_off() {
        let system = FakeSystem::fedora();
        system.set_service_active("sshd.service", false);
        let module = FirewallRule::new(context(&system), false);
        let args = args(FIREWALL_CLOSE, vec![("port", port(22))]);
        assert!(module.plan(&args).unwrap().guards.is_empty());
        module.apply(&args).unwrap();
    }

    #[test]
    fn a_port_and_a_source_together_are_refused() {
        let system = FakeSystem::fedora();
        let module = FirewallRule::new(context(&system), true);
        let args = args(
            FIREWALL_OPEN,
            vec![("port", port(8080)), ("source", text("10.0.0.0/8"))],
        );
        assert_eq!(
            module.plan(&args).unwrap_err().code(),
            crate::error::INVALID_ARGUMENT
        );
    }

    #[test]
    fn status_reports_the_backend() {
        let system = FakeSystem::ubuntu();
        let module = FirewallStatus::new(context(&system));
        let outcome = module.apply(&args(FIREWALL_STATUS, Vec::new())).unwrap();
        assert!(outcome.notes[0].contains("ufw"));
    }
}
