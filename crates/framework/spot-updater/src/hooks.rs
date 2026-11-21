//! The data hooks: what an update does to the database and files before and after the switch.
//!
//! An update that changes the schema backs the data up, migrates it, and — on any failure after
//! the migration started — restores it from that backup. An update that changes no schema never
//! calls [`backup`](DataHooks::backup) or [`migrate`](DataHooks::migrate), and so never needs
//! [`restore`](DataHooks::restore). The engine owns the ordering and the rollback rules; a hook
//! only performs its one step.

use crate::error::UpdaterError;

/// Backs up, migrates and restores the data an update touches.
///
/// Hooks must be durable on return and safe to repeat after interruption. A backup must
/// preserve its original snapshot until probation closes; restore must replay that snapshot
/// even if a previous restore was interrupted. The host serializes updates for one root.
pub trait DataHooks: Send + Sync {
    /// Takes a restorable backup of the data before a schema change.
    ///
    /// # Errors
    /// [`UpdaterError::Hook`] when the backup fails; the update rolls back before touching the
    /// schema.
    fn backup(&self) -> Result<(), UpdaterError>;

    /// Migrates the data from `from_epoch` to `to_epoch`.
    ///
    /// # Errors
    /// [`UpdaterError::Hook`] when the migration fails; the update rolls back and restores.
    fn migrate(&self, from_epoch: u64, to_epoch: u64) -> Result<(), UpdaterError>;

    /// Restores the data from the backup taken by [`backup`](DataHooks::backup).
    ///
    /// # Errors
    /// [`UpdaterError::Hook`] when the restore fails; the update needs attention.
    fn restore(&self) -> Result<(), UpdaterError>;
}

/// Hooks for an update that changes no schema: every step is a no-op.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoDataHooks;

impl DataHooks for NoDataHooks {
    fn backup(&self) -> Result<(), UpdaterError> {
        Ok(())
    }

    fn migrate(&self, _from_epoch: u64, _to_epoch: u64) -> Result<(), UpdaterError> {
        Ok(())
    }

    fn restore(&self) -> Result<(), UpdaterError> {
        Ok(())
    }
}
