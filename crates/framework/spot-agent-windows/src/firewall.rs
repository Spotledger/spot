//! The Windows Firewall: list, add/remove a rule and enable/disable a profile.
//!
//! Every change reads the current rules first, so an add of an identical rule, a remove of
//! an absent rule and a profile that already has the requested state all plan as "no
//! change" and apply nothing.
//!
//! The **lock-out guard** refuses to add or remove the rule that carries the agent's own
//! outbound management connection or an RDP rule unless the operation carries
//! `allow_critical`. Without it the agent could cut its own connection or remote access.
//! A rule's ports and profiles are compared as sets, so `80,443` and `443,80` are the same
//! rule.

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, OperationHandler};
use spot_mgmt_ops::Validated;

use crate::api::{Direction, FirewallRule, Profile, Protocol, RuleAction, WinApi};
use crate::args;
use crate::catalogue::{
    FIREWALL_ADD_RULE, FIREWALL_LIST, FIREWALL_REMOVE_RULE, FIREWALL_SET_PROFILE,
};
use crate::error::WindowsError;
use crate::guards::AgentConfig;
use crate::handlers::handler_outcome;
use crate::plan::{Change, Check, Outcome, Preview};

/// The Windows Firewall module.
pub struct Firewall {
    api: Arc<dyn WinApi>,
    config: AgentConfig,
}

impl Firewall {
    /// A firewall module over `api` and the guards in `config`.
    #[must_use]
    pub fn new(api: Arc<dyn WinApi>, config: AgentConfig) -> Self {
        Self { api, config }
    }

    /// Every rule, sorted by name, optionally filtered to one exact name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    pub fn list(&self, name: Option<&str>) -> Result<Vec<FirewallRule>, WindowsError> {
        let mut rules: Vec<FirewallRule> = self
            .api
            .list_firewall_rules()?
            .into_iter()
            .filter(|rule| name.is_none_or(|wanted| rule.name == wanted))
            .collect();
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rules)
    }

    /// Whether one profile is enabled.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    pub fn profile_enabled(&self, profile: Profile) -> Result<bool, WindowsError> {
        self.api.profile_enabled(profile)
    }

    /// Plans the operation in `args` without touching the machine.
    ///
    /// # Errors
    /// [`WindowsError::RuleProtected`] for a protected rule without the flag, or any read
    /// failure.
    pub fn plan(&self, args: &Validated) -> Result<Preview, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            FIREWALL_LIST => {
                let filter = args::optional_text(args, "name")?;
                self.api.list_firewall_rules()?;
                let _ = filter;
                Ok(Preview::no_change(op))
            }
            FIREWALL_ADD_RULE => {
                let rule = self.rule_arg(args)?;
                self.config
                    .guard_rule(&rule.name, args::boolean(args, "allow_critical")?)?;
                let existing = self.find(&rule.name)?;
                if existing.as_ref() == Some(&rule) {
                    return Ok(Preview::no_change(op));
                }
                let action = if existing.is_some() { "update" } else { "add" };
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        format!("firewall-rule:{}", rule.name),
                        action,
                        format!("{action} {} ({})", rule.name, rule.direction.as_str()),
                    )],
                ))
            }
            FIREWALL_REMOVE_RULE => {
                let name = args::text(args, "name")?;
                self.config
                    .guard_rule(name, args::boolean(args, "allow_critical")?)?;
                if self.find(name)?.is_none() {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        format!("firewall-rule:{name}"),
                        "remove",
                        format!("remove {name}"),
                    )],
                ))
            }
            FIREWALL_SET_PROFILE => {
                let profile = profile_arg(args)?;
                let enabled = args::boolean(args, "enabled")?;
                if self.api.profile_enabled(profile)? == enabled {
                    return Ok(Preview::no_change(op));
                }
                let verb = if enabled { "enable" } else { "disable" };
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        format!("firewall-profile:{}", profile.as_str()),
                        verb,
                        format!("{verb} the {} profile", profile.as_str()),
                    )],
                ))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("firewall operation {other}"),
            }),
        }
    }

    /// Applies the plan `preview`; a plan with no changes does nothing.
    ///
    /// # Errors
    /// Any control failure.
    pub fn apply(&self, args: &Validated, preview: &Preview) -> Result<Outcome, WindowsError> {
        let op = args.op.clone();
        if preview.is_no_change() {
            return Ok(Outcome::already_current(op));
        }
        match args.op.as_str() {
            FIREWALL_ADD_RULE => {
                self.api.add_firewall_rule(&self.rule_arg(args)?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            FIREWALL_REMOVE_RULE => {
                self.api.remove_firewall_rule(args::text(args, "name")?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            FIREWALL_SET_PROFILE => {
                self.api
                    .set_profile_enabled(profile_arg(args)?, args::boolean(args, "enabled")?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("firewall operation {other}"),
            }),
        }
    }

    /// Verifies the desired state after an apply.
    ///
    /// # Errors
    /// Any read failure.
    pub fn verify(&self, args: &Validated) -> Result<Check, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            FIREWALL_LIST => {
                let count = self.api.list_firewall_rules()?.len();
                Ok(Check::satisfied(op, format!("{count} rules listed")))
            }
            FIREWALL_ADD_RULE => {
                let rule = self.rule_arg(args)?;
                if self.find(&rule.name)?.as_ref() == Some(&rule) {
                    Ok(Check::satisfied(op, format!("{} is present", rule.name)))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{} does not match", rule.name),
                    ))
                }
            }
            FIREWALL_REMOVE_RULE => {
                let name = args::text(args, "name")?;
                if self.find(name)?.is_none() {
                    Ok(Check::satisfied(op, format!("{name} is absent")))
                } else {
                    Ok(Check::unsatisfied(op, format!("{name} is still present")))
                }
            }
            FIREWALL_SET_PROFILE => {
                let profile = profile_arg(args)?;
                let wanted = args::boolean(args, "enabled")?;
                if self.api.profile_enabled(profile)? == wanted {
                    Ok(Check::satisfied(
                        op,
                        format!("the {} profile is {}", profile.as_str(), state(wanted)),
                    ))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("the {} profile is {}", profile.as_str(), state(!wanted)),
                    ))
                }
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("firewall operation {other}"),
            }),
        }
    }

    /// Plans, applies and verifies `args`.
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

    /// The rule with this name, if any.
    fn find(&self, name: &str) -> Result<Option<FirewallRule>, WindowsError> {
        Ok(self
            .api
            .list_firewall_rules()?
            .into_iter()
            .find(|rule| rule.name == name))
    }

    /// Builds the [`FirewallRule`] named by the arguments.
    fn rule_arg(&self, args: &Validated) -> Result<FirewallRule, WindowsError> {
        let name = args::text(args, "name")?.to_owned();
        let direction = Direction::parse(args::choice(args, "direction")?).ok_or_else(|| {
            WindowsError::InvalidArgument {
                param: "direction".to_owned(),
                detail: "unknown direction".to_owned(),
            }
        })?;
        let protocol = Protocol::parse(args::optional_choice(args, "protocol")?.unwrap_or("tcp"))
            .ok_or_else(|| WindowsError::InvalidArgument {
            param: "protocol".to_owned(),
            detail: "unknown protocol".to_owned(),
        })?;
        let action = RuleAction::parse(args::optional_choice(args, "action")?.unwrap_or("allow"))
            .ok_or_else(|| WindowsError::InvalidArgument {
            param: "action".to_owned(),
            detail: "unknown rule action".to_owned(),
        })?;
        let mut local_ports = Vec::with_capacity(4);
        for port in args::integer_list(args, "ports")? {
            let port = u16::try_from(port).map_err(|_| WindowsError::InvalidArgument {
                param: "ports".to_owned(),
                detail: "a port is outside 0..=65535".to_owned(),
            })?;
            local_ports.push(port);
        }
        local_ports.sort_unstable();
        local_ports.dedup();
        let mut profiles = Vec::with_capacity(3);
        for name in args::text_list(args, "profiles")? {
            let profile = Profile::parse(&name).ok_or_else(|| WindowsError::InvalidArgument {
                param: "profiles".to_owned(),
                detail: "unknown firewall profile".to_owned(),
            })?;
            profiles.push(profile);
        }
        profiles.sort_by_key(|profile| profile_index(*profile));
        profiles.dedup();
        Ok(FirewallRule {
            name,
            direction,
            action,
            protocol,
            local_ports,
            profiles,
            enabled: true,
        })
    }
}

impl OperationHandler for Firewall {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        handler_outcome(self.execute(plan.args()))
    }
}

/// The display order of the three profiles.
fn profile_index(profile: Profile) -> u8 {
    match profile {
        Profile::Domain => 0,
        Profile::Private => 1,
        Profile::Public => 2,
    }
}

/// Reads and parses the `profile` argument.
fn profile_arg(args: &Validated) -> Result<Profile, WindowsError> {
    Profile::parse(args::choice(args, "profile")?).ok_or_else(|| WindowsError::InvalidArgument {
        param: "profile".to_owned(),
        detail: "unknown firewall profile".to_owned(),
    })
}

/// `enabled` or `disabled`, for a check detail.
fn state(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};

    use crate::fake::FakeWin;

    use super::*;

    fn module() -> (Arc<FakeWin>, Firewall) {
        let api = Arc::new(FakeWin::with_stock_firewall());
        let firewall = Firewall::new(api.clone(), AgentConfig::default());
        (api, firewall)
    }

    fn args(op: &str, params: Vec<(&str, ParamValue)>) -> Validated {
        Validated {
            op: OpId::new(op).unwrap(),
            params: params
                .into_iter()
                .map(|(name, value)| AcceptedParam {
                    name: name.to_owned(),
                    ty: ParamType::Bool,
                    value,
                    from_default: false,
                    secret: false,
                })
                .collect(),
        }
    }

    #[test]
    fn removing_the_rdp_rule_needs_the_flag() {
        let (_api, firewall) = module();
        let refused = firewall
            .plan(&args(
                FIREWALL_REMOVE_RULE,
                vec![(
                    "name",
                    ParamValue::Text("Remote Desktop - User Mode (TCP-In)".to_owned()),
                )],
            ))
            .unwrap_err();
        assert_eq!(refused.code(), crate::error::RULE_PROTECTED);
    }

    #[test]
    fn adding_an_identical_rule_plans_no_change() {
        let (_api, firewall) = module();
        let preview = firewall
            .plan(&args(
                FIREWALL_ADD_RULE,
                vec![
                    ("name", ParamValue::Text("Web (HTTP-In)".to_owned())),
                    ("direction", ParamValue::Text("inbound".to_owned())),
                    ("protocol", ParamValue::Text("tcp".to_owned())),
                    (
                        "ports",
                        ParamValue::List(vec![ParamValue::Int(80), ParamValue::Int(443)]),
                    ),
                    ("action", ParamValue::Text("allow".to_owned())),
                    (
                        "profiles",
                        ParamValue::List(vec![
                            ParamValue::Text("domain".to_owned()),
                            ParamValue::Text("private".to_owned()),
                            ParamValue::Text("public".to_owned()),
                        ]),
                    ),
                ],
            ))
            .unwrap();
        assert!(preview.is_no_change(), "{preview:?}");
    }
}
