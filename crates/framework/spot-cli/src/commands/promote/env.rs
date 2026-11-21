//! The identity of a site's environment: `spot_environments::EnvRef`, the one address the
//! promotion service compares on both ends.
//!
//! The **set** is the host root directory's name: every site of one host root is one pipeline.
//! The **tenant** is derived from the site name (a version-7 UUID from a fixed time and ten
//! bytes of a hash of the name), so the same site always has the same address and two sites
//! never share one. Nothing here reads the clock or a random source.

use std::path::Path;

use spot_environments::{EnvRef, Role};
use spot_tenancy::TenantId;

use super::super::bundle::{EnvRecord, read_env, role_of, site_dir};
use super::Env;

/// A site linked to an environment.
pub struct Linked {
    /// The site name.
    pub site: String,
    /// The stored record.
    pub record: EnvRecord,
    /// The role the record names.
    pub role: Role,
    /// The address the promotion service compares.
    pub env: EnvRef,
}

fn fnv(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The tenant id of `site`: deterministic, distinct per name.
#[must_use]
pub fn tenant_of(site: &str) -> TenantId {
    let first = fnv(site.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let second = fnv(site.as_bytes(), 0x8422_2325_cbf2_9ce4);
    let mut entropy = [0_u8; 10];
    entropy[..8].copy_from_slice(&first.to_be_bytes());
    entropy[8..].copy_from_slice(&second.to_be_bytes()[..2]);
    TenantId::new_v7(0, entropy)
}

/// The environment set's name: the host root directory's name.
#[must_use]
pub fn set_of(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .or_else(|| root.file_name())
        .map_or_else(
            || "default".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        )
}

/// The address of `site` as `role` in `set`.
#[must_use]
pub fn env_ref(set: &str, site: &str, role: Role) -> EnvRef {
    EnvRef {
        set: set.to_owned(),
        role,
        tenant: tenant_of(site),
    }
}

/// The guard's view of an address: the set and the role's name.
#[must_use]
pub fn guard_env(env: &EnvRef) -> Env {
    Env {
        set: env.set.clone(),
        role: env.role.name().to_owned(),
    }
}

/// Reads the site's environment record and builds its address.
///
/// # Errors
/// A refusal message when the site is not linked or the record names an unknown kind, and the
/// message of a record that does not parse.
pub fn link(root: &Path, set: &str, site: &str) -> Result<Linked, String> {
    let record = read_env(&site_dir(root, site))
        .map_err(|e| e.0)?
        .ok_or_else(|| {
            format!(
                "refused (not_linked): site `{site}` has no environment; run `spot bundle env link`"
            )
        })?;
    let role = role_of(&record.kind).ok_or_else(|| {
        format!("refused (not_linked): the environment record of `{site}` names an unknown kind")
    })?;
    Ok(Linked {
        site: site.to_owned(),
        env: env_ref(set, site, role),
        record,
        role,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_site_has_one_stable_address_and_two_sites_two() {
        assert_eq!(tenant_of("alpha"), tenant_of("alpha"));
        assert_ne!(tenant_of("alpha"), tenant_of("beta"));
    }

    #[test]
    fn the_address_carries_set_role_and_tenant() {
        let a = env_ref("erp", "alpha", Role::Dev);
        let b = env_ref("erp", "alpha", Role::Test);
        assert_ne!(a, b);
        assert_eq!(guard_env(&a).role, "dev");
        assert_eq!(guard_env(&a).set, "erp");
    }
}
