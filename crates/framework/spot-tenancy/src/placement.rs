//! The placement map: which cluster and database takes a new tenant.

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::model::{ClusterId, DatabaseId, Placement, Region, TenantId};

/// One database that can hold tenants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    /// The cluster.
    pub cluster: ClusterId,
    /// The database in it.
    pub database: DatabaseId,
    /// The residency region the hardware sits in.
    pub region: Region,
    /// The most live tenants this database may hold.
    pub capacity: u32,
    /// Whether it accepts new tenants (false while draining or in maintenance).
    pub accepting: bool,
}

/// The placement policy. Pure and deterministic: the same inputs give the same placement.
pub struct PlacementMap;

impl PlacementMap {
    /// Chooses a placement for `tenant`.
    ///
    /// The region must match exactly (data residency is never traded for capacity: there is no
    /// fallback to another region), the database must accept tenants and have room, and among
    /// the candidates the least-loaded wins (load = live tenants over capacity, compared by
    /// cross-multiplication, no floating point). Ties go to the lowest cluster id, then the
    /// lowest database id.
    ///
    /// # Errors
    /// [`Error::NoPlacement`] when no database qualifies.
    pub fn choose(
        tenant: &TenantId,
        region: &Region,
        clusters: &[Cluster],
        load: &BTreeMap<(ClusterId, DatabaseId), u32>,
    ) -> Result<Placement> {
        let used = |c: &Cluster| {
            load.get(&(c.cluster.clone(), c.database.clone()))
                .copied()
                .unwrap_or(0)
        };
        let best = clusters
            .iter()
            .filter(|c| {
                &c.region == region && c.accepting && c.capacity > 0 && used(c) < c.capacity
            })
            .min_by(|a, b| {
                // used_a / cap_a  vs  used_b / cap_b  ->  used_a * cap_b  vs  used_b * cap_a (u64, no overflow).
                let left = u64::from(used(a)) * u64::from(b.capacity);
                let right = u64::from(used(b)) * u64::from(a.capacity);
                left.cmp(&right)
                    .then_with(|| a.cluster.cmp(&b.cluster))
                    .then_with(|| a.database.cmp(&b.database))
            })
            .ok_or_else(|| Error::NoPlacement {
                region: region.to_string(),
            })?;
        Ok(Placement {
            cluster: best.cluster.clone(),
            database: best.database.clone(),
            schema: tenant.schema_name(),
        })
    }
}
