//! The registry: every duty and role of a tenant, validated as a whole.
//!
//! A [`Registry`] is immutable. A change builds a new one, validated again from scratch, so an
//! invalid registry cannot exist and the permission engine never sees a half-applied edit.
//!
//! Rules checked here (ADR-070 sections 2 and 3):
//!
//! - role inheritance is **acyclic** and at most [`MAX_DEPTH`] roles deep, with the path named;
//! - every duty and parent a role names exists, and a pinned duty version matches;
//! - a **reference** role is read-only: it enters only through [`Registry::install_pack`], and
//!   no edit replaces or removes it. To change one, copy it ([`crate::copy_role`]);
//! - a grant inside a duty or role names its owner, so a grant cannot be smuggled in under
//!   another role's name.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::{Duty, DutyName, Role, RoleName, check_name};

/// The deepest role inheritance allowed: a role, its parent, and so on, counting the role
/// itself. A role without parents has depth 1.
pub const MAX_DEPTH: usize = 8;

/// Every duty and role of a tenant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    duties: BTreeMap<DutyName, Duty>,
    roles: BTreeMap<RoleName, Role>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    duty: Vec<Duty>,
    #[serde(default)]
    role: Vec<Role>,
}

impl Registry {
    /// Builds a registry, reporting every problem at once.
    pub fn new(duties: Vec<Duty>, roles: Vec<Role>) -> Result<Registry> {
        let mut problems = Vec::new();
        let mut duty_map = BTreeMap::new();
        for duty in duties {
            if duty_map.contains_key(&duty.name) {
                problems.push(IamError::about(
                    e::DUPLICATE_NAME,
                    duty.name.as_str(),
                    "two duties have this name",
                ));
            }
            duty_map.entry(duty.name.clone()).or_insert(duty);
        }
        let mut role_map = BTreeMap::new();
        for role in roles {
            if role_map.contains_key(&role.name) {
                problems.push(IamError::about(
                    e::DUPLICATE_NAME,
                    role.name.as_str(),
                    "two roles have this name",
                ));
            }
            role_map.entry(role.name.clone()).or_insert(role);
        }
        let registry = Registry {
            duties: duty_map,
            roles: role_map,
        };
        problems.extend(registry.problems());
        match IamErrors::from_list(problems) {
            Some(errors) => Err(errors),
            None => Ok(registry),
        }
    }

    /// Loads a registry from TOML: `[[duty]]` and `[[role]]` tables.
    pub fn from_toml(text: &str) -> Result<Registry> {
        let file: File = toml::from_str(text).map_err(|err| {
            IamErrors::one(IamError::new(
                e::INVALID_NAME,
                format!(
                    "the registry file is not valid: {}",
                    first_line(&err.to_string())
                ),
            ))
        })?;
        Registry::new(file.duty, file.role)
    }

    /// The duty with this name.
    pub fn duty(&self, name: &DutyName) -> Option<&Duty> {
        self.duties.get(name)
    }

    /// The role with this name.
    pub fn role(&self, name: &RoleName) -> Option<&Role> {
        self.roles.get(name)
    }

    /// Every duty, in name order.
    pub fn duties(&self) -> impl Iterator<Item = &Duty> {
        self.duties.values()
    }

    /// Every role, in name order.
    pub fn roles(&self) -> impl Iterator<Item = &Role> {
        self.roles.values()
    }

    /// A registry with this custom role added or replaced. A reference role is refused, both as
    /// the role being replaced and as the new one: reference roles come only from a pack.
    pub fn with_role(&self, role: Role) -> Result<Registry> {
        let replaces_reference = self.roles.get(&role.name).is_some_and(Role::is_reference);
        if role.is_reference() || replaces_reference {
            return Err(IamErrors::one(IamError::about(
                e::REFERENCE_READONLY,
                role.name.as_str(),
                "a reference role is read-only and comes only from a pack; copy it to make a custom role",
            )));
        }
        let mut next = self.clone();
        next.roles.insert(role.name.clone(), role);
        next.checked()
    }

    /// A registry without this role. A reference role is refused; so is a role that another
    /// role still inherits from (the result would not validate).
    pub fn without_role(&self, name: &RoleName) -> Result<Registry> {
        if self.roles.get(name).is_some_and(Role::is_reference) {
            return Err(IamErrors::one(IamError::about(
                e::REFERENCE_READONLY,
                name.as_str(),
                "a reference role is read-only; it goes away only with its pack",
            )));
        }
        let mut next = self.clone();
        next.roles.remove(name);
        next.checked()
    }

    /// A registry with a pack's duties and reference roles installed: duties are replaced by
    /// name, and a reference role of the same name is replaced. A custom role of the same name
    /// is a conflict and refused. Copies made from the old reference roles are not touched.
    pub fn install_pack(&self, duties: Vec<Duty>, roles: Vec<Role>) -> Result<Registry> {
        let mut problems = Vec::new();
        let mut next = self.clone();
        for duty in duties {
            next.duties.insert(duty.name.clone(), duty);
        }
        for role in roles {
            if !role.is_reference() {
                problems.push(IamError::about(
                    e::REFERENCE_READONLY,
                    role.name.as_str(),
                    "a pack holds reference roles only",
                ));
                continue;
            }
            if self
                .roles
                .get(&role.name)
                .is_some_and(|r| !r.is_reference())
            {
                problems.push(IamError::about(
                    e::DUPLICATE_NAME,
                    role.name.as_str(),
                    "a custom role already has the name of this reference role",
                ));
                continue;
            }
            next.roles.insert(role.name.clone(), role);
        }
        if let Some(errors) = IamErrors::from_list(problems) {
            return Err(errors);
        }
        next.checked()
    }

    fn checked(self) -> Result<Registry> {
        match IamErrors::from_list(self.problems()) {
            Some(errors) => Err(errors),
            None => Ok(self),
        }
    }

    /// Every problem of the registry as it stands.
    fn problems(&self) -> Vec<IamError> {
        let mut problems = Vec::new();
        for duty in self.duties.values() {
            check_name(duty.name.as_str(), duty.name.as_str(), &mut problems);
            for grant in &duty.grants {
                if grant.role.as_str() != duty.name.as_str() {
                    problems.push(grant_role(duty.name.as_str()));
                }
            }
            for grant in &duty.field_grants {
                if grant.role.as_str() != duty.name.as_str() {
                    problems.push(grant_role(duty.name.as_str()));
                }
            }
        }
        for role in self.roles.values() {
            let name = role.name.as_str();
            check_name(name, name, &mut problems);
            for grant in &role.direct_grants {
                if grant.role.as_str() != name {
                    problems.push(grant_role(name));
                }
            }
            for grant in &role.direct_field_grants {
                if grant.role.as_str() != name {
                    problems.push(grant_role(name));
                }
            }
            for used in &role.duties {
                match self.duties.get(&used.name) {
                    None => problems.push(IamError::about(
                        e::UNKNOWN_DUTY,
                        name,
                        format!("includes the unknown duty `{}`", used.name.as_str()),
                    )),
                    Some(duty) => {
                        if let Some(pinned) = used.version
                            && pinned != duty.version
                        {
                            problems.push(IamError::about(
                                e::DUTY_VERSION,
                                name,
                                format!(
                                    "pins duty `{}` at version {} but the registry has {}",
                                    used.name.as_str(),
                                    pinned.0,
                                    duty.version.0
                                ),
                            ));
                        }
                    }
                }
            }
            for parent in &role.parents {
                if !self.roles.contains_key(parent) {
                    problems.push(IamError::about(
                        e::UNKNOWN_PARENT,
                        name,
                        format!("inherits from the unknown role `{}`", parent.as_str()),
                    ));
                }
            }
        }
        problems.extend(self.inheritance_problems());
        problems
    }

    /// Cycles (with their path) and depth.
    fn inheritance_problems(&self) -> Vec<IamError> {
        let mut problems = Vec::new();
        let mut cycles: BTreeSet<Vec<&RoleName>> = BTreeSet::new();
        let mut done: BTreeSet<&RoleName> = BTreeSet::new();
        for start in self.roles.keys() {
            let mut stack: Vec<&RoleName> = Vec::new();
            self.find_cycles(start, &mut stack, &mut done, &mut cycles);
        }
        let in_cycle: BTreeSet<&RoleName> = cycles.iter().flatten().copied().collect();
        for cycle in &cycles {
            let mut path: Vec<&str> = cycle.iter().map(|r| r.as_str()).collect();
            if let Some(first) = path.first().copied() {
                path.push(first);
            }
            problems.push(IamError::about(
                e::ROLE_CYCLE,
                cycle.first().map_or("", |r| r.as_str()),
                format!("role inheritance is a cycle: {}", path.join(" -> ")),
            ));
        }
        // Depth, over the roles that are not part of or below a cycle.
        let mut depth: BTreeMap<&RoleName, (usize, Option<&RoleName>)> = BTreeMap::new();
        for name in self.roles.keys() {
            self.depth_of(name, &in_cycle, &mut depth, &mut Vec::new());
        }
        for (name, (d, _)) in &depth {
            if *d == MAX_DEPTH + 1 {
                let mut chain = vec![name.as_str()];
                let mut at = *name;
                while let Some((_, Some(parent))) = depth.get(at) {
                    chain.push(parent.as_str());
                    at = parent;
                }
                problems.push(IamError::about(
                    e::ROLE_DEPTH,
                    name.as_str(),
                    format!(
                        "role inheritance is deeper than {MAX_DEPTH}: {}",
                        chain.join(" -> ")
                    ),
                ));
            }
        }
        problems
    }

    fn find_cycles<'a>(
        &'a self,
        at: &'a RoleName,
        stack: &mut Vec<&'a RoleName>,
        done: &mut BTreeSet<&'a RoleName>,
        cycles: &mut BTreeSet<Vec<&'a RoleName>>,
    ) {
        if done.contains(at) {
            return;
        }
        if let Some(pos) = stack.iter().position(|r| *r == at) {
            let mut cycle: Vec<&RoleName> = stack.get(pos..).unwrap_or_default().to_vec();
            // Start the cycle at its smallest name, so the same cycle is reported once.
            if let Some(min) = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| **r)
                .map(|(i, _)| i)
            {
                cycle.rotate_left(min);
            }
            cycles.insert(cycle);
            return;
        }
        let Some(role) = self.roles.get(at) else {
            return;
        };
        stack.push(at);
        for parent in &role.parents {
            if let Some((key, _)) = self.roles.get_key_value(parent) {
                self.find_cycles(key, stack, done, cycles);
            }
        }
        stack.pop();
        done.insert(at);
    }

    /// The depth of `at` (itself counted) and the parent on its deepest chain. A role in or
    /// above a cycle is left out.
    fn depth_of<'a>(
        &'a self,
        at: &'a RoleName,
        in_cycle: &BTreeSet<&'a RoleName>,
        memo: &mut BTreeMap<&'a RoleName, (usize, Option<&'a RoleName>)>,
        visiting: &mut Vec<&'a RoleName>,
    ) -> Option<usize> {
        if let Some((d, _)) = memo.get(at) {
            return Some(*d);
        }
        if in_cycle.contains(at) || visiting.contains(&at) {
            return None;
        }
        let role = self.roles.get(at)?;
        visiting.push(at);
        let mut best: (usize, Option<&RoleName>) = (0, None);
        for parent in &role.parents {
            if let Some((key, _)) = self.roles.get_key_value(parent)
                && let Some(d) = self.depth_of(key, in_cycle, memo, visiting)
                && d > best.0
            {
                best = (d, Some(key));
            }
        }
        visiting.pop();
        memo.insert(at, (best.0 + 1, best.1));
        Some(best.0 + 1)
    }
}

fn grant_role(owner: &str) -> IamError {
    IamError::about(
        e::GRANT_ROLE,
        owner,
        "a grant must name its owner as its role",
    )
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

/// Checks assignments against a registry: unknown roles, duplicates and bad windows, every
/// problem at once.
pub fn validate_assignments(
    registry: &Registry,
    assignments: &[crate::model::Assignment],
) -> Result<()> {
    let mut problems = Vec::new();
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    let mut shapes: Vec<&crate::model::Assignment> = Vec::new();
    for a in assignments {
        if let Err(errors) = a.validate() {
            problems.extend(errors.errors().iter().cloned());
        }
        if !ids.insert(a.id.as_str()) {
            problems.push(IamError::about(
                e::DUPLICATE_ASSIGNMENT,
                &a.id,
                "two assignments have this id",
            ));
        }
        if registry.role(&a.role).is_none() {
            problems.push(IamError::about(
                e::UNKNOWN_ROLE,
                &a.id,
                format!("gives the unknown role `{}`", a.role.as_str()),
            ));
        }
        let same = shapes.iter().any(|b| {
            b.id != a.id
                && b.user == a.user
                && b.role == a.role
                && b.scope == a.scope
                && b.valid_from == a.valid_from
                && b.valid_to == a.valid_to
        });
        if same {
            problems.push(IamError::about(
                e::DUPLICATE_ASSIGNMENT,
                &a.id,
                "another assignment gives the same user the same role with the same scope and validity",
            ));
        }
        shapes.push(a);
    }
    match IamErrors::from_list(problems) {
        Some(errors) => Err(errors),
        None => Ok(()),
    }
}
