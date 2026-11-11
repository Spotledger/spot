//! Local users and groups: create, disable, reset a password and manage membership.
//!
//! A reset account's new password is **generated here** from the injected
//! [`PasswordSource`], installed with [`WinApi::set_user_password`] and delivered through
//! the [`SealedResultChannel`]. It never becomes a handler outcome and never appears in a
//! plan, a check or an error message.
//!
//! Built-in accounts (`Administrator`, `Guest` and the other configured names) and the
//! protected groups (`Administrators` by default) are touched only when the operation
//! carries `allow_protected`. Every state-setting operation reads the account first, so
//! creating an existing account, disabling a disabled one and setting the membership it
//! already has all plan as "no change".

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, OperationHandler};
use spot_mgmt_ops::Validated;

use crate::api::{LocalGroup, LocalUser, WinApi};
use crate::args;
use crate::catalogue::{
    GROUP_ADD_MEMBER, GROUP_LIST, GROUP_REMOVE_MEMBER, USER_CREATE, USER_DISABLE, USER_LIST,
    USER_RESET_PASSWORD, USER_SET_GROUPS,
};
use crate::error::WindowsError;
use crate::guards::AgentConfig;
use crate::handlers::handler_outcome;
use crate::plan::{Change, Check, Outcome, Preview};
use crate::secrets::{PasswordSource, SealedResultChannel, SecretLabel};

/// The local users and groups module.
pub struct Users {
    api: Arc<dyn WinApi>,
    config: AgentConfig,
    passwords: Arc<dyn PasswordSource>,
    delivery: Arc<dyn SealedResultChannel>,
}

impl Users {
    /// A users/groups module over `api`, the guards in `config`, the password source and
    /// the sealed result channel.
    #[must_use]
    pub fn new(
        api: Arc<dyn WinApi>,
        config: AgentConfig,
        passwords: Arc<dyn PasswordSource>,
        delivery: Arc<dyn SealedResultChannel>,
    ) -> Self {
        Self {
            api,
            config,
            passwords,
            delivery,
        }
    }

    /// Every local user, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    pub fn list_users(&self) -> Result<Vec<LocalUser>, WindowsError> {
        let mut users = self.api.list_local_users()?;
        users.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(users)
    }

    /// Every local group, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    pub fn list_groups(&self) -> Result<Vec<LocalGroup>, WindowsError> {
        let mut groups = self.api.list_local_groups()?;
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(groups)
    }

    /// One local user by name.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUser`], [`WindowsError::Api`].
    pub fn user(&self, name: &str) -> Result<LocalUser, WindowsError> {
        self.api.local_user(name)
    }

    /// Plans the operation in `args` without touching the machine.
    ///
    /// # Errors
    /// [`WindowsError::UserProtected`] for a protected account or group without the flag,
    /// [`WindowsError::UnknownUser`], [`WindowsError::UnknownGroup`], or any read failure.
    pub fn plan(&self, args: &Validated) -> Result<Preview, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            USER_LIST => {
                self.api.list_local_users()?;
                Ok(Preview::no_change(op))
            }
            GROUP_LIST => {
                self.api.list_local_groups()?;
                Ok(Preview::no_change(op))
            }
            USER_CREATE => {
                let name = args::text(args, "user_name")?;
                self.guard_account(args, name)?;
                let desired = sorted(args::text_list(args, "groups")?);
                self.guard_groups(&desired, args::boolean(args, "allow_protected")?)?;
                match self.lookup(name)? {
                    None => Ok(Preview::with_changes(
                        op,
                        vec![Change::new(
                            account(name),
                            "create",
                            format!("create {name} with a generated password"),
                        )],
                    )),
                    Some(user) => {
                        if desired.is_empty() || sorted(user.groups.clone()) == desired {
                            return Ok(Preview::no_change(op));
                        }
                        Ok(Preview::with_changes(
                            op,
                            vec![Change::new(
                                account(name),
                                "set_groups",
                                format!("set {name} group membership"),
                            )],
                        ))
                    }
                }
            }
            USER_DISABLE => {
                let name = args::text(args, "user_name")?;
                self.guard_account(args, name)?;
                let user = self.require_user(name)?;
                if !user.enabled {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        account(name),
                        "disable",
                        format!("disable {name}"),
                    )],
                ))
            }
            USER_RESET_PASSWORD => {
                let name = args::text(args, "user_name")?;
                self.guard_account(args, name)?;
                self.require_user(name)?;
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        account(name),
                        "reset_password",
                        format!("reset {name}'s password"),
                    )],
                ))
            }
            USER_SET_GROUPS => {
                let name = args::text(args, "user_name")?;
                self.guard_account(args, name)?;
                let desired = sorted(args::text_list(args, "groups")?);
                self.guard_groups(&desired, args::boolean(args, "allow_protected")?)?;
                let user = self.require_user(name)?;
                if sorted(user.groups.clone()) == desired {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        account(name),
                        "set_groups",
                        format!("set {name} group membership"),
                    )],
                ))
            }
            GROUP_ADD_MEMBER | GROUP_REMOVE_MEMBER => {
                let group = args::text(args, "group")?;
                let user = args::text(args, "user_name")?;
                self.guard_group(args, group)?;
                self.guard_account(args, user)?;
                self.require_group(group)?;
                self.require_user(user)?;
                let member = self.is_member(group, user)?;
                let add = args.op.as_str() == GROUP_ADD_MEMBER;
                if member == add {
                    return Ok(Preview::no_change(op));
                }
                let verb = if add { "add_member" } else { "remove_member" };
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        format!("group:{group}"),
                        verb,
                        format!(
                            "{} {user} {} {group}",
                            if add { "add" } else { "remove" },
                            if add { "to" } else { "from" }
                        ),
                    )],
                ))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("user operation {other}"),
            }),
        }
    }

    /// Applies the plan `preview`; a plan with no changes does nothing.
    ///
    /// # Errors
    /// Any control failure, or [`WindowsError::SecretDelivery`] when the sealed channel
    /// refuses a generated password.
    pub fn apply(&self, args: &Validated, preview: &Preview) -> Result<Outcome, WindowsError> {
        let op = args.op.clone();
        if preview.is_no_change() {
            return Ok(Outcome::already_current(op));
        }
        match args.op.as_str() {
            USER_CREATE => {
                let name = args::text(args, "user_name")?;
                let full_name = args::optional_text(args, "full_name")?.unwrap_or("");
                let comment = args::optional_text(args, "comment")?.unwrap_or("");
                let desired = sorted(args::text_list(args, "groups")?);
                if self.lookup(name)?.is_none() {
                    let secret = self.passwords.generate();
                    self.api
                        .create_local_user(name, &secret, full_name, comment)?;
                    self.delivery.deliver(
                        &SecretLabel {
                            op: op.as_str().to_owned(),
                            account: name.to_owned(),
                        },
                        secret,
                    )?;
                }
                if !desired.is_empty() {
                    self.api.set_user_groups(name, &desired)?;
                }
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            USER_DISABLE => {
                self.api
                    .set_user_enabled(args::text(args, "user_name")?, false)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            USER_RESET_PASSWORD => {
                let name = args::text(args, "user_name")?;
                let secret = self.passwords.generate();
                self.api.set_user_password(name, &secret)?;
                self.delivery.deliver(
                    &SecretLabel {
                        op: op.as_str().to_owned(),
                        account: name.to_owned(),
                    },
                    secret,
                )?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            USER_SET_GROUPS => {
                let name = args::text(args, "user_name")?;
                let desired = sorted(args::text_list(args, "groups")?);
                self.api.set_user_groups(name, &desired)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            GROUP_ADD_MEMBER => {
                self.api
                    .add_group_member(args::text(args, "group")?, args::text(args, "user_name")?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            GROUP_REMOVE_MEMBER => {
                self.api.remove_group_member(
                    args::text(args, "group")?,
                    args::text(args, "user_name")?,
                )?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("user operation {other}"),
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
            USER_LIST => {
                let count = self.api.list_local_users()?.len();
                Ok(Check::satisfied(op, format!("{count} users listed")))
            }
            GROUP_LIST => {
                let count = self.api.list_local_groups()?.len();
                Ok(Check::satisfied(op, format!("{count} groups listed")))
            }
            USER_CREATE => {
                let name = args::text(args, "user_name")?;
                let desired = sorted(args::text_list(args, "groups")?);
                match self.lookup(name)? {
                    Some(user) if desired.is_empty() || sorted(user.groups.clone()) == desired => {
                        Ok(Check::satisfied(op, format!("{name} exists")))
                    }
                    Some(_) => Ok(Check::unsatisfied(
                        op,
                        format!("{name} exists but its groups do not match"),
                    )),
                    None => Ok(Check::unsatisfied(op, format!("{name} does not exist"))),
                }
            }
            USER_DISABLE => {
                let name = args::text(args, "user_name")?;
                let user = self.require_user(name)?;
                if user.enabled {
                    Ok(Check::unsatisfied(op, format!("{name} is still enabled")))
                } else {
                    Ok(Check::satisfied(op, format!("{name} is disabled")))
                }
            }
            USER_RESET_PASSWORD => {
                // The password value is never readable, so verification confirms the
                // account exists (the set call succeeded and did not remove it).
                let name = args::text(args, "user_name")?;
                self.require_user(name)?;
                Ok(Check::satisfied(
                    op,
                    format!("{name} exists with a freshly set password"),
                ))
            }
            USER_SET_GROUPS => {
                let name = args::text(args, "user_name")?;
                let desired = sorted(args::text_list(args, "groups")?);
                let user = self.require_user(name)?;
                if sorted(user.groups.clone()) == desired {
                    Ok(Check::satisfied(op, format!("{name} groups match")))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{name} groups do not match"),
                    ))
                }
            }
            GROUP_ADD_MEMBER | GROUP_REMOVE_MEMBER => {
                let group = args::text(args, "group")?;
                let user = args::text(args, "user_name")?;
                let member = self.is_member(group, user)?;
                let add = args.op.as_str() == GROUP_ADD_MEMBER;
                if member == add {
                    Ok(Check::satisfied(
                        op,
                        format!("{user} {} {group}", if add { "is in" } else { "is not in" }),
                    ))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{user} membership in {group} did not change"),
                    ))
                }
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("user operation {other}"),
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

    /// The user with this name, if any.
    fn lookup(&self, name: &str) -> Result<Option<LocalUser>, WindowsError> {
        match self.api.local_user(name) {
            Ok(user) => Ok(Some(user)),
            Err(WindowsError::UnknownUser { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// The user with this name, or [`WindowsError::UnknownUser`].
    fn require_user(&self, name: &str) -> Result<LocalUser, WindowsError> {
        self.api.local_user(name)
    }

    /// The group with this name, or [`WindowsError::UnknownGroup`].
    fn require_group(&self, name: &str) -> Result<LocalGroup, WindowsError> {
        self.api
            .list_local_groups()?
            .into_iter()
            .find(|group| group.name == name)
            .ok_or_else(|| WindowsError::UnknownGroup {
                name: name.to_owned(),
            })
    }

    /// Whether `user` is a member of `group`.
    fn is_member(&self, group: &str, user: &str) -> Result<bool, WindowsError> {
        let group = self.require_group(group)?;
        Ok(group.members.iter().any(|member| member == user))
    }

    /// Applies the account guard for the `user_name` argument.
    fn guard_account(&self, args: &Validated, name: &str) -> Result<(), WindowsError> {
        self.config
            .guard_account(name, args::boolean(args, "allow_protected")?)
    }

    /// Applies the group guard for the `group` argument.
    fn guard_group(&self, args: &Validated, group: &str) -> Result<(), WindowsError> {
        self.config
            .guard_group(group, args::boolean(args, "allow_protected")?)
    }

    /// Applies the group guard to every group in a desired membership.
    fn guard_groups(&self, groups: &[String], allow_protected: bool) -> Result<(), WindowsError> {
        for group in groups {
            self.config.guard_group(group, allow_protected)?;
        }
        Ok(())
    }
}

impl OperationHandler for Users {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        handler_outcome(self.execute(plan.args()))
    }
}

/// The `account:<name>` resource label.
fn account(name: &str) -> String {
    format!("account:{name}")
}

/// A sorted, de-duplicated copy of a membership list, for order-insensitive comparison.
fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};

    use crate::fake::FakeWin;
    use crate::secrets::FixedPasswordSource;

    use super::*;

    #[derive(Default)]
    struct RecordingChannel {
        delivered: std::sync::Mutex<Vec<(String, usize)>>,
    }

    impl SealedResultChannel for RecordingChannel {
        fn deliver(
            &self,
            label: &SecretLabel,
            secret: crate::secrets::Secret,
        ) -> Result<(), WindowsError> {
            self.delivered
                .lock()
                .map_err(|_| WindowsError::SecretDelivery)?
                .push((label.account.clone(), secret.len()));
            Ok(())
        }
    }

    fn module() -> (Arc<FakeWin>, Arc<RecordingChannel>, Users) {
        let api = Arc::new(FakeWin::with_stock_users());
        let channel = Arc::new(RecordingChannel::default());
        let users = Users::new(
            api.clone(),
            AgentConfig::default(),
            Arc::new(FixedPasswordSource::new("Fixed-Passw0rd!Fixed-Pass")),
            channel.clone(),
        );
        (api, channel, users)
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
    fn disabling_a_protected_account_needs_the_flag() {
        let (_api, _channel, users) = module();
        let refused = users
            .plan(&args(
                USER_DISABLE,
                vec![("user_name", ParamValue::Text("Administrator".to_owned()))],
            ))
            .unwrap_err();
        assert_eq!(refused.code(), crate::error::USER_PROTECTED);
    }

    #[test]
    fn creating_a_user_delivers_a_secret_and_never_returns_it() {
        let (_api, channel, users) = module();
        let invocation = args(
            USER_CREATE,
            vec![
                ("user_name", ParamValue::Text("ana2".to_owned())),
                (
                    "groups",
                    ParamValue::List(vec![ParamValue::Text("Users".to_owned())]),
                ),
            ],
        );
        let outcome = users.execute(&invocation).unwrap();
        assert!(!outcome.already_current);
        let delivered = channel.delivered.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "ana2");
        assert!(delivered[0].1 > 0);
    }

    #[test]
    fn creating_the_same_user_twice_is_idempotent() {
        let (_api, _channel, users) = module();
        let invocation = args(
            USER_CREATE,
            vec![("user_name", ParamValue::Text("ana".to_owned()))],
        );
        users.execute(&invocation).unwrap();
        let second = users.execute(&invocation).unwrap();
        assert!(second.already_current);
    }
}
