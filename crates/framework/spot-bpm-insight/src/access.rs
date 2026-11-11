//! Process-level read permission for aggregates.
//!
//! Aggregates are a read of a process's runs, so they honour the viewer's process-level read
//! permission (WP-PLT-03 owns the checks). This crate is pure and holds no IAM model: the host
//! passes a [`ProcessAccess`] predicate, and a process the viewer may not read is **absent** from
//! the result — never a row of zeros, which would leak that the process exists and has runs.
//!
//! [`ReadAll`], [`ReadNone`] and [`AllowList`] are the small implementations the tests and a
//! simple host use; a real host implements [`ProcessAccess`] over its permission engine.

use std::collections::BTreeSet;

use crate::process::ProcessKey;

/// Whether a viewer may read the runs of a process version.
pub trait ProcessAccess {
    /// Whether the viewer may read `process`'s runs.
    fn may_read(&self, process: &ProcessKey) -> bool;
}

/// A viewer that may read every process (a system principal, or a test).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadAll;

impl ProcessAccess for ReadAll {
    fn may_read(&self, _process: &ProcessKey) -> bool {
        true
    }
}

/// A viewer that may read no process at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadNone;

impl ProcessAccess for ReadNone {
    fn may_read(&self, _process: &ProcessKey) -> bool {
        false
    }
}

/// A viewer that may read exactly the listed process versions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowList {
    allowed: BTreeSet<ProcessKey>,
}

impl AllowList {
    /// An empty allow-list: the viewer may read nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one readable process version.
    pub fn insert(&mut self, process: ProcessKey) -> &mut Self {
        self.allowed.insert(process);
        self
    }

    /// The readable process versions.
    #[must_use]
    pub fn allowed(&self) -> &BTreeSet<ProcessKey> {
        &self.allowed
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }
}

impl ProcessAccess for AllowList {
    fn may_read(&self, process: &ProcessKey) -> bool {
        self.allowed.contains(process)
    }
}

impl<T: ProcessAccess + ?Sized> ProcessAccess for &T {
    fn may_read(&self, process: &ProcessKey) -> bool {
        (**self).may_read(process)
    }
}

impl<T: ProcessAccess + ?Sized> ProcessAccess for Box<T> {
    fn may_read(&self, process: &ProcessKey) -> bool {
        (**self).may_read(process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allow_list_admits_only_its_processes() {
        let mut access = AllowList::new();
        access.insert(ProcessKey::new("order_flow", 1));
        assert!(access.may_read(&ProcessKey::new("order_flow", 1)));
        assert!(!access.may_read(&ProcessKey::new("order_flow", 2)));
        assert!(!access.may_read(&ProcessKey::new("other", 1)));
        assert!(!ReadNone.may_read(&ProcessKey::new("order_flow", 1)));
        assert!(ReadAll.may_read(&ProcessKey::new("other", 9)));
    }
}
