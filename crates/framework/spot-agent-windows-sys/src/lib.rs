//! `spot-agent-windows-sys` — the storage-and-trust half of the Windows modules
//! (WP-ADM-05.T2, blueprint WS22, ADR-44).
//!
//! Part 1 of WP-ADM-05 covers services, updates, the firewall and users. This crate covers
//! the modules that touch storage and trust, with the same Linux-testable design: a trait
//! with a complete in-memory fake that runs on every platform, and only the thin real
//! implementation gated on Windows.
//!
//! - [`vss`]: the [`VssApi`](vss::VssApi) trait for Volume Shadow Copy (create for a volume
//!   set with writers, list, delete, expose read-only), the [`FakeVss`](vss::FakeVss), the
//!   real `vssadmin` client, and the oldest-first [`apply_retention`](vss::apply_retention)
//!   policy.
//! - [`backup`]: PostgreSQL backup mode behind [`PgBackupPort`](backup::PgBackupPort), the
//!   [`PgBackupGuard`](backup::PgBackupGuard) that leaves backup mode on every path, and
//!   [`SnapshotCoordinator`](backup::SnapshotCoordinator), which runs the fixed sequence
//!   `pg_backup_start`, VSS create, expose, `pg_backup_stop`.
//! - [`provider`]: [`VssProvider`], the Windows backend plugged into
//!   [`spot_snapshot::SnapshotProvider`], with a checksummed copy and a canonical manifest.
//! - [`certificates`]: LocalMachine store management (`Root`, `CA`, `My`) behind
//!   [`CertificateStore`](certificates::CertificateStore): list, import, remove, expiry and
//!   chain checks, the HTTPS binding, and public-only export.
//! - [`catalogue`]: the typed operations the modules register on the management catalogue.
//! - [`error`]: the stable `agent_windows_sys.*` codes, catalogued in
//!   `crates/framework/spot-errors/catalogue/agent_windows_sys.toml`.
//!
//! Time is always an argument (the injected [`spot_core::Clock`] or an explicit instant);
//! no money, quantity or size is ever a float; there is no `unsafe` and no `unwrap` outside
//! tests.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod backup;
pub mod catalogue;
pub mod cert_dates;
pub mod certificates;
pub mod error;
pub mod provider;
pub mod vss;

#[cfg(windows)]
pub mod windows;

pub use backup::{
    BackupHorizon, BackupLabel, BackupSession, BackupStep, CoordinatedSnapshot, FakePgBackup,
    PgBackupGuard, PgBackupPort, SnapshotCoordinator, delete_shadow,
};
pub use catalogue::{MODULE_NAME, WindowsSysModule, op_id};
pub use cert_dates::{parse_certificate_datetime, parse_certutil_dump_validity};
pub use certificates::{
    BindOutcome, CertStore, CertificateInfo, CertificateStore, ChainStatus, Expiry, FakeCertStore,
    HttpsBinding, Thumbprint,
};
pub use error::{WindowsSysError, WindowsSysResult};
pub use provider::{FixedFacts, PROVIDER_KIND, SnapshotFacts, VssFacts, VssProvider};
pub use vss::{
    FakeVss, RetentionPolicy, ShadowCopy, ShadowCopyId, VolumeId, VolumeSet, VssApi, VssStep,
    WriterSet, apply_retention,
};

/// The crate name, for diagnostics and the manifest's `provider` field.
pub const CRATE_NAME: &str = "spot-agent-windows-sys";

/// The crate version, as the manifest records it.
pub const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
