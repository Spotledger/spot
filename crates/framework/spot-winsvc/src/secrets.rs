//! First-start secrets: the PostgreSQL superuser password and the site keys (brief
//! section "Directories": "generated at first start with random values and stored in
//! files readable only by the service account — never in the MSI, registry or command
//! line"). Generation uses the OS CSPRNG, the same seam `spot-auth` uses for its own
//! tokens; this module never logs or returns the generated value through an error.

use rand::TryRngCore;
use std::path::Path;

/// A generated secret's length in bytes before encoding (256 bits).
pub const SECRET_BYTES: usize = 32;

/// Characters a generated secret is drawn from: upper/lower letters and digits only, so
/// it is safe in a `postgresql.conf`-adjacent password file and on any shell that might
/// (but never should) see it.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Draws [`SECRET_BYTES`] bytes from the OS CSPRNG and renders them over [`ALPHABET`].
///
/// # Errors
/// [`crate::error::WinsvcError::Failed`] when the OS entropy source itself fails (never
/// silently falls back to a weaker source).
pub fn generate_secret() -> Result<String, crate::error::WinsvcError> {
    let mut raw = [0u8; SECRET_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut raw)
        .map_err(|_| crate::error::WinsvcError::failed("OS entropy source unavailable"))?;
    Ok(raw
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect())
}

/// Writes `secret` to `path`, creating it if needed, with permissions that deny every
/// account but the owner (the service account runs the process that calls this). On
/// Windows the real ACL ([`crate::acl`]) is applied separately by the installer; this
/// function's own `cfg(unix)` branch sets mode `0600` so the same logic is exercised on
/// the Linux test host.
///
/// # Errors
/// [`crate::error::WinsvcError::Io`] on any filesystem failure. Never includes `secret`.
pub fn write_secret_file(path: &Path, secret: &str) -> Result<(), crate::error::WinsvcError> {
    std::fs::write(path, secret)
        .map_err(|e| crate::error::WinsvcError::io("writing secret file", &e))?;
    restrict_permissions(path)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), crate::error::WinsvcError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| crate::error::WinsvcError::io("restricting secret file permissions", &e))
}

#[cfg(windows)]
fn restrict_permissions(path: &Path) -> Result<(), crate::error::WinsvcError> {
    // The real ACL: only the service account (`NT SERVICE\SpotledgerServer`) and
    // Administrators/SYSTEM can read this file; `Users` gets nothing. Applied here, at
    // write time, so the secret is never briefly world-readable between being written
    // and a later installer pass — see `crate::service::win_acl`.
    crate::service::win_acl::restrict_secret_file(path)
}

/// Reads the secret at `path` if it already exists; otherwise generates a fresh one with
/// [`generate_secret`], writes it through [`write_secret_file`] (so it is restricted from
/// the moment it first exists), and returns it. This is the one first-start pattern both
/// the PostgreSQL superuser password and the site keys use (brief section
/// "Directories": both "are generated at first start with random values and stored in
/// files readable only by the service account").
///
/// # Errors
/// [`crate::error::WinsvcError::Io`] reading an existing file or creating its parent
/// directory; whatever [`generate_secret`] or [`write_secret_file`] return otherwise.
pub fn ensure_secret(path: &Path) -> Result<String, crate::error::WinsvcError> {
    match std::fs::read_to_string(path) {
        Ok(existing) => Ok(existing),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let generated = generate_secret()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| crate::error::WinsvcError::io("creating secrets directory", &e))?;
            }
            write_secret_file(path, &generated)?;
            Ok(generated)
        }
        Err(e) => Err(crate::error::WinsvcError::io("reading secret file", &e)),
    }
}

/// The PostgreSQL superuser password's file name under the secrets directory
/// ([`crate::dirs::data_subdirs::SECRETS`]).
pub const PG_SUPERUSER_PASSWORD_FILE: &str = "pg-superuser-password";

/// The site keys' file name under the secrets directory. One generated value is used as
/// the site's symmetric encryption/signing key material (brief: "the PostgreSQL
/// superuser password and the site keys are generated at first start").
pub const SITE_KEYS_FILE: &str = "site-keys";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn generated_secrets_have_the_expected_length_and_alphabet() {
        let secret = generate_secret().expect("OS entropy source");
        assert_eq!(secret.len(), SECRET_BYTES);
        assert!(secret.bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn two_generated_secrets_are_never_equal() {
        let a = generate_secret().expect("first secret");
        let b = generate_secret().expect("second secret");
        assert_ne!(a, b);
    }

    #[test]
    #[cfg(unix)]
    fn write_secret_file_restricts_permissions_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pg-superuser-password");
        write_secret_file(&path, "a-generated-secret").expect("write");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let contents = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(contents, "a-generated-secret");
    }

    #[test]
    fn ensure_secret_generates_once_and_reuses_it_on_every_later_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets").join(PG_SUPERUSER_PASSWORD_FILE);

        let first = ensure_secret(&path).expect("first call generates");
        assert_eq!(first.len(), SECRET_BYTES);
        assert!(path.exists());

        let second = ensure_secret(&path).expect("second call reads back");
        assert_eq!(first, second, "a second call must never regenerate");
    }

    #[test]
    fn ensure_secret_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secrets").join(SITE_KEYS_FILE);
        assert!(!path.parent().expect("has a parent").exists());

        ensure_secret(&path).expect("creates the secrets directory and the file");

        assert!(path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn ensure_secret_restricts_permissions_like_write_secret_file_does() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SITE_KEYS_FILE);

        ensure_secret(&path).expect("generates and writes");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_pg_password_and_site_keys_file_names_are_distinct() {
        assert_ne!(PG_SUPERUSER_PASSWORD_FILE, SITE_KEYS_FILE);
    }
}
