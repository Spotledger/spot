//! The Windows handler set: the `os.*` operations of `spot-agent-windows`. Registered only
//! on Windows and only when `allowed_modules` lists `os`.

use spot_agent_core::HandlerRegistry;
use spot_agent_windows::{Secret, SecretLabel, WindowsError};

use super::HandlerCtx;

/// The operation namespace.
pub const NAMESPACE: &str = "os";

/// A sealed result channel that refuses delivery: generated secrets are not delivered
/// until the sealed channel is wired (a later task), so user creation fails closed.
#[derive(Debug, Default)]
pub struct RefuseDelivery;

impl spot_agent_windows::SealedResultChannel for RefuseDelivery {
    fn deliver(&self, _label: &SecretLabel, _secret: Secret) -> Result<(), WindowsError> {
        Err(WindowsError::SecretDelivery)
    }
}

/// Registers the Windows handlers (a no-op on other platforms).
#[cfg(windows)]
pub fn register(reg: &mut HandlerRegistry, _ctx: &HandlerCtx) {
    use std::sync::Arc;
    spot_agent_windows::handlers::register(
        reg,
        Arc::new(spot_agent_windows::real::RealWin::new()),
        spot_agent_windows::AgentConfig::default(),
        Arc::new(spot_agent_windows::OsPasswordSource),
        Arc::new(RefuseDelivery),
    );
}

/// Registers the Windows handlers (a no-op on other platforms).
#[cfg(not(windows))]
pub fn register(_reg: &mut HandlerRegistry, _ctx: &HandlerCtx) {}

/// The operations of the Windows modules (Windows builds only).
pub fn register_catalogue(builder: &mut spot_mgmt_ops::CatalogueBuilder) {
    use spot_mgmt_ops::OperationModule as _;
    if cfg!(windows) {
        spot_agent_windows::AgentWindowsModule.register(builder);
    }
}
