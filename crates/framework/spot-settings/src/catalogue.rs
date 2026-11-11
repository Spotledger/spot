//! The seed catalogue: ten real platform settings, compiled in from `settings/*.toml`.
//!
//! These are examples the platform tasks will own later. They are compiled into the library
//! with `include_str!`, so this crate never touches the filesystem; [`load_seed_catalogue`]
//! and [`parse_spec`] are the only entry points.

use crate::error::{SettingsError, SettingsErrorKind, SettingsErrorReport};
use crate::spec::SettingSpec;

/// The seed files, in key order.
const SEED_FILES: &[&str] = &[
    include_str!("../settings/company_fiscal_year_start.toml"),
    include_str!("../settings/os_kernel_version.toml"),
    include_str!("../settings/runtime_http_listen_address.toml"),
    include_str!("../settings/runtime_jobs_worker_concurrency.toml"),
    include_str!("../settings/runtime_logging_level.toml"),
    include_str!("../settings/runtime_session_idle_timeout.toml"),
    include_str!("../settings/runtime_smtp_host.toml"),
    include_str!("../settings/runtime_smtp_password.toml"),
    include_str!("../settings/tenant_backup_retention_days.toml"),
    include_str!("../settings/tenant_default_currency.toml"),
];

/// How many seed settings are compiled in.
pub const SEED_FILE_COUNT: usize = SEED_FILES.len();

/// Parses one setting spec from TOML text.
///
/// # Errors
///
/// Returns `settings.catalogue_invalid` when the text is not a valid [`SettingSpec`]. It
/// never panics, whatever the input.
pub fn parse_spec(text: &str) -> Result<SettingSpec, SettingsErrorReport> {
    match toml::from_str::<SettingSpec>(text) {
        Ok(spec) => Ok(spec),
        Err(error) => Err(SettingsError::new(
            SettingsErrorKind::CatalogueInvalid,
            format!("setting spec is not valid TOML: {error}"),
        )
        .into()),
    }
}

/// Loads the ten compiled-in seed settings.
///
/// # Errors
///
/// Returns `settings.catalogue_invalid` if any compiled-in file fails to parse (a build-time
/// mistake, caught by the crate tests).
pub fn load_seed_catalogue() -> Result<Vec<SettingSpec>, SettingsErrorReport> {
    let mut specs = Vec::with_capacity(SEED_FILES.len());
    let mut report = SettingsErrorReport::new();
    for text in SEED_FILES {
        match parse_spec(text) {
            Ok(spec) => specs.push(spec),
            Err(error) => report.extend(error),
        }
    }
    report.into_result()?;
    Ok(specs)
}
