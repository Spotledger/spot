//! Replicas: status, lag, `pg_basebackup` standbys and a guarded promotion.
//!
//! A promotion is the one replica operation that can split a cluster: if the old primary is
//! still reachable, two nodes may accept writes. The module therefore refuses a promotion
//! while [`ReplicaPort::primary_reachable`] is true unless the caller carries the `critical`
//! flag, and reports what it found either way. The guard is a refusal, not a silent no-op.
//!
//! All database access goes through [`ReplicaPort`]; the module reads no clock and spawns no
//! process itself.

use crate::error::{RuntimeError, RuntimeResult};

/// Which side of a replication pair this node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaRole {
    /// Accepts writes.
    Primary,
    /// Streams from a primary.
    Standby,
}

impl ReplicaRole {
    /// A short, stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Standby => "standby",
        }
    }
}

/// The replication status of this node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaStatus {
    /// Whether this node is a primary or a standby.
    pub role: ReplicaRole,
    /// Whether a standby is streaming from its primary.
    pub streaming: bool,
    /// The primary's host, when known (never a credential).
    pub primary_host: Option<String>,
    /// The last WAL position received, as PostgreSQL's text form.
    pub receive_lsn: Option<String>,
    /// The last WAL position replayed, as PostgreSQL's text form.
    pub replay_lsn: Option<String>,
}

/// How far a standby is behind its primary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaLag {
    /// The byte distance between the primary's WAL and the standby's replay position.
    pub bytes: u64,
    /// The age of the last replayed transaction, when PostgreSQL can estimate it.
    pub seconds: Option<u64>,
    /// Whether the standby is currently streaming at all.
    pub streaming: bool,
}

/// A request to build a standby from the primary with `pg_basebackup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandbyRequest {
    /// The primary's host name or address.
    pub host: String,
    /// The primary's port.
    pub port: u16,
    /// The standby's data directory (absolute).
    pub data_dir: String,
    /// The replication slot to use, when one was provisioned.
    pub slot: Option<String>,
}

/// What building a standby did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandbyOutcome {
    /// The standby's data directory.
    pub data_dir: String,
    /// The primary it copied from.
    pub host: String,
    /// The primary's port.
    pub port: u16,
    /// The replication slot used, when any.
    pub slot: Option<String>,
    /// Human notes (for example that a restart is needed to start streaming).
    pub notes: Vec<String>,
}

/// What a promotion did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromoteReport {
    /// Whether the primary was reachable when the promotion was requested.
    pub primary_reachable: bool,
    /// Whether the critical flag was needed and present.
    pub forced: bool,
    /// Whether the node was promoted.
    pub promoted: bool,
    /// A short, secret-free note for the console.
    pub note: String,
}

/// The replica administrative seam. The real adapter runs `pg_basebackup`, reads
/// `pg_stat_replication` and runs `pg_ctl promote`; tests script it.
pub trait ReplicaPort: Send + Sync {
    /// The current replication status.
    ///
    /// # Errors
    /// A backend error when the status cannot be read.
    fn status(&self) -> RuntimeResult<ReplicaStatus>;

    /// The current lag.
    ///
    /// # Errors
    /// A backend error when the lag cannot be computed.
    fn lag(&self) -> RuntimeResult<ReplicaLag>;

    /// Builds a standby with `pg_basebackup`.
    ///
    /// # Errors
    /// A backend error when the copy fails.
    fn create_standby(&self, request: &StandbyRequest) -> RuntimeResult<StandbyOutcome>;

    /// Promotes this node to primary.
    ///
    /// # Errors
    /// A backend error when the promotion fails.
    fn promote(&self) -> RuntimeResult<()>;

    /// Whether the old primary still answers. This is what makes promotion dangerous.
    ///
    /// # Errors
    /// A backend error when reachability cannot be established; the caller treats an error as
    /// "unknown" and refuses unless the critical flag is present.
    fn primary_reachable(&self) -> RuntimeResult<bool>;
}

/// Reads replication status.
///
/// # Errors
/// The port's error.
pub fn status(replica: &dyn ReplicaPort) -> RuntimeResult<ReplicaStatus> {
    replica.status()
}

/// Reads replication lag.
///
/// # Errors
/// The port's error.
pub fn lag(replica: &dyn ReplicaPort) -> RuntimeResult<ReplicaLag> {
    replica.lag()
}

/// Builds a standby, validating the request before the port sees it.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] for an empty host, a zero port or a relative data
/// directory, and the port's error.
pub fn create_standby(
    replica: &dyn ReplicaPort,
    request: &StandbyRequest,
) -> RuntimeResult<StandbyOutcome> {
    if request.host.trim().is_empty() {
        return Err(invalid("create", "host", "must not be empty"));
    }
    if request.port == 0 {
        return Err(invalid("create", "port", "must be between 1 and 65535"));
    }
    let host = request.host.trim();
    if host.starts_with('-')
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'_' | b'-'))
    {
        return Err(invalid(
            "create",
            "host",
            "is not a valid host name or address",
        ));
    }
    if !request.data_dir.starts_with('/')
        || request.data_dir.split('/').any(|part| part == "..")
        || request.data_dir.contains('\0')
    {
        return Err(invalid(
            "create",
            "data_dir",
            "must be an absolute path without parent segments",
        ));
    }
    if let Some(slot) = &request.slot
        && (slot.is_empty()
            || slot.len() > 63
            || !slot
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    {
        return Err(invalid(
            "create",
            "slot",
            "may contain only lowercase letters, digits and underscore",
        ));
    }
    replica.create_standby(request)
}

/// Promotes this node, refusing while the primary is reachable unless `critical` is set.
///
/// # Errors
/// [`RuntimeError::PromotePrimaryReachable`] when the primary answers and `critical` is
/// false, or the port's error.
pub fn promote(replica: &dyn ReplicaPort, critical: bool) -> RuntimeResult<PromoteReport> {
    let primary_reachable = replica.primary_reachable()?;
    if primary_reachable && !critical {
        return Err(RuntimeError::PromotePrimaryReachable);
    }
    replica.promote()?;
    let note = if primary_reachable {
        "promoted with the critical flag while the old primary was reachable; check for a split brain"
            .to_owned()
    } else {
        "promoted after the primary became unreachable".to_owned()
    };
    Ok(PromoteReport {
        primary_reachable,
        forced: primary_reachable && critical,
        promoted: true,
        note,
    })
}

/// The invalid-argument error for a replica operation.
fn invalid(op: &str, param: &str, detail: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        op: format!("db.replica.{op}"),
        param: param.to_owned(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeReplica {
        reachable: bool,
        promoted: Mutex<usize>,
        created: Mutex<Vec<StandbyRequest>>,
    }

    impl ReplicaPort for FakeReplica {
        fn status(&self) -> RuntimeResult<ReplicaStatus> {
            Ok(ReplicaStatus {
                role: ReplicaRole::Standby,
                streaming: true,
                primary_host: Some("primary".to_owned()),
                receive_lsn: Some("0/100".to_owned()),
                replay_lsn: Some("0/90".to_owned()),
            })
        }

        fn lag(&self) -> RuntimeResult<ReplicaLag> {
            Ok(ReplicaLag {
                bytes: 16,
                seconds: Some(2),
                streaming: true,
            })
        }

        fn create_standby(&self, request: &StandbyRequest) -> RuntimeResult<StandbyOutcome> {
            self.created.lock().unwrap().push(request.clone());
            Ok(StandbyOutcome {
                data_dir: request.data_dir.clone(),
                host: request.host.clone(),
                port: request.port,
                slot: request.slot.clone(),
                notes: vec!["start the standby to begin streaming".to_owned()],
            })
        }

        fn promote(&self) -> RuntimeResult<()> {
            *self.promoted.lock().unwrap() += 1;
            Ok(())
        }

        fn primary_reachable(&self) -> RuntimeResult<bool> {
            Ok(self.reachable)
        }
    }

    #[test]
    fn status_and_lag_come_from_the_port() {
        let replica = FakeReplica::default();
        assert_eq!(status(&replica).unwrap().role, ReplicaRole::Standby);
        assert_eq!(lag(&replica).unwrap().bytes, 16);
    }

    #[test]
    fn promotion_is_refused_while_the_primary_answers() {
        let replica = FakeReplica {
            reachable: true,
            ..FakeReplica::default()
        };
        let error = promote(&replica, false).unwrap_err();
        assert_eq!(error.code(), crate::error::PROMOTE_PRIMARY_REACHABLE);
        assert_eq!(*replica.promoted.lock().unwrap(), 0);
    }

    #[test]
    fn the_critical_flag_allows_a_forced_promotion() {
        let replica = FakeReplica {
            reachable: true,
            ..FakeReplica::default()
        };
        let report = promote(&replica, true).unwrap();
        assert!(report.primary_reachable);
        assert!(report.forced);
        assert!(report.promoted);
        assert_eq!(*replica.promoted.lock().unwrap(), 1);
    }

    #[test]
    fn an_unreachable_primary_needs_no_flag() {
        let replica = FakeReplica::default();
        let report = promote(&replica, false).unwrap();
        assert!(!report.primary_reachable);
        assert!(!report.forced);
    }

    #[test]
    fn a_standby_request_is_validated() {
        let replica = FakeReplica::default();
        let ok = StandbyRequest {
            host: "primary".to_owned(),
            port: 5433,
            data_dir: "/var/lib/spot/pgsql".to_owned(),
            slot: Some("standby_1".to_owned()),
        };
        assert_eq!(
            create_standby(&replica, &ok).unwrap().slot.as_deref(),
            Some("standby_1")
        );
        assert_eq!(replica.created.lock().unwrap().len(), 1);

        let relative = StandbyRequest {
            data_dir: "pgsql".to_owned(),
            ..ok.clone()
        };
        assert_eq!(
            create_standby(&replica, &relative).unwrap_err().code(),
            crate::error::INVALID_ARGUMENT
        );
        let no_host = StandbyRequest {
            host: " ".to_owned(),
            ..ok
        };
        assert!(create_standby(&replica, &no_host).is_err());
        for (host, dir, slot) in [
            ("-oProxyCommand", "/var/lib/spot/pgsql", "s"),
            ("primary", "/var/lib/spot/../etc", "s"),
            ("primary", "/var/lib/spot/pgsql", "bad slot'; --"),
        ] {
            let bad = StandbyRequest {
                host: host.to_owned(),
                port: 5433,
                data_dir: dir.to_owned(),
                slot: Some(slot.to_owned()),
            };
            assert!(
                create_standby(&replica, &bad).is_err(),
                "{host} {dir} {slot}"
            );
        }
    }
}
