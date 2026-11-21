//! Inbox filtering: who may act on, see and supervise a task.
//!
//! **Acting** (claim, complete, delegate) is for the assignee set only: an assignee, a member of
//! an assignee role, or an active delegate. **Seeing** a task in a listing is wider:
//! [`PermissionAccess`] also lets a user see a task whose underlying document the
//! `spot-permissions` engine lets them read, judged on the document's real state and values
//! (a [`DocFactsSource`]); when the facts cannot be loaded the answer is no. **Supervising** is
//! for a direct manager of an assignee. Reading a document never lets anyone act on its task.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};

use crate::resolve::Directory;
use crate::task::Task;

/// A boxed, `Send` future, so [`DocFactsSource`] and [`InboxAccess::may_see`] stay object-safe.
pub type AccessFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whether a user may act on, see or supervise a task.
pub trait InboxAccess: Send + Sync {
    /// Whether `user` may act on `task`: claim, complete, delegate. Only the assignee set may.
    fn may_act(&self, user: &str, task: &Task) -> bool;

    /// Whether `user` may see `task` in their inbox listing. Everyone who may act may see; an
    /// implementation may let more people see, never act. The default is `may_act`.
    ///
    /// Async because seeing may need the subject document's facts from a service. Dropping the
    /// future cancels the check and leaves no state behind.
    fn may_see<'a>(&'a self, user: &'a str, task: &'a Task) -> AccessFuture<'a, bool> {
        Box::pin(async move { self.may_act(user, task) })
    }

    /// Whether `user` may supervise `task`: see it in the team inbox, reassign it.
    fn may_supervise(&self, user: &str, task: &Task) -> bool;
}

/// Whether `user` is an assignee, a member of an assignee role, or a delegate.
pub fn is_personal_assignee(user: &str, task: &Task, directory: &dyn Directory) -> bool {
    if task.is_explicit_assignee(user) {
        return true;
    }
    if task.roles.iter().any(|role| {
        directory
            .users_in_role(role)
            .iter()
            .any(|member| member == user)
    }) {
        return true;
    }
    task.delegation
        .as_ref()
        .is_some_and(|delegation| delegation.to == user)
}

/// Whether `user` directly manages one of the task's assignees.
pub fn is_direct_manager(user: &str, task: &Task, directory: &dyn Directory) -> bool {
    task.assignees
        .iter()
        .any(|assignee| directory.manager_of(assignee).as_deref() == Some(user))
}

/// The reference [`InboxAccess`]: the assignee set, the roles and the directory, and no
/// permission engine.
pub struct AssigneeAccess {
    directory: Arc<dyn Directory>,
}

impl AssigneeAccess {
    /// Builds the access rule over a directory.
    #[must_use]
    pub fn new(directory: Arc<dyn Directory>) -> Self {
        Self { directory }
    }
}

impl InboxAccess for AssigneeAccess {
    fn may_act(&self, user: &str, task: &Task) -> bool {
        is_personal_assignee(user, task, self.directory.as_ref())
    }

    fn may_supervise(&self, user: &str, task: &Task) -> bool {
        is_direct_manager(user, task, self.directory.as_ref())
    }
}

/// The principal (the user's assignments) a permission decision is asked for.
pub trait PrincipalSource: Send + Sync {
    /// The principal for `user`.
    fn principal(&self, user: &str) -> Principal;
}

/// An in-memory [`PrincipalSource`]: users to the role names their assignments hold.
#[derive(Debug, Clone, Default)]
pub struct MapPrincipals {
    roles: BTreeMap<String, Vec<String>>,
}

impl MapPrincipals {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the roles `user`'s assignments hold.
    #[must_use]
    pub fn with_roles(mut self, user: &str, roles: &[&str]) -> Self {
        self.roles.insert(
            user.to_owned(),
            roles.iter().map(|r| (*r).to_owned()).collect(),
        );
        self
    }
}

impl PrincipalSource for MapPrincipals {
    fn principal(&self, user: &str) -> Principal {
        let roles = self.roles.get(user).cloned().unwrap_or_default();
        let refs: Vec<&str> = roles.iter().map(String::as_str).collect();
        Principal::with_roles(user, &refs)
    }
}

/// Where the permission check gets the real state and values of a task's subject document.
pub trait DocFactsSource: Send + Sync {
    /// The facts of `task`'s subject document (its DocType, id, creator, lifecycle state and
    /// field values), or `None` when they cannot be loaded. `None` means deny. The future never
    /// blocks a thread; a host that wants a deadline wraps it in its runtime's timeout and treats
    /// the expiry as `None`.
    fn facts<'a>(&'a self, task: &'a Task) -> AccessFuture<'a, Option<DocFacts>>;
}

/// An in-memory [`DocFactsSource`]: documents keyed by the task's subject id.
#[derive(Debug, Clone, Default)]
pub struct MapDocFacts {
    docs: BTreeMap<String, DocFacts>,
}

impl MapDocFacts {
    /// An empty source: every lookup fails.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the facts of the document with record id `subject`.
    #[must_use]
    pub fn with_document(mut self, subject: &str, facts: DocFacts) -> Self {
        self.docs.insert(subject.to_owned(), facts);
        self
    }
}

impl DocFactsSource for MapDocFacts {
    fn facts<'a>(&'a self, task: &'a Task) -> AccessFuture<'a, Option<DocFacts>> {
        Box::pin(async move { self.docs.get(task.subject.as_deref()?).cloned() })
    }
}

/// [`InboxAccess`] that also consults `spot-permissions`: only an assignee may act, and a user
/// may *see* a task when they may act on it or the permission engine grants them `read` on the
/// underlying document, judged on the document's real facts.
pub struct PermissionAccess {
    engine: Arc<PermissionEngine>,
    principals: Arc<dyn PrincipalSource>,
    directory: Arc<dyn Directory>,
    facts: Arc<dyn DocFactsSource>,
}

impl PermissionAccess {
    /// Builds the access rule over the permission engine, a principal source, a directory and
    /// the source of the subject documents' facts.
    #[must_use]
    pub fn new(
        engine: Arc<PermissionEngine>,
        principals: Arc<dyn PrincipalSource>,
        directory: Arc<dyn Directory>,
        facts: Arc<dyn DocFactsSource>,
    ) -> Self {
        Self {
            engine,
            principals,
            directory,
            facts,
        }
    }
}

impl InboxAccess for PermissionAccess {
    fn may_act(&self, user: &str, task: &Task) -> bool {
        is_personal_assignee(user, task, self.directory.as_ref())
    }

    fn may_see<'a>(&'a self, user: &'a str, task: &'a Task) -> AccessFuture<'a, bool> {
        Box::pin(async move {
            if self.may_act(user, task) {
                return true;
            }
            let Some(doctype) = &task.doctype else {
                return false;
            };
            // No facts, or facts of some other document: deny.
            let Some(facts) = self.facts.facts(task).await else {
                return false;
            };
            if &facts.doctype != doctype || facts.id != task.subject {
                return false;
            }
            let principal = self.principals.principal(user);
            self.engine.can(&principal, Action::Read, &facts).allowed
        })
    }

    fn may_supervise(&self, user: &str, task: &Task) -> bool {
        is_direct_manager(user, task, self.directory.as_ref())
    }
}
