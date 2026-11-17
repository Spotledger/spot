//! Typed errors with stable `durability_gate.*` codes.

/// Everything the policy changes and the start-up checks can refuse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    /// `replica-sync` was asked for but the site has no standby configured: it would silently
    /// behave as `local`.
    #[error("replica-sync needs a configured standby")]
    NoStandby,
    /// An upgrade of the policy needs the journal shipper healthy.
    #[error("the journal shipper is not healthy, so the policy cannot be raised")]
    ShipperUnhealthy,
    /// Only the tenant administrator may lower the policy.
    #[error("only the tenant administrator may lower the durability policy")]
    PermissionDenied,
}

impl GateError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoStandby => "durability_gate.no_standby",
            Self::ShipperUnhealthy => "durability_gate.shipper_unhealthy",
            Self::PermissionDenied => "durability_gate.permission_denied",
        }
    }
}
