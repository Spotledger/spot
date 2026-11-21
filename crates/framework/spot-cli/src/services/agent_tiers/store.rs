//! The durable stores of the agent tier service: registered agents, support sessions and the
//! registry's event log, in three tables of the site's own database.
//!
//! The tables are created by [`Store::prepare`] (idempotent `CREATE TABLE IF NOT EXISTS`) and every
//! statement carries the site name, so a row of one tenant is unreachable from another even if two
//! sites share a database. A store error is a plain sentence; it never carries a row, a grant or a
//! driver message (which can quote a value).

use deadpool_postgres::Pool;
use spot_agent_tiers::{Domain, Status, Tier};
use spot_support_session::SessionGrant;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS spot_agent_tier_registration (
    site       text    NOT NULL,
    agent_id   text    NOT NULL,
    owner      text    NOT NULL,
    domain     text    NOT NULL,
    max_tier   text    NOT NULL,
    deep       boolean NOT NULL DEFAULT false,
    status     text    NOT NULL DEFAULT 'active',
    updated_at bigint  NOT NULL,
    PRIMARY KEY (site, agent_id)
);
CREATE TABLE IF NOT EXISTS spot_agent_tier_support_session (
    site       text   NOT NULL,
    id         text   NOT NULL,
    subject_id text   NOT NULL,
    grant_json text   NOT NULL,
    updated_at bigint NOT NULL,
    PRIMARY KEY (site, id)
);
CREATE INDEX IF NOT EXISTS spot_agent_tier_support_session_subject
    ON spot_agent_tier_support_session (site, subject_id);
CREATE TABLE IF NOT EXISTS spot_agent_tier_event (
    seq          bigserial PRIMARY KEY,
    site         text   NOT NULL,
    event        text   NOT NULL,
    agent_id     text   NOT NULL,
    by_actor     text   NOT NULL,
    evidence_ref text   NOT NULL,
    detail       text   NOT NULL,
    at           bigint NOT NULL
);
";

/// One registered agent, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// The agent's id: the id of the user record it signs in as.
    pub agent_id: String,
    /// The person or organisation that answers for it.
    pub owner: String,
    /// Support or operations.
    pub domain: Domain,
    /// The highest tier the registration grants.
    pub max_tier: Tier,
    /// Registered deep (only through the registry's own event).
    pub deep: bool,
    /// Whether it may act at all.
    pub status: Status,
}

/// What the store could not do. The text is fixed; it never quotes stored data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreError(pub &'static str);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for StoreError {}

fn pool_err<E>(_: E) -> StoreError {
    StoreError("the database is not available")
}

/// Parses the stored text of a tier (`T0`..`T3`).
fn tier_of(text: &str) -> Option<Tier> {
    match text {
        "T0" => Some(Tier::T0),
        "T1" => Some(Tier::T1),
        "T2" => Some(Tier::T2),
        "T3" => Some(Tier::T3),
        _ => None,
    }
}

fn domain_of(text: &str) -> Option<Domain> {
    match text {
        "support" => Some(Domain::Support),
        "operations" => Some(Domain::Operations),
        _ => None,
    }
}

fn status_of(text: &str) -> Option<Status> {
    match text {
        "active" => Some(Status::Active),
        "suspended" => Some(Status::Suspended),
        "retired" => Some(Status::Retired),
        _ => None,
    }
}

/// The three tables of one site.
#[derive(Clone)]
pub struct Store {
    pool: Pool,
    site: String,
}

impl Store {
    /// A store over `pool` for `site`.
    #[must_use]
    pub fn new(pool: Pool, site: &str) -> Self {
        Self {
            pool,
            site: site.to_owned(),
        }
    }

    /// Creates the tables if they are not there.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be reached or refuses the DDL.
    pub async fn prepare(&self) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        client
            .batch_execute(DDL)
            .await
            .map_err(|_| StoreError("the agent tier tables could not be created"))
    }

    /// The registration of `agent_id`, if any. A row that does not parse is an error, never a
    /// guess: the caller fails closed.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails or the row is not one this service wrote.
    pub async fn agent(&self, agent_id: &str) -> Result<Option<Registration>, StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        let row = client
            .query_opt(
                "SELECT owner, domain, max_tier, deep, status FROM spot_agent_tier_registration \
                 WHERE site = $1 AND agent_id = $2",
                &[&self.site, &agent_id],
            )
            .await
            .map_err(|_| StoreError("the registration could not be read"))?;
        let Some(row) = row else { return Ok(None) };
        let corrupt = StoreError("a stored registration is not valid");
        let domain: String = row.get(1);
        let tier: String = row.get(2);
        let status: String = row.get(4);
        Ok(Some(Registration {
            agent_id: agent_id.to_owned(),
            owner: row.get(0),
            domain: domain_of(&domain).ok_or(corrupt)?,
            max_tier: tier_of(&tier).ok_or(corrupt)?,
            deep: row.get(3),
            status: status_of(&status).ok_or(corrupt)?,
        }))
    }

    /// Inserts or replaces a registration.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write fails.
    pub async fn put_agent(&self, reg: &Registration, now: i64) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        client
            .execute(
                "INSERT INTO spot_agent_tier_registration \
                 (site, agent_id, owner, domain, max_tier, deep, status, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (site, agent_id) DO UPDATE SET owner = $3, domain = $4, \
                 max_tier = $5, deep = $6, status = $7, updated_at = $8",
                &[
                    &self.site,
                    &reg.agent_id,
                    &reg.owner,
                    &reg.domain.as_str(),
                    &reg.max_tier.as_str(),
                    &reg.deep,
                    &reg.status.as_str(),
                    &now,
                ],
            )
            .await
            .map(|_| ())
            .map_err(|_| StoreError("the registration could not be written"))
    }

    /// Every session grant whose subject is `subject_id`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails or a stored grant does not parse.
    pub async fn sessions_of(&self, subject_id: &str) -> Result<Vec<SessionGrant>, StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        let rows = client
            .query(
                "SELECT grant_json FROM spot_agent_tier_support_session \
                 WHERE site = $1 AND subject_id = $2 ORDER BY id",
                &[&self.site, &subject_id],
            )
            .await
            .map_err(|_| StoreError("the sessions could not be read"))?;
        rows.iter()
            .map(|row| {
                let text: String = row.get(0);
                serde_json::from_str(&text).map_err(|_| StoreError("a stored session is not valid"))
            })
            .collect()
    }

    /// One session grant by id.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails or the grant does not parse.
    pub async fn session(&self, id: &str) -> Result<Option<SessionGrant>, StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        let row = client
            .query_opt(
                "SELECT grant_json FROM spot_agent_tier_support_session WHERE site = $1 AND id = $2",
                &[&self.site, &id],
            )
            .await
            .map_err(|_| StoreError("the session could not be read"))?;
        match row {
            None => Ok(None),
            Some(row) => {
                let text: String = row.get(0);
                serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|_| StoreError("a stored session is not valid"))
            }
        }
    }

    /// Inserts or replaces a session grant (its tenant must be this site).
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the grant is about another tenant or the write fails.
    pub async fn put_session(&self, grant: &SessionGrant, now: i64) -> Result<(), StoreError> {
        if grant.tenant != self.site {
            return Err(StoreError("the session is about another tenant"));
        }
        let text = serde_json::to_string(grant)
            .map_err(|_| StoreError("the session could not be encoded"))?;
        let client = self.pool.get().await.map_err(pool_err)?;
        client
            .execute(
                "INSERT INTO spot_agent_tier_support_session (site, id, subject_id, grant_json, updated_at) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (site, id) DO UPDATE SET subject_id = $3, grant_json = $4, updated_at = $5",
                &[&self.site, &grant.id, &grant.subject.id, &text, &now],
            )
            .await
            .map(|_| ())
            .map_err(|_| StoreError("the session could not be written"))
    }

    /// Appends one event to the log.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the write fails.
    pub async fn event(
        &self,
        event: &str,
        agent_id: &str,
        by: &str,
        evidence_ref: &str,
        detail: &str,
        at: i64,
    ) -> Result<(), StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        client
            .execute(
                "INSERT INTO spot_agent_tier_event (site, event, agent_id, by_actor, evidence_ref, detail, at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[&self.site, &event, &agent_id, &by, &evidence_ref, &detail, &at],
            )
            .await
            .map(|_| ())
            .map_err(|_| StoreError("the event could not be written"))
    }

    /// The logged events of one agent, oldest first, as `(event, at)`.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the read fails.
    pub async fn events_of(&self, agent_id: &str) -> Result<Vec<(String, i64)>, StoreError> {
        let client = self.pool.get().await.map_err(pool_err)?;
        let rows = client
            .query(
                "SELECT event, at FROM spot_agent_tier_event WHERE site = $1 AND agent_id = $2 ORDER BY seq",
                &[&self.site, &agent_id],
            )
            .await
            .map_err(|_| StoreError("the events could not be read"))?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }
}
