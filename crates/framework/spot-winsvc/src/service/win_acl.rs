//! Applying the real Windows ACLs (`cfg(windows)` only; **unverified on this Linux CI
//! host** — reviewed by reading only, see the task's `## Open questions`).
//!
//! [`crate::acl`] is the portable, Linux-tested part: which trustee gets which right.
//! This module is the thin, Windows-only translation of that data into a real
//! `SetNamedSecurityInfoW` call, used for:
//! - the two product directories at install time (`%ProgramFiles%\Spotledger`,
//!   `%ProgramData%\Spotledger`), each protected (no inherited ACEs) then given exactly
//!   the entries [`crate::acl::program_files_acl`]/[`crate::acl::program_data_acl`] list;
//! - every generated secret file ([`crate::secrets::write_secret_file`]'s Windows branch),
//!   restricted to the service account plus Administrators/SYSTEM, `Users` excluded —
//!   the brief's "stored in files readable only by the service account".

use crate::acl::{AclRule, Right, Trustee, program_data_acl, program_files_acl};
use crate::error::WinsvcError;
use std::path::Path;
use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
    SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_NAME, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows::Win32::Security::{
    ACE_FLAGS, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, NO_INHERITANCE,
    OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
};
use windows::core::{PCWSTR, PWSTR};

/// `GENERIC_ALL` (winnt.h): full control.
const GENERIC_ALL: u32 = 0x1000_0000;
/// `GENERIC_READ` (winnt.h).
const GENERIC_READ: u32 = 0x8000_0000;
/// `GENERIC_WRITE` (winnt.h).
const GENERIC_WRITE: u32 = 0x4000_0000;
/// `GENERIC_EXECUTE` (winnt.h).
const GENERIC_EXECUTE: u32 = 0x2000_0000;
/// `DELETE` (winnt.h, a `STANDARD_RIGHTS` bit): needed for `Right::Modify` to match the
/// conventional meaning of the Windows "Modify" permission (read/write/execute/delete,
/// but not take ownership or change permissions — distinct from `FullControl`).
const DELETE: u32 = 0x0001_0000;

/// The access mask one [`Right`] maps to. `Right::None` is never actually passed to
/// `SetEntriesInAclW` ([`acl_rule_to_entry`] filters it out first) — it exists so the
/// portable [`crate::acl`] rule list can still state "no access" explicitly for `Users`.
fn access_mask(right: Right) -> u32 {
    match right {
        Right::FullControl => GENERIC_ALL,
        Right::Modify => GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | DELETE,
        Right::Write => GENERIC_READ | GENERIC_WRITE,
        Right::Read => GENERIC_READ,
        Right::None => 0,
    }
}

/// A UTF-16, NUL-terminated string for a Win32 wide-string argument. Leaked into a
/// `Vec<u16>` the caller must keep alive for the duration of the ACL call (trustees by
/// name are not copied by `SetEntriesInAclW`, only referenced).
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// How an ACE names its trustee. Built-in accounts go by name; the virtual service
/// account goes by its derived SID ([`crate::acl::service_sid_string`]) because resolving
/// `NT SERVICE\SpotledgerServer` by name fails (Win32 error 1332) until the service is
/// registered, which is not the case on a test host or for a secret written before the
/// service exists.
enum TrusteeName {
    Name(Vec<u16>),
    Sid(PSID),
}

impl TrusteeName {
    fn for_trustee(trustee: Trustee) -> Result<Self, WinsvcError> {
        if trustee != Trustee::ServiceAccount {
            return Ok(TrusteeName::Name(wide(trustee.account_name())));
        }
        let text = wide(&crate::acl::service_sid_string(
            crate::service::SERVICE_NAME,
        ));
        let mut sid = PSID::default();
        // SAFETY: `text` is a NUL-terminated UTF-16 string that outlives the call; `sid`
        // is an out-parameter that receives a `LocalAlloc`ed SID, freed in `Drop`.
        unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid) }
            .map_err(|e| WinsvcError::failed(format!("building the service SID: {e}")))?;
        Ok(TrusteeName::Sid(sid))
    }
}

impl Drop for TrusteeName {
    fn drop(&mut self) {
        if let TrusteeName::Sid(sid) = self {
            // SAFETY: the SID came from `ConvertStringSidToSidW` (a `LocalAlloc`) and is
            // freed exactly once, here.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(sid.0)));
            }
        }
    }
}

/// Builds one `EXPLICIT_ACCESS_W` naming `rule.trustee` by its well-known account name
/// (no SID lookup needed: `TRUSTEE_IS_NAME` resolves the name at apply time, exactly as
/// `icacls`/`SetNamedSecurityInfoW` already do). `names` must outlive the returned
/// entries; the caller passes the same backing `Vec<Vec<u16>>` to [`apply_acl`].
///
/// `inheritance` controls whether the entry propagates to children: directory rules
/// ([`secure_program_files_dir`], [`secure_program_data_dir`]) need
/// `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE` so subdirectories and files inherit the
/// same access, since the object is protected (no ACEs from its parent); secret-file
/// rules ([`restrict_secret_file`]) pass [`NO_INHERITANCE`] because a file has no
/// children to inherit anything.
fn acl_rule_to_entry(
    rule: &AclRule,
    name: &mut TrusteeName,
    inheritance: ACE_FLAGS,
) -> EXPLICIT_ACCESS_W {
    let (form, ptr) = match name {
        TrusteeName::Name(buf) => (TRUSTEE_IS_NAME, PWSTR(buf.as_mut_ptr())),
        TrusteeName::Sid(sid) => (TRUSTEE_IS_SID, PWSTR(sid.0.cast())),
    };
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: access_mask(rule.right),
        grfAccessMode: SET_ACCESS,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: form,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: ptr,
        },
    }
}

/// Applies `rules` to `path` as the object's DACL, after first protecting it (no
/// inherited ACEs from a parent directory) — the brief's "Users none" must mean the
/// trustee truly has no access, not merely no *explicit* entry while an inherited ACE
/// still grants it something.
///
/// `inheritance` is [`NO_INHERITANCE`] for a leaf (a secret file) or
/// `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE` for a directory whose subdirectories and
/// files must inherit the same access (the directory is protected, so without this the
/// service account's `Modify` on `%ProgramData%\Spotledger` would stop at the top level
/// and never reach the data it actually needs to read and write).
///
/// # Errors
/// [`WinsvcError::Failed`] when `SetEntriesInAclW` or `SetNamedSecurityInfoW` fail.
pub fn apply_acl(
    path: &Path,
    rules: &[AclRule],
    inheritance: ACE_FLAGS,
) -> Result<(), WinsvcError> {
    let path_wide = wide(&path.display().to_string());
    // Rights explicitly listed as `Right::None` (the brief's "Users none") are dropped
    // here too, same as `crate::acl::to_acl_text`: on a protected (non-inheriting)
    // object, simply never naming the trustee already denies it everything.
    let mut names: Vec<TrusteeName> = rules
        .iter()
        .filter(|r| r.right != Right::None)
        .map(|r| TrusteeName::for_trustee(r.trustee))
        .collect::<Result<_, _>>()?;
    let entries: Vec<EXPLICIT_ACCESS_W> = rules
        .iter()
        .filter(|r| r.right != Right::None)
        .zip(names.iter_mut())
        .map(|(rule, name)| acl_rule_to_entry(rule, name, inheritance))
        .collect();

    let mut new_acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `entries` and the `names` buffers it points into both outlive this call;
    // `new_acl` is an out-parameter `SetEntriesInAclW` allocates and we free with
    // `windows::Win32::Foundation::LocalFree`-equivalent handling below.
    let set_entries = unsafe {
        SetEntriesInAclW(
            Some(entries.as_slice()),
            None,
            std::ptr::addr_of_mut!(new_acl),
        )
    };
    if set_entries != ERROR_SUCCESS {
        return Err(WinsvcError::failed(format!(
            "building the access control list: Win32 error {}",
            set_entries.0
        )));
    }

    // SAFETY: `path_wide` outlives this call; `new_acl` was just built above and is a
    // valid `*const ACL` for `SetNamedSecurityInfoW`, which does not take ownership of
    // it (we free it immediately after, as its own documentation requires).
    let result = unsafe {
        SetNamedSecurityInfoW(
            windows::core::PWSTR(path_wide.as_ptr().cast_mut()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl),
            None,
        )
    };
    // SAFETY: `new_acl` was allocated by `SetEntriesInAclW` above (via `LocalAlloc`) and
    // is freed exactly once, after its only use.
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            new_acl.cast(),
        )));
    }
    if result != ERROR_SUCCESS {
        return Err(WinsvcError::failed(format!(
            "applying the access control list to {}: Win32 error {}",
            path.display(),
            result.0
        )));
    }
    Ok(())
}

/// Creates (if missing) and ACL-protects `%ProgramFiles%\Spotledger` (brief:
/// "Administrators/SYSTEM write, service account read").
///
/// # Errors
/// An I/O failure creating the directory, or whatever [`apply_acl`] returns.
pub fn secure_program_files_dir(path: &Path) -> Result<(), WinsvcError> {
    std::fs::create_dir_all(path)
        .map_err(|e| WinsvcError::io("creating the program files directory", &e))?;
    apply_acl(
        path,
        &program_files_acl(),
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
    )
}

/// Creates (if missing) and ACL-protects `%ProgramData%\Spotledger` (brief: "service
/// account modify, Administrators full, Users none").
///
/// # Errors
/// An I/O failure creating the directory, or whatever [`apply_acl`] returns.
pub fn secure_program_data_dir(path: &Path) -> Result<(), WinsvcError> {
    std::fs::create_dir_all(path)
        .map_err(|e| WinsvcError::io("creating the program data directory", &e))?;
    apply_acl(
        path,
        &program_data_acl(),
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
    )
}

/// Restricts a generated secret file (the PostgreSQL superuser password, the site keys)
/// to the service account plus Administrators/SYSTEM (for recovery); `Users` gets
/// nothing. Called from [`crate::secrets::write_secret_file`]'s Windows branch, right
/// after the file is written, so it is never briefly world-readable. [`NO_INHERITANCE`]
/// (not the directory flags [`secure_program_files_dir`]/[`secure_program_data_dir`]
/// use): a file has no children, so there is nothing for an ACE to propagate to.
///
/// # Errors
/// Whatever [`apply_acl`] returns.
pub fn restrict_secret_file(path: &Path) -> Result<(), WinsvcError> {
    let rules = [
        AclRule {
            trustee: Trustee::ServiceAccount,
            right: Right::Read,
        },
        AclRule {
            trustee: Trustee::Administrators,
            right: Right::FullControl,
        },
        AclRule {
            trustee: Trustee::System,
            right: Right::FullControl,
        },
        AclRule {
            trustee: Trustee::Users,
            right: Right::None,
        },
    ];
    apply_acl(path, &rules, NO_INHERITANCE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn entry_flags(trustee: Trustee, inheritance: ACE_FLAGS) -> ACE_FLAGS {
        let rule = AclRule {
            trustee,
            right: Right::Read,
        };
        let mut name = TrusteeName::for_trustee(trustee).expect("trustee");
        acl_rule_to_entry(&rule, &mut name, inheritance).grfInheritance
    }

    #[test]
    fn directory_entries_carry_container_and_object_inherit() {
        let dir = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
        let flags = entry_flags(Trustee::Administrators, dir);
        assert_eq!(flags, dir);
        assert_ne!(flags.0 & CONTAINER_INHERIT_ACE.0, 0);
        assert_ne!(flags.0 & OBJECT_INHERIT_ACE.0, 0);
    }

    #[test]
    fn secret_file_entries_carry_no_inheritance() {
        assert_eq!(entry_flags(Trustee::ServiceAccount, NO_INHERITANCE).0, 0);
    }

    #[test]
    fn the_service_account_is_named_by_sid_so_it_need_not_be_installed() {
        let name = TrusteeName::for_trustee(Trustee::ServiceAccount).expect("sid");
        assert!(matches!(name, TrusteeName::Sid(_)));
    }

    #[test]
    fn a_secret_file_can_be_restricted_before_the_service_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        std::fs::write(&path, "x").expect("write");
        restrict_secret_file(&path).expect("restrict with no service installed");
    }
}
