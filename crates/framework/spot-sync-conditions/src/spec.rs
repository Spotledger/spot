//! The row type of the catalogue table.

use spot_conditions::Severity;

/// One row of the catalogue table, before it becomes a [`crate::CatalogueEntry`].
pub(crate) struct Spec {
    pub(crate) id: &'static str,
    pub(crate) severity: Severity,
    pub(crate) subject: &'static str,
    pub(crate) params: &'static [&'static str],
    /// `(action, attempts)`.
    pub(crate) response: &'static [(&'static str, u32)],
    /// `(elapsed milliseconds, severity)`.
    pub(crate) escalation: &'static [(u64, Severity)],
    pub(crate) actions: &'static [&'static str],
    pub(crate) debounce_ms: u64,
    /// The one remedy to show first: an automatic response action or an operator command of
    /// this entry. `None` keeps the default order (first operator command, else last step).
    pub(crate) primary_remedy: Option<&'static str>,
    pub(crate) reason: &'static str,
}
