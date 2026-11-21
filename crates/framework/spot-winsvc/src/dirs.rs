//! Where Spotledger lives on disk (brief section "Directories").
//!
//! `%ProgramFiles%\Spotledger\` holds the binaries; `%ProgramData%\Spotledger\` holds the
//! data, config, logs and generated secrets. Both are resolved from the environment so
//! tests on any platform can point them at a temporary directory; the real service always
//! uses the Windows well-known folders (`ProgramFiles`/`ProgramData` are set by Windows
//! itself for every process, including a service's).

use std::path::PathBuf;

/// The product directory name under both roots.
pub const PRODUCT_DIR: &str = "Spotledger";

/// `%ProgramFiles%\Spotledger` (binaries), or `SPOT_WINSVC_PROGRAM_FILES` when set (tests
/// and non-Windows development hosts, which have no `ProgramFiles` variable).
#[must_use]
pub fn program_files_dir() -> PathBuf {
    resolve("SPOT_WINSVC_PROGRAM_FILES", "ProgramFiles", "/opt")
}

/// `%ProgramData%\Spotledger` (data, config, logs, secrets), or
/// `SPOT_WINSVC_PROGRAM_DATA` when set.
#[must_use]
pub fn program_data_dir() -> PathBuf {
    resolve("SPOT_WINSVC_PROGRAM_DATA", "ProgramData", "/var/lib")
}

fn resolve(test_var: &str, native_var: &str, non_windows_fallback_root: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(test_var) {
        return PathBuf::from(dir).join(PRODUCT_DIR);
    }
    if let Ok(dir) = std::env::var(native_var) {
        return PathBuf::from(dir).join(PRODUCT_DIR);
    }
    PathBuf::from(non_windows_fallback_root).join(PRODUCT_DIR)
}

/// Subdirectories under `%ProgramData%\Spotledger` the host creates on first start.
pub mod data_subdirs {
    /// PostgreSQL's data directory.
    pub const PG_DATA: &str = "pgdata";
    /// PostgreSQL's log directory.
    pub const PG_LOGS: &str = "pglogs";
    /// The service host's own log directory.
    pub const LOGS: &str = "logs";
    /// Generated secrets: the PostgreSQL superuser password and the site keys.
    pub const SECRETS: &str = "secrets";
    /// Site configuration (`spot.toml` and friends).
    pub const CONFIG: &str = "config";
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn program_files_and_program_data_differ_and_end_in_the_product_name() {
        let pf = program_files_dir();
        let pd = program_data_dir();
        assert_ne!(pf, pd);
        assert_eq!(pf.file_name().expect("test value"), PRODUCT_DIR);
        assert_eq!(pd.file_name().expect("test value"), PRODUCT_DIR);
    }

    #[test]
    fn test_override_wins_over_native_variable() {
        // SAFETY-free: `std::env::set_var` touches process env, not memory; the test
        // restores both variables so it never leaks into a sibling test.
        let prev_test = std::env::var("SPOT_WINSVC_PROGRAM_DATA").ok();
        let prev_native = std::env::var("ProgramData").ok();
        unsafe {
            std::env::set_var("ProgramData", "C:\\ProgramData");
            std::env::set_var("SPOT_WINSVC_PROGRAM_DATA", "/tmp/spot-test-data");
        }
        assert_eq!(
            program_data_dir(),
            PathBuf::from("/tmp/spot-test-data").join(PRODUCT_DIR)
        );
        unsafe {
            match prev_test {
                Some(v) => std::env::set_var("SPOT_WINSVC_PROGRAM_DATA", v),
                None => std::env::remove_var("SPOT_WINSVC_PROGRAM_DATA"),
            }
            match prev_native {
                Some(v) => std::env::set_var("ProgramData", v),
                None => std::env::remove_var("ProgramData"),
            }
        }
    }
}
