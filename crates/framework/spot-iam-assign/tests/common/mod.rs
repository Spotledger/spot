#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde::Deserialize;
use spot_iam_assign::{
    Assignment, Facts, IdSource, RoleDirectory, RoleName, RuleSet, RuleSpec, UserId,
};
use time::OffsetDateTime;
use time::macros::datetime;

/// A counting id source: `id-0001`, `id-0002`, ... so a plan is deterministic in tests.
#[derive(Debug, Default)]
pub struct CountingIds {
    next: u64,
}

impl CountingIds {
    /// A source whose first id is `id-0001`.
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdSource for CountingIds {
    fn next(&mut self) -> String {
        self.next += 1;
        format!("id-{:04}", self.next)
    }
}

/// A fresh counting source.
pub fn counting_ids() -> CountingIds {
    CountingIds::new()
}

/// An id source that hands out these ids in order, so a test can name them.
#[derive(Debug)]
pub struct ScriptedIds {
    queued: std::collections::VecDeque<String>,
}

impl ScriptedIds {
    /// A source that gives `ids` in order and panics if asked for more.
    pub fn new(ids: &[&str]) -> Self {
        Self {
            queued: ids.iter().map(|id| (*id).to_owned()).collect(),
        }
    }
}

impl IdSource for ScriptedIds {
    fn next(&mut self) -> String {
        self.queued
            .pop_front()
            .expect("the plan asked for more ids than the test scripted")
    }
}

#[derive(Deserialize)]
pub struct UserFixture {
    pub id: String,
    pub facts: Facts,
}

#[derive(Deserialize)]
pub struct Fixture {
    pub rule: Vec<RuleSpec>,
    pub user: Vec<UserFixture>,
}

pub fn fixture() -> Fixture {
    toml::from_str(include_str!("../fixtures/rules.toml")).unwrap()
}

pub fn roles() -> RoleDirectory {
    RoleDirectory::new(
        [
            "Sales User",
            "Accountant",
            "Auditor",
            "Manager",
            "Controller",
            "Break Glass",
        ]
        .map(RoleName::from),
    )
    .with_emergency_only([RoleName::from("Break Glass")])
}

pub fn rule_set() -> RuleSet {
    RuleSet::load(fixture().rule, &roles()).unwrap()
}

pub fn users() -> Vec<(UserId, Facts)> {
    fixture()
        .user
        .into_iter()
        .map(|u| (u.id, u.facts))
        .collect()
}

pub const T0: OffsetDateTime = datetime!(2026-10-12 08:00 UTC);

pub fn later(hours: i64) -> OffsetDateTime {
    T0 + time::Duration::hours(hours)
}

/// Plans, applies and returns the new state.
pub fn settle(plan: &spot_iam_assign::Plan, current: &[Assignment]) -> Vec<Assignment> {
    plan.apply(current)
}

pub fn live_roles(current: &[Assignment], user: &str, now: OffsetDateTime) -> Vec<String> {
    let mut out: Vec<String> = current
        .iter()
        .filter(|a| a.user == user && a.valid_to.is_none_or(|t| t > now))
        .map(|a| a.role.as_str().to_owned())
        .collect();
    out.sort();
    out
}
