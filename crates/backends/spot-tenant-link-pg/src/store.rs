//! The PostgreSQL [`LinkStore`]: one tenant's links, inside that tenant's own schema.
//!
//! # Isolation
//!
//! A store is built for one [`TenantId`]. Every transaction starts by making that tenant's
//! schema (`t_<id>`, derived from the id and never from a tenant-chosen string) the only entry
//! of `search_path`, transaction-locally, so the statements — all of them parameterised, none
//! built from caller text — can see no other tenant's rows even if a caller passes another
//! tenant's link id. A link whose local side is not this tenant is refused on the way in and on
//! the way out. A tenant that holds a link with another tenant keeps **its own copy** in its own
//! schema; the two copies share the link id and nothing else.
//!
//! # Versions and failures
//!
//! `insert` assigns version 1; `update` locks the row, checks the expected version, the
//! immutable fields (parties, local side, creation time) and the state machine, then bumps the
//! version, so two administrators cannot silently overwrite each other's revocation. A live
//! pair is unique by a partial index. Every failure is a fixed-reason
//! [`Error::StoreFailed`]; none carries SQL, a driver message or a value.
//!
//! # Registry and invalidation
//!
//! A committed change marks the link `registry_pending`, then tries to write the control
//! registry and clear the mark. If the control database is unreachable the change stays
//! committed and pending; [`PgLinkStore::reconcile_registry`] finishes the job. Registry state
//! never gates an exchange — the tenant's own row does. After the commit the store calls its
//! [`LinkInvalidation`] hook so readers' caches drop the link.

use std::sync::Arc;

use deadpool_postgres::{Object, Pool, Transaction};
use spot_app_api::AppResult;
use spot_tenancy::TenantId;
use spot_tenant_link::{
    AsyncNonceClaimer, ConsentPolicy, Error, HandshakeBody, HandshakeMessage, Link, LinkId,
    LinkStore, Nonce, NonceClaim, Offer, Party, PeerKeyDirectory, Result, Trust, check_peer_key,
    claim_with_async,
};
use tokio_postgres::IsolationLevel;
use uuid::Uuid;

use crate::blocking::block_on;
use crate::cache::LinkInvalidation;
use crate::codec::{
    LIVE_FILTER, assemble, clear_children, json, revocation_columns, select_rows, to_i64, to_u64,
    write_children,
};
use crate::error::{is_unique_violation, query_failed, store_failed};
use crate::registry::{PgLinkRegistry, RegistryEntry};
use crate::schema::prepare_tenant;

/// The link store of one tenant.
pub struct PgLinkStore {
    pub(crate) pool: Pool,
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
    tenant: TenantId,
    pub(crate) schema: String,
    registry: Option<Arc<PgLinkRegistry>>,
    invalidation: Option<Arc<dyn LinkInvalidation>>,
}

impl std::fmt::Debug for PgLinkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLinkStore")
            .field("tenant", &self.tenant.to_string())
            .finish_non_exhaustive()
    }
}

impl PgLinkStore {
    /// A store for `tenant` on `pool`, the pool of the database that holds the tenant's schema.
    /// It touches the database only when [`prepare`](Self::prepare) or a method is called.
    ///
    /// `runtime` is what the synchronous trait methods block on. Do not call them from a
    /// current-thread runtime.
    #[must_use]
    pub fn new(pool: Pool, runtime: Arc<tokio::runtime::Runtime>, tenant: TenantId) -> Self {
        let schema = tenant.schema_name();
        Self {
            pool,
            runtime,
            tenant,
            schema,
            registry: None,
            invalidation: None,
        }
    }

    /// Writes the control-plane registry entry for every committed change.
    #[must_use]
    pub fn with_registry(mut self, registry: Arc<PgLinkRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Calls `hook` after every committed change, so caches drop the link.
    #[must_use]
    pub fn with_invalidation(mut self, hook: Arc<dyn LinkInvalidation>) -> Self {
        self.invalidation = Some(hook);
        self
    }

    /// The tenant this store belongs to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The party this store holds links for.
    #[must_use]
    pub fn local_party(&self) -> Party {
        Party::Tenant(self.tenant)
    }

    /// Creates the link tables in the tenant's schema if they are missing. Idempotent and safe
    /// from several nodes at once. The tenant's schema must already exist.
    ///
    /// # Errors
    /// `tenant_link_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        prepare_tenant(&self.pool, &self.schema).await
    }

    /// The durable handshake ports for this tenant: the nonce log and the peer key directory,
    /// both over this store's pool and schema. A production host takes both from this call and
    /// never wires `MemoryNonceLog` or `MemoryPeerKeyDirectory`.
    #[must_use]
    pub fn handshake_ports(&self) -> (crate::ports::PgNonceLog, crate::ports::PgPeerKeyDirectory) {
        let copy = || Self::new(self.pool.clone(), Arc::clone(&self.runtime), self.tenant);
        (
            crate::ports::PgNonceLog::new(copy()),
            crate::ports::PgPeerKeyDirectory::new(copy()),
        )
    }

    /// Blocks on one of this store's async methods from synchronous code (the handshake and
    /// the `LinkStore` trait are synchronous). From inside a multi-thread runtime it uses
    /// `block_in_place`; do not call it from a current-thread runtime.
    pub fn run<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        block_on(&self.runtime, future)
    }

    /// Stores a new link together with the local side's offer, in one transaction, so a link
    /// never exists without the offer a later accept or grant is checked against.
    ///
    /// # Errors
    /// As [`LinkStore::insert`], plus [`Error::InvalidOffer`] when the offer's side is not a
    /// party of the link.
    pub async fn insert_with_offer(&self, link: Link, offer: &Offer) -> Result<Link> {
        self.insert_inner(link, Some(offer)).await
    }

    pub(crate) async fn conn(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|_| store_failed("no link store connection is available"))
    }

    /// Begins a transaction scoped to the tenant's schema.
    pub(crate) async fn scoped<'c>(
        &self,
        conn: &'c mut Object,
        read_only: bool,
    ) -> Result<Transaction<'c>> {
        let tx = if read_only {
            conn.build_transaction()
                .isolation_level(IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .await
        } else {
            conn.transaction().await
        }
        .map_err(|_| store_failed("the link store transaction could not begin"))?;
        tx.query_one(
            "SELECT set_config('search_path', $1, true)",
            &[&self.schema],
        )
        .await
        .map_err(query_failed)?;
        Ok(tx)
    }

    fn check_local(&self, link: &Link) -> Result<()> {
        if link.local != self.local_party() {
            return Err(Error::InvalidParty {
                reason: "a tenant stores only links it is a party of",
            });
        }
        Ok(())
    }

    async fn insert_inner(&self, mut link: Link, offer: Option<&Offer>) -> Result<Link> {
        link.validate()?;
        self.check_local(&link)?;
        if link.version != 0 {
            return Err(store_failed("a new link is not already versioned"));
        }
        link.version = 1;
        let id = link.id.as_uuid();
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        let (revoked_by, reason, revoked_at) = revocation_columns(&link)?;
        let inserted = tx
            .execute(
                "INSERT INTO spot_tenant_link (id, party_a, party_b, local, carrier, state, created, expires, \
                                   revoked_by, revoke_reason, revoked_at, version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                &[
                    &id,
                    &link.parties[0].canonical(),
                    &link.parties[1].canonical(),
                    &link.local.canonical(),
                    &link.carrier.as_str(),
                    &link.state.as_str(),
                    &to_i64(link.created)?,
                    &link.expires.map(to_i64).transpose()?,
                    &revoked_by,
                    &reason,
                    &revoked_at,
                    &1_i64,
                ],
            )
            .await;
        match inserted {
            Ok(_) => {}
            Err(error) if is_unique_violation(&error) => {
                return Err(Error::DuplicateLink {
                    link: link.id.to_string(),
                });
            }
            Err(error) => return Err(query_failed(error)),
        }
        write_children(&tx, &link).await?;
        if let Some(offer) = offer {
            if link.party_index(&offer.side).is_none() {
                return Err(Error::InvalidOffer {
                    reason: "an offer is made by one of the link's parties",
                });
            }
            write_offer(&tx, &link.id, offer).await?;
        }
        tx.commit().await.map_err(query_failed)?;
        self.after_commit(&link).await;
        Ok(link)
    }

    /// Replaces a link if the stored version is `expected`, **and** keeps `offer` (the local
    /// side's offer a state change such as an acceptance is checked against), in one
    /// transaction: either the version-checked change and the offer both land, or neither does.
    ///
    /// # Errors
    /// As [`LinkStore::update`], plus [`Error::InvalidOffer`] when the offer's side is not a
    /// party of the link.
    pub async fn update_with_offer(
        &self,
        link: Link,
        expected: u64,
        offer: &Offer,
    ) -> Result<Link> {
        self.update_inner(link, expected, Some(offer)).await
    }

    async fn update_inner(
        &self,
        mut link: Link,
        expected: u64,
        offer: Option<&Offer>,
    ) -> Result<Link> {
        link.validate()?;
        self.check_local(&link)?;
        let id = link.id.as_uuid();
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        let rows = select_rows(&tx, "id = $1", &[&id], true).await?;
        let stored = assemble(&tx, &rows)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound {
                link: link.id.to_string(),
            })?;
        self.check_local(&stored)?;
        if stored.version != expected {
            return Err(Error::VersionConflict {
                link: link.id.to_string(),
                expected,
                found: stored.version,
            });
        }
        if stored.parties != link.parties
            || stored.local != link.local
            || stored.created != link.created
        {
            return Err(Error::InvalidParty {
                reason: "a link's parties, holder and creation time never change",
            });
        }
        if stored.state != link.state && !stored.state.can_become(link.state) {
            return Err(Error::InvalidTransition {
                link: link.id.to_string(),
                from: stored.state,
                to: link.state,
            });
        }
        link.version = expected
            .checked_add(1)
            .ok_or_else(|| store_failed("version overflow"))?;
        let (revoked_by, reason, revoked_at) = revocation_columns(&link)?;
        let updated = tx
            .execute(
                "UPDATE spot_tenant_link SET state = $2, expires = $3, revoked_by = $4, revoke_reason = $5, \
                        revoked_at = $6, version = $7, registry_pending = true \
                 WHERE id = $1 AND version = $8",
                &[
                    &id,
                    &link.state.as_str(),
                    &link.expires.map(to_i64).transpose()?,
                    &revoked_by,
                    &reason,
                    &revoked_at,
                    &to_i64(link.version)?,
                    &to_i64(expected)?,
                ],
            )
            .await
            .map_err(query_failed)?;
        if updated != 1 {
            return Err(Error::VersionConflict {
                link: link.id.to_string(),
                expected,
                found: stored.version,
            });
        }
        clear_children(&tx, &id).await?;
        write_children(&tx, &link).await?;
        if let Some(offer) = offer {
            if link.party_index(&offer.side).is_none() {
                return Err(Error::InvalidOffer {
                    reason: "an offer is made by one of the link's parties",
                });
            }
            write_offer(&tx, &link.id, offer).await?;
        }
        tx.commit().await.map_err(query_failed)?;
        self.after_commit(&link).await;
        Ok(link)
    }

    /// Registry write and cache invalidation after a commit. Neither can undo the commit.
    async fn after_commit(&self, link: &Link) {
        if let Some(hook) = &self.invalidation {
            hook.invalidate(&link.id);
        }
        if self.sync_registry(link).await.is_err() {
            // The change stays pending; `reconcile_registry` finishes it.
        }
    }

    async fn sync_registry(&self, link: &Link) -> Result<()> {
        let Some(registry) = &self.registry else {
            return Ok(());
        };
        registry.upsert(&RegistryEntry::of(link)).await?;
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        tx.execute(
            "UPDATE spot_tenant_link SET registry_pending = false WHERE id = $1 AND version = $2",
            &[&link.id.as_uuid(), &to_i64(link.version)?],
        )
        .await
        .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)
    }

    /// Writes the registry entry of every link whose last change did not reach the control
    /// database, and returns how many it brought up to date.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] when the registry or the tenant schema is unreachable; the links
    /// stay pending.
    pub async fn reconcile_registry(&self) -> Result<usize> {
        if self.registry.is_none() {
            return Ok(0);
        }
        let links = {
            let mut conn = self.conn().await?;
            let tx = self.scoped(&mut conn, true).await?;
            let rows = select_rows(&tx, "registry_pending", &[], false).await?;
            assemble(&tx, &rows).await?
        };
        let mut done = 0;
        for link in &links {
            self.sync_registry(link).await?;
            done += 1;
        }
        Ok(done)
    }

    /// The ids of links whose registry entry is not yet written.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn registry_pending(&self) -> Result<Vec<LinkId>> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let rows = tx
            .query(
                "SELECT id FROM spot_tenant_link WHERE registry_pending ORDER BY id",
                &[],
            )
            .await
            .map_err(query_failed)?;
        rows.iter()
            .map(|row| {
                LinkId::from_uuid(row.get::<_, Uuid>(0))
                    .map_err(|_| store_failed("a stored link row is not valid"))
            })
            .collect()
    }

    /// Keeps one side's offer for a link, so a later accept or grant is checked against both
    /// sides. The link must exist in this tenant's schema.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::InvalidOffer`] when the offer's side is not a party of the
    /// link, or [`Error::StoreFailed`].
    pub async fn put_offer(&self, link: &LinkId, offer: &Offer) -> Result<()> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        let rows = select_rows(&tx, "id = $1", &[&link.as_uuid()], true).await?;
        let stored = assemble(&tx, &rows)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound {
                link: link.to_string(),
            })?;
        self.check_local(&stored)?;
        if stored.party_index(&offer.side).is_none() {
            return Err(Error::InvalidOffer {
                reason: "an offer is made by one of the link's parties",
            });
        }
        write_offer(&tx, link, offer).await?;
        tx.commit().await.map_err(query_failed)
    }

    /// The offers kept for a link: `(local side's, peer's)`, either of which may be missing.
    ///
    /// # Errors
    /// [`Error::NotFound`] or [`Error::StoreFailed`].
    pub async fn offers(&self, link: &LinkId) -> Result<(Option<Offer>, Option<Offer>)> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let rows = select_rows(&tx, "id = $1", &[&link.as_uuid()], false).await?;
        let stored = assemble(&tx, &rows)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound {
                link: link.to_string(),
            })?;
        let mut local = None;
        let mut peer = None;
        for row in tx
            .query(
                "SELECT side, body::text FROM spot_tenant_link_offer WHERE link = $1",
                &[&link.as_uuid()],
            )
            .await
            .map_err(query_failed)?
        {
            let side = Party::try_from(row.get::<_, String>(0))
                .map_err(|_| store_failed("a stored offer is not valid"))?;
            let offer: Offer = serde_json::from_str(&row.get::<_, String>(1))
                .map_err(|_| store_failed("a stored offer is not valid"))?;
            if side == stored.local {
                local = Some(offer);
            } else {
                peer = Some(offer);
            }
        }
        Ok((local, peer))
    }

    /// Screens one inbound handshake message and, only if it is genuine, claims its nonce. The
    /// host calls this for every inbound handshake message **before** handing it to
    /// [`Handshake::receive`](spot_tenant_link::Handshake::receive) or
    /// [`from_proposal`](spot_tenant_link::Handshake::from_proposal).
    ///
    /// The nonce is claimed last. Before any row is written the message must
    ///
    /// 1. carry a signature that verifies against the key it names;
    /// 2. name a link: a proposal must be addressed to this tenant, and any later message must
    ///    belong to a link this tenant already holds, so an unknown link id is refused;
    /// 3. be signed by the peer's key: the key recorded on the link, or, where the link holds
    ///    none yet (a proposal, or the invitee's acceptance), a key `keys` pins or `trust`
    ///    confirms (the handshake's own rule, [`check_peer_key`]).
    ///
    /// An outsider who cannot sign as the peer therefore cannot add a row. This is the only way
    /// to claim a nonce through the public API. The returned [`NonceClaim`] names this tenant's
    /// link and the nonce; pass it to `Handshake::receive_claimed` (or `from_proposal_claimed`)
    /// so the handshake does not record the nonce a second time, and call
    /// [`release`](NonceClaim::release) when the nonce is given back. The claim itself is atomic; a failed claim is a
    /// refusal, so a store outage fails closed.
    ///
    /// # Errors
    /// [`Error::BadSignature`], [`Error::WrongLink`], [`Error::InvalidParty`],
    /// [`Error::NotFound`], [`Error::WrongSigner`], [`Error::KeyMismatch`],
    /// [`Error::KeyUnpinned`], [`Error::Replayed`] when the nonce was seen, or
    /// [`Error::StoreFailed`].
    pub async fn claim_inbound(
        &self,
        message: &HandshakeMessage,
        keys: &dyn PeerKeyDirectory,
        trust: &Trust,
        at: u64,
    ) -> Result<NonceClaim> {
        message.verify()?;
        let link_id = *message.body.link().ok_or_else(|| Error::WrongLink {
            expected: "a link".to_owned(),
            found: "none".to_owned(),
        })?;
        if let HandshakeBody::Propose(body) = &message.body {
            if body.draft.peer != self.local_party() {
                return Err(Error::InvalidParty {
                    reason: "a proposal is addressed to this tenant",
                });
            }
            check_peer_key(keys, trust, &body.draft.local, message.signer)?;
        } else {
            let link = self.get_inner(&link_id).await?;
            let peer = link.peer().ok_or(Error::InvalidParty {
                reason: "the link's local side is not one of its parties",
            })?;
            let index = link.party_index(peer).ok_or(Error::InvalidParty {
                reason: "the peer is one of the link's parties",
            })?;
            match link.peer_keys[index] {
                Some(recorded) if recorded == message.signer => {}
                Some(_) => {
                    return Err(Error::WrongSigner {
                        expected: "the key recorded for the peer on this link".to_owned(),
                        found: "another key".to_owned(),
                    });
                }
                None => check_peer_key(keys, trust, peer, message.signer)?,
            }
        }
        let primitive = InboundClaimer {
            store: self,
            at,
            revision: message.revision,
        };
        claim_with_async(&primitive, &link_id, &message.nonce)
            .await?
            .ok_or(Error::Replayed {
                revision: message.revision,
            })
    }

    /// Claims `nonce` for `link`, refusing it if it was seen before, across restarts. Crate
    /// private on purpose: a nonce is claimed only for a message that was verified first
    /// ([`claim_inbound`](Self::claim_inbound), `LinkAdmin::receive_revoke`).
    pub(crate) async fn claim_nonce(
        &self,
        link: &LinkId,
        nonce: &Nonce,
        at: u64,
        revision: u64,
    ) -> Result<()> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        let claimed = tx
            .execute(
                "INSERT INTO spot_tenant_link_nonce (link, nonce, seen_at) VALUES ($1, $2, $3) \
                 ON CONFLICT (link, nonce) DO NOTHING",
                &[&link.as_uuid(), &nonce.as_bytes().as_slice(), &to_i64(at)?],
            )
            .await
            .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)?;
        if claimed == 0 {
            return Err(Error::Replayed { revision });
        }
        Ok(())
    }

    /// Whether the nonce is held for `link` in this tenant's table.
    pub(crate) async fn holds_nonce(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let rows = tx
            .query(
                "SELECT 1 FROM spot_tenant_link_nonce WHERE link = $1 AND nonce = $2",
                &[&link.as_uuid(), &nonce.as_bytes().as_slice()],
            )
            .await
            .map_err(query_failed)?;
        Ok(!rows.is_empty())
    }

    /// Gives back a nonce claimed for a revoke whose commit then failed, so a resend is not
    /// refused as a replay. Crate private, like the claim.
    pub(crate) async fn release_nonce(&self, link: &LinkId, nonce: &Nonce) -> Result<()> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        tx.execute(
            "DELETE FROM spot_tenant_link_nonce WHERE link = $1 AND nonce = $2",
            &[&link.as_uuid(), &nonce.as_bytes().as_slice()],
        )
        .await
        .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)
    }

    /// Deletes the nonces recorded more than the replay window before `now`, in one statement,
    /// and returns how many it removed.
    ///
    /// The window is the handshake's own: the maximum age of a consent
    /// ([`ConsentPolicy::max_age_ms`]). A captured `propose` or `accept` older than that fails
    /// the consent age check whatever its nonce, every other handshake message is bound to the
    /// transcript position the handshake keeps (`Handshake::resume` restores it), and a revoke
    /// ends the link for good. No other time bound exists to read, so none is invented here. A
    /// nonce recorded exactly at the cut-off, or later, is kept; pass the same `policy` the
    /// handshake runs under.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn prune_nonces(&self, now: u64, policy: ConsentPolicy) -> Result<u64> {
        let cutoff = now.saturating_sub(policy.max_age_ms());
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        let removed = tx
            .execute(
                "DELETE FROM spot_tenant_link_nonce WHERE seen_at < $1",
                &[&to_i64(cutoff)?],
            )
            .await
            .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)?;
        Ok(removed)
    }

    /// How many handshake nonces are kept for a link.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn nonce_count(&self, link: &LinkId) -> Result<u64> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let row = tx
            .query_one(
                "SELECT count(*) FROM spot_tenant_link_nonce WHERE link = $1",
                &[&link.as_uuid()],
            )
            .await
            .map_err(query_failed)?;
        to_u64(row.get(0))
    }

    async fn get_inner(&self, id: &LinkId) -> Result<Link> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let rows = select_rows(&tx, "id = $1", &[&id.as_uuid()], false).await?;
        let link = assemble(&tx, &rows)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound {
                link: id.to_string(),
            })?;
        self.check_local(&link)?;
        Ok(link)
    }

    async fn find_inner(&self, first: &Party, second: &Party) -> Result<Option<Link>> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let (a, b) = (first.canonical(), second.canonical());
        let clause = format!(
            "{LIVE_FILTER} AND ((party_a = $1 AND party_b = $2) OR (party_a = $2 AND party_b = $1))"
        );
        let rows = select_rows(&tx, &clause, &[&a, &b], false).await?;
        let links = assemble(&tx, &rows).await?;
        links.iter().try_for_each(|link| self.check_local(link))?;
        Ok(links.into_iter().next())
    }

    async fn list_inner(&self, party: &Party) -> Result<Vec<Link>> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let text = party.canonical();
        let rows = select_rows(&tx, "party_a = $1 OR party_b = $1", &[&text], false).await?;
        let links = assemble(&tx, &rows).await?;
        links.iter().try_for_each(|link| self.check_local(link))?;
        Ok(links)
    }
}

impl LinkStore for PgLinkStore {
    fn insert(&self, link: Link) -> Result<Link> {
        block_on(&self.runtime, self.insert_inner(link, None))
    }

    fn get(&self, id: &LinkId) -> Result<Link> {
        block_on(&self.runtime, self.get_inner(id))
    }

    fn update(&self, link: Link, expected_version: u64) -> Result<Link> {
        block_on(
            &self.runtime,
            self.update_inner(link, expected_version, None),
        )
    }

    fn find(&self, first: &Party, second: &Party) -> Result<Option<Link>> {
        block_on(&self.runtime, self.find_inner(first, second))
    }

    fn list_for(&self, party: &Party) -> Result<Vec<Link>> {
        block_on(&self.runtime, self.list_inner(party))
    }
}

/// Writes (or replaces) one side's offer for a link.
async fn write_offer(client: &Transaction<'_>, link: &LinkId, offer: &Offer) -> Result<()> {
    client
        .execute(
            "INSERT INTO spot_tenant_link_offer (link, side, body) VALUES ($1, $2, $3::text::jsonb) \
             ON CONFLICT (link, side) DO UPDATE SET body = EXCLUDED.body",
            &[&link.as_uuid(), &offer.side.canonical(), &json(offer)?],
        )
        .await
        .map_err(query_failed)?;
    Ok(())
}

/// The store primitive `claim_inbound` mints its claim from: the atomic insert of
/// [`PgLinkStore::claim_nonce`], with a replay read as "not inserted". A claim is built only by
/// `claim_with_async` over this value (WP-CTL-09.T7).
struct InboundClaimer<'a> {
    store: &'a PgLinkStore,
    at: u64,
    revision: u64,
}

impl AsyncNonceClaimer for InboundClaimer<'_> {
    async fn insert_new(&self, link: &LinkId, nonce: &Nonce) -> Result<bool> {
        match self
            .store
            .claim_nonce(link, nonce, self.at, self.revision)
            .await
        {
            Ok(()) => Ok(true),
            Err(Error::Replayed { .. }) => Ok(false),
            Err(other) => Err(other),
        }
    }
}
