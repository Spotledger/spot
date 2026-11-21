//! Windows Update: search, list pending, install by id or severity, and reboot state.
//!
//! The module never reboots. `os.update.install` downloads and installs the selected
//! updates and reports whether the machine is now waiting for a reboot;
//! `os.update.schedule_reboot` is the only way to arrange one, at an absolute instant, and
//! `os.update.reboot_status` reports the flag. An update that is already installed plans as
//! "no change", so applying twice installs nothing the second time.

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, OperationHandler};
use spot_mgmt_ops::Validated;

use crate::api::{Severity, UpdateInfo, WinApi};
use crate::args;
use crate::catalogue::{
    UPDATE_INSTALL, UPDATE_PENDING, UPDATE_REBOOT_STATUS, UPDATE_SCHEDULE_REBOOT, UPDATE_SEARCH,
};
use crate::error::WindowsError;
use crate::handlers::handler_outcome;
use crate::plan::{Change, Check, Outcome, Preview};

/// The Windows Update module.
pub struct Updates {
    api: Arc<dyn WinApi>,
}

impl Updates {
    /// An updates module over `api`.
    #[must_use]
    pub fn new(api: Arc<dyn WinApi>) -> Self {
        Self { api }
    }

    /// Every update the agent offers.
    ///
    /// # Errors
    /// [`WindowsError::Api`], [`WindowsError::Unsupported`].
    pub fn search(&self, include_hidden: bool) -> Result<Vec<UpdateInfo>, WindowsError> {
        self.api.search_updates(include_hidden)
    }

    /// The offered updates that are not installed yet.
    ///
    /// # Errors
    /// [`WindowsError::Api`], [`WindowsError::Unsupported`].
    pub fn pending(&self) -> Result<Vec<UpdateInfo>, WindowsError> {
        let mut updates: Vec<UpdateInfo> = self
            .api
            .search_updates(false)?
            .into_iter()
            .filter(|update| !update.installed)
            .collect();
        updates.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(updates)
    }

    /// Whether the machine is waiting for a reboot.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    pub fn reboot_required(&self) -> Result<bool, WindowsError> {
        self.api.reboot_required()
    }

    /// Plans the operation in `args` without touching the machine.
    ///
    /// # Errors
    /// [`WindowsError::UpdateSelection`] when the selection is empty or ambiguous,
    /// [`WindowsError::UnknownUpdate`] for an unknown id, [`WindowsError::RebootTimeInvalid`]
    /// for a bad instant, or any read failure.
    pub fn plan(&self, args: &Validated) -> Result<Preview, WindowsError> {
        let op = args.op.clone();
        match args.op.as_str() {
            UPDATE_SEARCH => {
                let include_hidden = args::boolean(args, "include_hidden")?;
                self.api.search_updates(include_hidden)?;
                Ok(Preview::no_change(op))
            }
            UPDATE_PENDING => {
                self.pending()?;
                Ok(Preview::no_change(op))
            }
            UPDATE_REBOOT_STATUS => {
                self.api.reboot_required()?;
                Ok(Preview::no_change(op))
            }
            UPDATE_INSTALL => {
                let selected = self.selection(args)?;
                let pending: Vec<&UpdateInfo> =
                    selected.iter().filter(|update| !update.installed).collect();
                if pending.is_empty() {
                    return Ok(Preview::no_change(op));
                }
                let changes = pending
                    .iter()
                    .map(|update| {
                        Change::new(
                            format!("update:{}", update.id),
                            "install",
                            format!("install {}", describe(update)),
                        )
                    })
                    .collect();
                Ok(Preview::with_changes(op, changes))
            }
            UPDATE_SCHEDULE_REBOOT => {
                let at = reboot_instant(args)?;
                if self.api.scheduled_reboot()? == Some(at) {
                    return Ok(Preview::no_change(op));
                }
                Ok(Preview::with_changes(
                    op,
                    vec![Change::new(
                        "reboot",
                        "schedule",
                        format!("schedule a reboot at {at}"),
                    )],
                ))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("update operation {other}"),
            }),
        }
    }

    /// Applies the plan `preview`; a plan with no changes does nothing.
    ///
    /// # Errors
    /// Any download or install failure.
    pub fn apply(&self, args: &Validated, preview: &Preview) -> Result<Outcome, WindowsError> {
        let op = args.op.clone();
        if preview.is_no_change() {
            return Ok(Outcome::already_current(op));
        }
        match args.op.as_str() {
            UPDATE_INSTALL => {
                for update in self.selection(args)? {
                    if update.installed {
                        continue;
                    }
                    self.api.download_update(&update.id)?;
                    self.api.install_update(&update.id)?;
                }
                let reboot_required = self.api.reboot_required()?;
                Ok(Outcome::applied(
                    op,
                    preview.changes.clone(),
                    reboot_required,
                ))
            }
            UPDATE_SCHEDULE_REBOOT => {
                self.api.schedule_reboot(reboot_instant(args)?)?;
                Ok(Outcome::applied(op, preview.changes.clone(), false))
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("update operation {other}"),
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
            UPDATE_SEARCH => {
                let count = self
                    .api
                    .search_updates(args::boolean(args, "include_hidden")?)?
                    .len();
                Ok(Check::satisfied(op, format!("{count} updates offered")))
            }
            UPDATE_PENDING => {
                let count = self.pending()?.len();
                Ok(Check::satisfied(op, format!("{count} pending updates")))
            }
            UPDATE_REBOOT_STATUS => {
                let required = self.api.reboot_required()?;
                Ok(Check::satisfied(
                    op,
                    if required {
                        "a reboot is required".to_owned()
                    } else {
                        "no reboot is required".to_owned()
                    },
                ))
            }
            UPDATE_INSTALL => {
                let selected = self.selection(args)?;
                let missing: Vec<&str> = selected
                    .iter()
                    .filter(|update| !update.installed)
                    .map(|update| update.id.as_str())
                    .collect();
                if missing.is_empty() {
                    Ok(Check::satisfied(op, "every selected update is installed"))
                } else {
                    Ok(Check::unsatisfied(
                        op,
                        format!("{} selected updates are not installed", missing.len()),
                    ))
                }
            }
            UPDATE_SCHEDULE_REBOOT => {
                let at = reboot_instant(args)?;
                if self.api.scheduled_reboot()? == Some(at) {
                    Ok(Check::satisfied(
                        op,
                        format!("a reboot is scheduled at {at}"),
                    ))
                } else {
                    Ok(Check::unsatisfied(op, "no reboot is scheduled".to_owned()))
                }
            }
            other => Err(WindowsError::Unsupported {
                feature: format!("update operation {other}"),
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

    /// Resolves the `ids`/`severity` selection into the offered updates it names.
    fn selection(&self, args: &Validated) -> Result<Vec<UpdateInfo>, WindowsError> {
        let ids = args::text_list(args, "ids")?;
        let severity = args::optional_choice(args, "severity")?;
        let offered = self.api.search_updates(true)?;
        match (ids.is_empty(), severity) {
            (true, None) => Err(WindowsError::UpdateSelection {
                detail: "provide either ids or a severity".to_owned(),
            }),
            (false, Some(_)) => Err(WindowsError::UpdateSelection {
                detail: "provide ids or a severity, not both".to_owned(),
            }),
            (false, None) => {
                let mut selected = Vec::with_capacity(ids.len());
                for id in &ids {
                    let update = offered
                        .iter()
                        .find(|update| &update.id == id)
                        .ok_or_else(|| WindowsError::UnknownUpdate { id: id.clone() })?;
                    selected.push(update.clone());
                }
                Ok(selected)
            }
            (true, Some(name)) => {
                let wanted =
                    Severity::parse(name).ok_or_else(|| WindowsError::InvalidArgument {
                        param: "severity".to_owned(),
                        detail: "unknown severity".to_owned(),
                    })?;
                Ok(offered
                    .into_iter()
                    .filter(|update| update.severity == wanted)
                    .collect())
            }
        }
    }
}

impl OperationHandler for Updates {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        handler_outcome(self.execute(plan.args()))
    }
}

/// A short, secret-free description of an update, with its KB and severity.
fn describe(update: &UpdateInfo) -> String {
    match &update.kb {
        Some(kb) => format!("{kb} ({}), {}", update.severity.as_str(), update.title),
        None => format!("{} ({})", update.severity.as_str(), update.title),
    }
}

/// Reads and checks the `at` argument of a scheduled reboot.
fn reboot_instant(args: &Validated) -> Result<i64, WindowsError> {
    let at = args::integer(args, "at")?;
    if at < 1 {
        return Err(WindowsError::RebootTimeInvalid { at });
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Validated};

    use crate::fake::FakeWin;

    use super::*;

    fn module() -> (Arc<FakeWin>, Updates) {
        let api = Arc::new(FakeWin::with_stock_updates());
        let updates = Updates::new(api.clone());
        (api, updates)
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
    fn install_by_severity_selects_only_pending_updates() {
        let (_api, updates) = module();
        let preview = updates
            .plan(&args(
                UPDATE_INSTALL,
                vec![("severity", ParamValue::Text("critical".to_owned()))],
            ))
            .unwrap();
        assert_eq!(preview.changes.len(), 1);
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let (_api, updates) = module();
        let error = updates.plan(&args(UPDATE_INSTALL, Vec::new())).unwrap_err();
        assert_eq!(error.code(), crate::error::UPDATE_SELECTION);
    }

    #[test]
    fn a_past_instant_is_refused() {
        let (_api, updates) = module();
        let error = updates
            .plan(&args(
                UPDATE_SCHEDULE_REBOOT,
                vec![("at", ParamValue::Int(0))],
            ))
            .unwrap_err();
        assert_eq!(error.code(), crate::error::REBOOT_TIME_INVALID);
    }
}
