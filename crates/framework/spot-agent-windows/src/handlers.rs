//! Wiring the modules into a [`HandlerRegistry`].
//!
//! [`register`] builds one instance of each module over the shared seam and registers it
//! under every catalogue id it owns. The executor then runs only these handlers; an id
//! with no handler is refused by [`spot_agent_core`] itself. [`handler_outcome`] is the
//! one place a typed [`WindowsError`] becomes a [`HandlerOutcome::Failed`], and it reports
//! the stable code with the message so the console can match on it.

use std::sync::Arc;

use spot_agent_core::{HandlerOutcome, HandlerRegistry};
use spot_mgmt_ops::OpId;

use crate::api::WinApi;
use crate::catalogue::{
    FIREWALL_ADD_RULE, FIREWALL_LIST, FIREWALL_REMOVE_RULE, FIREWALL_SET_PROFILE, GROUP_ADD_MEMBER,
    GROUP_LIST, GROUP_REMOVE_MEMBER, SERVICE_LIST, SERVICE_RESTART, SERVICE_SET_RECOVERY,
    SERVICE_SET_START_TYPE, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, UPDATE_INSTALL,
    UPDATE_PENDING, UPDATE_REBOOT_STATUS, UPDATE_SCHEDULE_REBOOT, UPDATE_SEARCH, USER_CREATE,
    USER_DISABLE, USER_LIST, USER_RESET_PASSWORD, USER_SET_GROUPS,
};
use crate::error::WindowsError;
use crate::firewall::Firewall;
use crate::guards::AgentConfig;
use crate::plan::Outcome;
use crate::secrets::{PasswordSource, SealedResultChannel};
use crate::services::Services;
use crate::updates::Updates;
use crate::users::Users;

/// Turns a module result into a handler outcome. A failure carries the stable code and the
/// message, neither of which ever holds a secret.
#[must_use]
pub fn handler_outcome(result: Result<Outcome, WindowsError>) -> HandlerOutcome {
    match result {
        Ok(_) => HandlerOutcome::Succeeded,
        Err(error) => HandlerOutcome::Failed {
            detail: format!("{}: {error}", error.code()),
        },
    }
}

/// Registers every Windows module under its catalogue ids.
///
/// The registry ends up with one handler per id from [`crate::catalogue`], so the executor
/// can run any Windows operation and refuses nothing the module implements.
pub fn register(
    registry: &mut HandlerRegistry,
    api: Arc<dyn WinApi>,
    config: AgentConfig,
    passwords: Arc<dyn PasswordSource>,
    delivery: Arc<dyn SealedResultChannel>,
) {
    let services: Arc<Services> = Arc::new(Services::new(api.clone(), config.clone()));
    let updates: Arc<Updates> = Arc::new(Updates::new(api.clone()));
    let firewall: Arc<Firewall> = Arc::new(Firewall::new(api.clone(), config.clone()));
    let users: Arc<Users> = Arc::new(Users::new(api, config, passwords, delivery));

    for id in [
        SERVICE_LIST,
        SERVICE_STATUS,
        SERVICE_START,
        SERVICE_STOP,
        SERVICE_RESTART,
        SERVICE_SET_START_TYPE,
        SERVICE_SET_RECOVERY,
    ] {
        registry.register(op(id), services.clone());
    }
    for id in [
        UPDATE_SEARCH,
        UPDATE_PENDING,
        UPDATE_INSTALL,
        UPDATE_SCHEDULE_REBOOT,
        UPDATE_REBOOT_STATUS,
    ] {
        registry.register(op(id), updates.clone());
    }
    for id in [
        FIREWALL_LIST,
        FIREWALL_ADD_RULE,
        FIREWALL_REMOVE_RULE,
        FIREWALL_SET_PROFILE,
    ] {
        registry.register(op(id), firewall.clone());
    }
    for id in [
        USER_LIST,
        USER_CREATE,
        USER_DISABLE,
        USER_RESET_PASSWORD,
        USER_SET_GROUPS,
        GROUP_LIST,
        GROUP_ADD_MEMBER,
        GROUP_REMOVE_MEMBER,
    ] {
        registry.register(op(id), users.clone());
    }
}

/// The [`OpId`] for a catalogue constant. The constants are valid dotted lower-case paths
/// by construction (a test in [`crate::catalogue`] proves it), so this cannot fail for the
/// ids this crate owns.
fn op(id: &str) -> OpId {
    match OpId::new(id) {
        Some(op) => op,
        None => panic!("the Windows catalogue declares an invalid operation id"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::catalogue::operations;
    use crate::fake::FakeWin;
    use crate::secrets::FixedPasswordSource;

    use super::*;

    struct NullChannel;

    impl SealedResultChannel for NullChannel {
        fn deliver(
            &self,
            _label: &crate::secrets::SecretLabel,
            _secret: crate::secrets::Secret,
        ) -> Result<(), WindowsError> {
            Ok(())
        }
    }

    #[test]
    fn every_catalogue_operation_gets_a_handler() {
        let mut registry = HandlerRegistry::new();
        register(
            &mut registry,
            Arc::new(FakeWin::stock()),
            AgentConfig::default(),
            Arc::new(FixedPasswordSource::new("Fixed-Passw0rd!Fixed-Pass")),
            Arc::new(NullChannel),
        );
        assert_eq!(registry.len(), operations().len());
        for operation in operations() {
            assert!(
                registry.contains(&operation.id),
                "{} has no handler",
                operation.id.as_str()
            );
        }
    }
}
