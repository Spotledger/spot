//! A task named for a role is acted on by the role's members **now**: the members listed at
//! creation are only shown and notified, so removing someone from the role stops them, and a
//! reassignment makes the one user named an explicit assignee.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use common::spec;
use spot_bpm_tasks::{AssigneeSpec, Directory, Task, access::is_personal_assignee};

/// A role directory whose members the test changes.
struct Roles(Mutex<Vec<String>>);

impl Directory for Roles {
    fn users_in_role(&self, role: &str) -> Vec<String> {
        if role == "approver" {
            self.0.lock().unwrap().clone()
        } else {
            Vec::new()
        }
    }

    fn manager_of(&self, _user: &str) -> Option<String> {
        None
    }
}

fn role_task() -> Task {
    let mut open = spec("t1", "case1", "review", &["ann", "bob"]);
    open.assignee_spec = AssigneeSpec::Role("approver".to_owned());
    open.roles = BTreeSet::from(["approver".to_owned()]);
    Task::open(open)
}

#[test]
fn removing_a_member_from_the_role_stops_them_acting() {
    let directory = Arc::new(Roles(Mutex::new(vec!["ann".to_owned(), "bob".to_owned()])));
    let task = role_task();
    assert!(is_personal_assignee("ann", &task, directory.as_ref()));
    assert!(is_personal_assignee("bob", &task, directory.as_ref()));
    // Ann loses the role: she is still listed in the task, but no longer acts.
    *directory.0.lock().unwrap() = vec!["bob".to_owned()];
    assert!(task.assignees.contains("ann"));
    assert!(!is_personal_assignee("ann", &task, directory.as_ref()));
    assert!(is_personal_assignee("bob", &task, directory.as_ref()));
    // A member added later may act too.
    *directory.0.lock().unwrap() = vec!["bob".to_owned(), "cy".to_owned()];
    assert!(is_personal_assignee("cy", &task, directory.as_ref()));
}

#[test]
fn a_named_user_does_not_depend_on_any_role() {
    let directory = Roles(Mutex::new(Vec::new()));
    let mut open = spec("t2", "case1", "review", &["ann"]);
    open.assignee_spec = AssigneeSpec::User("ann".to_owned());
    let task = Task::open(open);
    assert!(is_personal_assignee("ann", &task, &directory));
    // A reassigned role task (roles cleared) names its user explicitly.
    let mut reassigned = role_task();
    reassigned.roles.clear();
    reassigned.assignees = BTreeSet::from(["dee".to_owned()]);
    assert!(is_personal_assignee("dee", &reassigned, &directory));
}
