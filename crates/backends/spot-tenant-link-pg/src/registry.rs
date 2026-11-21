//! The control-plane link registry: it records **that** a link exists, between whom, and
//! whether it is live. Nothing else (ADR-058 rule 3).
//!
//! The row has four columns: the two parties in canonical order (`tenant_a`, `tenant_b`, the
//! `tenant:<uuid>` / `external:<id>` text of a [`Party`]), the link id and a coarse state, `live`
//! or `ended`. There is no grant, topic, field, key or consent column, so the control plane cannot
//! be asked what two tenants exchange, and a leak of the registry reveals only the graph.
//!
//! Both parties of a link share its id, so both tenants' stores upsert the **same** row. The
//! parties never change, and `ended` is sticky: once either side has revoked or expired the link,
//! a late write from the other side cannot make the registry say `live` again.

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_tenant_link::{Error, Link, LinkId, LinkState, Party, Result};

use crate::error::{query_failed, store_failed};
use crate::schema::prepare_registry;

/// Whether a link is usable at all, as the control plane sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryState {
    /// Proposed, accepted or active.
    Live,
    /// Revoked or expired.
    Ended,
}

impl RegistryState {
    /// The state as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Ended => "ended",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "live" => Some(Self::Live),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }

    /// The registry state of a link state.
    #[must_use]
    pub fn of(state: LinkState) -> Self {
        match state {
            LinkState::Proposed | LinkState::AcceptedByPeer | LinkState::Active => Self::Live,
            LinkState::Revoked | LinkState::Expired => Self::Ended,
        }
    }
}

/// One registry row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    /// The link id (the same on both sides).
    pub link: LinkId,
    /// The first party, in canonical order.
    pub tenant_a: Party,
    /// The second party.
    pub tenant_b: Party,
    /// Live or ended.
    pub state: RegistryState,
}

impl RegistryEntry {
    /// The entry for a link: its id, its two parties and its coarse state. No other field of the
    /// link is read.
    #[must_use]
    pub fn of(link: &Link) -> Self {
        Self {
            link: link.id,
            tenant_a: link.parties[0].clone(),
            tenant_b: link.parties[1].clone(),
            state: RegistryState::of(link.state),
        }
    }
}

/// The registry in the control database.
pub struct PgLinkRegistry {
    pool: Pool,
}

impl std::fmt::Debug for PgLinkRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLinkRegistry").finish_non_exhaustive()
    }
}

impl PgLinkRegistry {
    /// A registry on the control database's `pool`.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the registry table if it is missing. Idempotent.
    ///
    /// # Errors
    /// `tenant_link_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        prepare_registry(&self.pool).await
    }

    async fn conn(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|_| store_failed("no registry connection is available"))
    }

    /// Records or updates a link's entry. The parties of an existing entry never change, and an
    /// ended entry stays ended.
    ///
    /// # Errors
    /// [`Error::InvalidParty`] when the id is already registered for different parties, or
    /// [`Error::StoreFailed`].
    pub async fn upsert(&self, entry: &RegistryEntry) -> Result<()> {
        let id = entry.link.as_uuid();
        let a = entry.tenant_a.canonical();
        let b = entry.tenant_b.canonical();
        let conn = self.conn().await?;
        let changed = conn
            .execute(
                "INSERT INTO spot_control.link_registry (link_id, tenant_a, tenant_b, state) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (link_id) DO UPDATE SET \
                    state = CASE WHEN spot_control.link_registry.state = 'ended' \
                                 THEN 'ended' ELSE EXCLUDED.state END \
                 WHERE spot_control.link_registry.tenant_a = EXCLUDED.tenant_a \
                   AND spot_control.link_registry.tenant_b = EXCLUDED.tenant_b",
                &[&id, &a, &b, &entry.state.as_str()],
            )
            .await
            .map_err(query_failed)?;
        if changed == 0 {
            return Err(Error::InvalidParty {
                reason: "a registered link keeps its two parties",
            });
        }
        Ok(())
    }

    /// One entry.
    ///
    /// # Errors
    /// [`Error::NotFound`] or [`Error::StoreFailed`].
    pub async fn get(&self, link: &LinkId) -> Result<RegistryEntry> {
        let conn = self.conn().await?;
        let row = conn
            .query_opt(
                "SELECT tenant_a, tenant_b, state FROM spot_control.link_registry \
                 WHERE link_id = $1",
                &[&link.as_uuid()],
            )
            .await
            .map_err(query_failed)?
            .ok_or_else(|| Error::NotFound {
                link: link.to_string(),
            })?;
        decode(*link, &row)
    }

    /// Every entry that names `party`, ordered by link id. This is how the control plane routes
    /// to a tenant's peers.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn entries_for(&self, party: &Party) -> Result<Vec<RegistryEntry>> {
        let conn = self.conn().await?;
        let text = party.canonical();
        let rows = conn
            .query(
                "SELECT link_id, tenant_a, tenant_b, state FROM spot_control.link_registry \
                 WHERE tenant_a = $1 OR tenant_b = $1 ORDER BY link_id",
                &[&text],
            )
            .await
            .map_err(query_failed)?;
        rows.iter()
            .map(|row| {
                let id: uuid::Uuid = row.get(0);
                let link = LinkId::from_uuid(id)
                    .map_err(|_| store_failed("a registry row is not valid"))?;
                decode_columns(link, row.get(1), row.get(2), row.get(3))
            })
            .collect()
    }
}

fn decode(link: LinkId, row: &tokio_postgres::Row) -> Result<RegistryEntry> {
    decode_columns(link, row.get(0), row.get(1), row.get(2))
}

fn decode_columns(link: LinkId, a: String, b: String, state: String) -> Result<RegistryEntry> {
    let invalid = || store_failed("a registry row is not valid");
    Ok(RegistryEntry {
        link,
        tenant_a: Party::try_from(a).map_err(|_| invalid())?,
        tenant_b: Party::try_from(b).map_err(|_| invalid())?,
        state: RegistryState::parse(&state).ok_or_else(invalid)?,
    })
}
