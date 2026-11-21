//! Users and groups: create, lock, unlock and delete accounts, read an account's groups, and
//! add or remove a supplementary group membership.
//!
//! Accounts are read from `/etc/passwd` and their lock state from `/etc/shadow`, so a plan can
//! see whether anything would change. No operation touches an account whose UID is below the
//! context's floor unless the UID is explicitly allow-listed: the agent must not lock, delete or
//! re-home a system account by accident. Sudoers drop-ins are **not** managed here.
//!
//! Every account and group name is validated twice — by the catalogue schema, and again here
//! against `^[a-z_][a-z0-9_-]{0,31}$` — and passed as one `argv` entry after `--`.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};

use crate::args::Args;
use crate::command::CommandRequest;
use crate::context::LinuxContext;
use crate::error::LinuxError;
use crate::ids::{
    self, USER_ADD_GROUP, USER_CREATE, USER_DELETE, USER_GROUPS, USER_LIST, USER_LOCK,
    USER_REMOVE_GROUP, USER_UNLOCK,
};
use crate::preview::{Check, LinuxOperation, Outcome, Preview, Step, first_line, run_steps};

/// Reads `/etc/passwd` into name → UID.
fn passwd_uids(
    context: &LinuxContext,
) -> Result<std::collections::BTreeMap<String, u32>, LinuxError> {
    let text = context.fs.read_to_string("/etc/passwd")?;
    let mut accounts = std::collections::BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 4 {
            continue;
        }
        if let Ok(uid) = fields[2].parse::<u32>() {
            accounts.insert(fields[0].to_owned(), uid);
        }
    }
    Ok(accounts)
}

/// The UID of `name`, when the account exists.
fn uid_of(context: &LinuxContext, name: &str) -> Result<Option<u32>, LinuxError> {
    Ok(passwd_uids(context)?.get(name).copied())
}

/// Whether `name`'s password is locked, from `/etc/shadow`.
fn is_locked(context: &LinuxContext, name: &str) -> Result<bool, LinuxError> {
    let text = context.fs.read_to_string("/etc/shadow")?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 2 || fields[0] != name {
            continue;
        }
        return Ok(fields[1].starts_with('!') || fields[1].starts_with('*'));
    }
    Ok(false)
}

/// The groups `name` belongs to, via `id -nG`.
fn read_groups(context: &LinuxContext, name: &str) -> Result<BTreeSet<String>, LinuxError> {
    let request = CommandRequest::new("id").args(["-nG", name]);
    let output = context.runner.run(&request)?;
    if !output.success() {
        return Err(LinuxError::CommandFailed {
            program: "id".to_owned(),
            status: output.status,
            detail: first_line(&output.stderr),
        });
    }
    Ok(output
        .stdout
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

/// Refuses an account below the UID floor unless it is allow-listed.
fn ensure_manageable(context: &LinuxContext, name: &str) -> Result<u32, LinuxError> {
    let uid = uid_of(context, name)?.ok_or_else(|| LinuxError::Unavailable {
        what: format!("account {name}"),
    })?;
    if !context.uid_is_manageable(uid) {
        return Err(LinuxError::ProtectedUid {
            name: name.to_owned(),
            uid,
        });
    }
    Ok(uid)
}

/// `os.user.list`: read the normal accounts.
pub struct UserList {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl UserList {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(USER_LIST),
        }
    }
}

impl LinuxOperation for UserList {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, _args: &Validated) -> Result<Preview, LinuxError> {
        Ok(Preview::new(
            self.op.clone(),
            "list the normal accounts",
            Vec::new(),
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let _ = self.plan(args)?;
        let accounts = passwd_uids(&self.context)?;
        let floor = self.context.uid_floor;
        let normal = accounts.values().filter(|uid| **uid >= floor).count();
        Ok(Outcome::new(
            &self.op,
            0,
            vec![format!("{normal} normal account(s)")],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the account list is readable"))
    }
}

/// `os.user.create`: create an account.
pub struct UserCreate {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl UserCreate {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(USER_CREATE),
        }
    }
}

impl LinuxOperation for UserCreate {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let name = args.account_name("name")?;
        let create_home = args.flag("create_home")?;
        let home = args.optional_text("home")?;
        if let Some(home) = home
            && !crate::args::is_absolute_path(home)
        {
            return Err(LinuxError::InvalidArgument {
                op: self.op.as_str().to_owned(),
                param: "home".to_owned(),
                detail: "not an absolute path".to_owned(),
            });
        }
        let shell = args.optional_text("shell")?;
        if let Some(shell) = shell
            && !crate::args::is_absolute_path(shell)
        {
            return Err(LinuxError::InvalidArgument {
                op: self.op.as_str().to_owned(),
                param: "shell".to_owned(),
                detail: "not an absolute path".to_owned(),
            });
        }
        let groups = args.optional_list_text("groups")?;
        for group in &groups {
            if !crate::args::is_account_name(group) {
                return Err(LinuxError::InvalidArgument {
                    op: self.op.as_str().to_owned(),
                    param: "groups".to_owned(),
                    detail: "a group name is not a valid account name".to_owned(),
                });
            }
        }
        let requested_uid = match args.optional_int("uid")? {
            Some(value) => Some(
                u32::try_from(value).map_err(|_| LinuxError::InvalidArgument {
                    op: self.op.as_str().to_owned(),
                    param: "uid".to_owned(),
                    detail: "not a valid UID".to_owned(),
                })?,
            ),
            None => None,
        };
        if uid_of(&self.context, name)?.is_some() {
            return Ok(Preview::new(
                self.op.clone(),
                format!("account {name} already exists"),
                Vec::new(),
                Vec::new(),
            ));
        }
        if let Some(uid) = requested_uid
            && !self.context.uid_is_manageable(uid)
        {
            return Err(LinuxError::ProtectedUid {
                name: name.to_owned(),
                uid,
            });
        }
        let mut request = CommandRequest::new("useradd");
        request = if create_home {
            request.arg("--create-home")
        } else {
            request.arg("--no-create-home")
        };
        if let Some(home) = home {
            request = request.arg("--home-dir").arg(home);
        }
        if let Some(shell) = shell {
            request = request.arg("--shell").arg(shell);
        }
        if let Some(uid) = requested_uid {
            request = request.arg("--uid").arg(uid.to_string());
        }
        if !groups.is_empty() {
            request = request.arg("--groups").arg(groups.join(","));
        }
        let request = request.arg("--").arg(name);
        Ok(Preview::new(
            self.op.clone(),
            format!("create account {name}"),
            vec![Step::new(format!("create account {name}"), request)],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
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
        let name = args.account_name("name")?;
        if uid_of(&self.context, name)?.is_some() {
            Ok(Check::ok(&self.op, format!("account {name} exists")))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("account {name} does not exist"),
            ))
        }
    }
}

/// `os.user.lock` and `os.user.unlock`: lock or unlock an account's password.
pub struct UserLock {
    context: Arc<LinuxContext>,
    op: OpId,
    lock: bool,
}

impl UserLock {
    /// A module over `context` that locks when `lock`, unlocks otherwise.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>, lock: bool) -> Self {
        Self {
            context,
            op: ids::op_id(if lock { USER_LOCK } else { USER_UNLOCK }),
            lock,
        }
    }
}

impl LinuxOperation for UserLock {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let name = args.account_name("name")?;
        ensure_manageable(&self.context, name)?;
        if is_locked(&self.context, name)? == self.lock {
            return Ok(Preview::new(
                self.op.clone(),
                format!("account {name} is already in the desired lock state"),
                Vec::new(),
                Vec::new(),
            ));
        }
        let verb = if self.lock { "--lock" } else { "--unlock" };
        let request = CommandRequest::new("usermod").arg(verb).arg("--").arg(name);
        Ok(Preview::new(
            self.op.clone(),
            format!(
                "{} account {name}",
                if self.lock { "lock" } else { "unlock" }
            ),
            vec![Step::new(
                if self.lock {
                    format!("lock account {name}")
                } else {
                    format!("unlock account {name}")
                },
                request,
            )],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
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
        let name = args.account_name("name")?;
        if is_locked(&self.context, name)? == self.lock {
            Ok(Check::ok(
                &self.op,
                format!("account {name} is in the desired lock state"),
            ))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("account {name} is not in the desired lock state"),
            ))
        }
    }
}

/// `os.user.delete`: delete an account, optionally removing its home.
pub struct UserDelete {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl UserDelete {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(USER_DELETE),
        }
    }
}

impl LinuxOperation for UserDelete {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let name = args.account_name("name")?;
        let remove_home = args.flag("remove_home")?;
        if uid_of(&self.context, name)?.is_none() {
            return Ok(Preview::new(
                self.op.clone(),
                format!("account {name} does not exist"),
                Vec::new(),
                Vec::new(),
            ));
        }
        ensure_manageable(&self.context, name)?;
        let mut request = CommandRequest::new("userdel");
        if remove_home {
            request = request.arg("--remove");
        }
        let request = request.arg("--").arg(name);
        Ok(Preview::new(
            self.op.clone(),
            format!(
                "delete account {name}{}",
                if remove_home { " and its home" } else { "" }
            ),
            vec![Step::new(format!("delete account {name}"), request)],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
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
        let name = args.account_name("name")?;
        if uid_of(&self.context, name)?.is_none() {
            Ok(Check::ok(&self.op, format!("account {name} is gone")))
        } else {
            Ok(Check::failed(
                &self.op,
                format!("account {name} still exists"),
            ))
        }
    }
}

/// `os.user.groups`: read an account's groups.
pub struct UserGroups {
    context: Arc<LinuxContext>,
    op: OpId,
}

impl UserGroups {
    /// A module over `context`.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>) -> Self {
        Self {
            context,
            op: ids::op_id(USER_GROUPS),
        }
    }
}

impl LinuxOperation for UserGroups {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let name = args.account_name("name")?;
        let request = CommandRequest::new("id").args(["-nG", name]);
        Ok(Preview::new(
            self.op.clone(),
            format!("read the groups of {name}"),
            vec![Step::new(format!("read the groups of {name}"), request)],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
        let preview = self.plan(args)?;
        let applied = run_steps(&*self.context.runner, &preview)?;
        let args = Args::new(args);
        let name = args.account_name("name")?;
        let groups = read_groups(&self.context, name)?;
        Ok(Outcome::new(
            &self.op,
            applied,
            vec![format!("{name} is in {} group(s)", groups.len())],
        ))
    }

    fn verify(&self, _args: &Validated) -> Result<Check, LinuxError> {
        Ok(Check::ok(&self.op, "the group list is readable"))
    }
}

/// `os.user.add_group` and `os.user.remove_group`: change a supplementary membership.
pub struct UserGroupMembership {
    context: Arc<LinuxContext>,
    op: OpId,
    add: bool,
}

impl UserGroupMembership {
    /// A module over `context` that adds when `add`, removes otherwise.
    #[must_use]
    pub fn new(context: Arc<LinuxContext>, add: bool) -> Self {
        Self {
            context,
            op: ids::op_id(if add {
                USER_ADD_GROUP
            } else {
                USER_REMOVE_GROUP
            }),
            add,
        }
    }
}

impl LinuxOperation for UserGroupMembership {
    fn op(&self) -> &OpId {
        &self.op
    }

    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError> {
        let args = Args::new(args);
        let name = args.account_name("name")?;
        let group = args.account_name("group")?;
        ensure_manageable(&self.context, name)?;
        let member = read_groups(&self.context, name)?.contains(group);
        if member == self.add {
            return Ok(Preview::new(
                self.op.clone(),
                format!("{name} is already in the desired group state"),
                Vec::new(),
                Vec::new(),
            ));
        }
        let request = if self.add {
            CommandRequest::new("usermod")
                .args(["--append", "--groups", group])
                .arg("--")
                .arg(name)
        } else {
            CommandRequest::new("gpasswd").args(["--delete", name, group])
        };
        Ok(Preview::new(
            self.op.clone(),
            format!(
                "{} {name} {} {group}",
                if self.add { "add" } else { "remove" },
                if self.add { "to" } else { "from" }
            ),
            vec![Step::new(
                format!(
                    "{} {name} {} {group}",
                    if self.add { "add" } else { "remove" },
                    if self.add { "to" } else { "from" }
                ),
                request,
            )],
            Vec::new(),
        ))
    }

    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError> {
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
        let name = args.account_name("name")?;
        let group = args.account_name("group")?;
        let member = read_groups(&self.context, name)?.contains(group);
        if member == self.add {
            Ok(Check::ok(
                &self.op,
                "the membership is in the desired state",
            ))
        } else {
            Ok(Check::failed(
                &self.op,
                "the membership is not in the desired state",
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

    fn text(value: &str) -> ParamValue {
        ParamValue::Text(value.to_owned())
    }

    #[test]
    fn create_is_idempotent_and_verifies() {
        let system = FakeSystem::fedora();
        let module = UserCreate::new(context(&system));
        let args = args(USER_CREATE, vec![("name", text("alice"))]);
        assert!(!module.plan(&args).unwrap().is_empty());
        module.apply(&args).unwrap();
        assert_eq!(system.snapshot().uid_of("alice"), Some(1000));
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn lock_and_unlock_are_idempotent() {
        let system = FakeSystem::fedora();
        system.add_user("alice", 1000);
        let lock = UserLock::new(context(&system), true);
        let args = args(USER_LOCK, vec![("name", text("alice"))]);
        assert!(!lock.plan(&args).unwrap().is_empty());
        lock.apply(&args).unwrap();
        assert!(system.snapshot().users["alice"].locked);
        assert!(lock.plan(&args).unwrap().is_empty());
        let unlock = UserLock::new(context(&system), false);
        unlock.apply(&args).unwrap();
        assert!(!system.snapshot().users["alice"].locked);
    }

    #[test]
    fn a_system_account_needs_the_allow_list() {
        let system = FakeSystem::fedora();
        let module = UserLock::new(context(&system), true);
        let args = args(USER_LOCK, vec![("name", text("root"))]);
        assert_eq!(
            module.plan(&args).unwrap_err().code(),
            crate::error::PROTECTED_UID
        );
        // The same call is allowed with an explicit allow-list entry.
        let system = FakeSystem::fedora();
        let allowed =
            Arc::new(LinuxContext::new(system.runner(), system.filesystem()).allow_low_uid(0));
        let module = UserLock::new(allowed, true);
        module.apply(&args).unwrap();
        assert!(system.snapshot().users["root"].locked);
    }

    #[test]
    fn group_membership_is_idempotent() {
        let system = FakeSystem::fedora();
        system.add_user("alice", 1000);
        let module = UserGroupMembership::new(context(&system), true);
        let args = args(
            USER_ADD_GROUP,
            vec![("name", text("alice")), ("group", text("wheel"))],
        );
        assert!(!module.plan(&args).unwrap().is_empty());
        module.apply(&args).unwrap();
        assert!(
            read_groups(&context(&system), "alice")
                .unwrap()
                .contains("wheel")
        );
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn delete_is_idempotent() {
        let system = FakeSystem::fedora();
        system.add_user("alice", 1000);
        let module = UserDelete::new(context(&system));
        let args = args(USER_DELETE, vec![("name", text("alice"))]);
        module.apply(&args).unwrap();
        assert_eq!(system.snapshot().uid_of("alice"), None);
        assert!(module.plan(&args).unwrap().is_empty());
    }

    #[test]
    fn list_counts_normal_accounts() {
        let system = FakeSystem::fedora();
        system.add_user("alice", 1000);
        let module = UserList::new(context(&system));
        let outcome = module.apply(&args(USER_LIST, Vec::new())).unwrap();
        assert!(outcome.notes[0].contains("1 normal"));
    }
}
