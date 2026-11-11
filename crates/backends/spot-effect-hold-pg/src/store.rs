//! [`PgLedger`]: the durable [`LedgerStore`] over the four tables in [`crate::schema`].

use deadpool_postgres::{Object, Pool, Transaction};
use spot_app_api::AppResult;
use spot_core::SiteId;
use spot_effect_hold::store::LedgerFuture;
use spot_effect_hold::{
    EffectId, EffectRecord, EffectState, Error, HoldState, LedgerStore, MigrationId, NewEffect,
    ReleaseCursor, Result, Sequence, Transition,
};

use crate::convert;
use crate::error;
use crate::schema;
use crate::sql;

/// The side-effect hold's ledger in PostgreSQL.
///
/// See the crate documentation for what it guarantees. Call [`PgLedger::prepare`] once at start-up;
/// a ledger shares its pool with every clone.
#[derive(Clone)]
pub struct PgLedger {
    pool: Pool,
}

impl std::fmt::Debug for PgLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLedger").finish_non_exhaustive()
    }
}

impl PgLedger {
    /// A ledger on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the ledger's tables and indexes if they are missing. Idempotent; safe from several
    /// nodes at once.
    ///
    /// # Errors
    /// `effect_hold_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self, operation: &str) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|e| error::pool(operation, &e))
    }

    /// Appends a held effect, deduplicating on `(site, key)` in one transaction.
    ///
    /// The existence check comes first so a duplicate consumes no sequence number; the unique
    /// constraint still decides a race, where the insert does nothing and the committed row's id
    /// is returned. The per-site sequence is allocated in the same transaction as the insert, so
    /// the order rows become visible in is the order [`LedgerStore::list_effects`] returns them.
    async fn append(&self, effect: &NewEffect) -> Result<EffectId> {
        let mut client = self.conn("append_effect").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|e| error::pg("append_effect", &e))?;
        let site = effect.effect.site.as_str();
        let key = effect.effect.key.as_str();

        if let Some(id) = existing(&tx, site, key).await? {
            commit(tx).await?;
            return Ok(id);
        }

        let sequence = convert::sequence_param(next_sequence(&tx, site).await?)?;
        let class = convert::class_to_json(&effect.effect.class)?;
        let class_key = effect.effect.class.key();
        let payload = convert::payload_to_json(&effect.effect.payload_ref)?;
        let status = convert::state_text(EffectState::Held);
        let note: Option<String> = None;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 13] = [
            &site,
            &sequence,
            &key,
            &class,
            &class_key,
            &effect.effect.kind,
            &effect.effect.source,
            &effect.effect.source_event,
            &effect.effect.created_at,
            &payload,
            &effect.migration_id.as_str(),
            &status,
            &note,
        ];
        let inserted = tx
            .query_opt(sql::INSERT_EFFECT, &params)
            .await
            .map_err(|e| error::pg("append_effect", &e))?;
        let id = match inserted {
            Some(row) => convert::effect_id(row.get("id"))?,
            // Another transaction inserted this key between the check and the insert. The
            // conflicting insert waited for it to commit, so its row is visible here.
            None => existing(&tx, site, key).await?.ok_or_else(|| {
                error::corrupt(format!(
                    "the effect for site {site} and key {key:?} is neither stored nor insertable"
                ))
            })?,
        };
        commit(tx).await?;
        Ok(id)
    }
}

/// Commits `tx`, reporting a failure as the operation that was being finished.
async fn commit(tx: Transaction<'_>) -> Result<()> {
    tx.commit()
        .await
        .map_err(|e| error::pg("append_effect", &e))
}

/// The id already stored for `(site, key)`, if any.
async fn existing(tx: &Transaction<'_>, site: &str, key: &str) -> Result<Option<EffectId>> {
    let row = tx
        .query_opt(sql::SELECT_EFFECT_ID, &[&site, &key])
        .await
        .map_err(|e| error::pg("append_effect", &e))?;
    row.map(|row| convert::effect_id(row.get("id"))).transpose()
}

/// The next sequence number for `site`.
async fn next_sequence(tx: &Transaction<'_>, site: &str) -> Result<Sequence> {
    let row = tx
        .query_one(sql::NEXT_SEQUENCE, &[&site])
        .await
        .map_err(|e| error::pg("append_effect", &e))?;
    convert::sequence(row.get("last_sequence"))
}

impl LedgerStore for PgLedger {
    fn append_effect<'a>(&'a self, effect: NewEffect) -> LedgerFuture<'a, EffectId> {
        Box::pin(async move { self.append(&effect).await })
    }

    fn list_effects<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
    ) -> LedgerFuture<'a, Vec<EffectRecord>> {
        Box::pin(async move {
            let client = self.conn("list_effects").await?;
            let site = site.as_str();
            let wanted: Vec<String> = states
                .iter()
                .map(|state| convert::state_text(*state).to_owned())
                .collect();
            let rows = client
                .query(sql::LIST_EFFECTS, &[&site, &wanted])
                .await
                .map_err(|e| error::pg("list_effects", &e))?;
            rows.iter().map(convert::effect_from_row).collect()
        })
    }

    fn transition_effect<'a>(
        &'a self,
        id: EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> LedgerFuture<'a, Transition> {
        Box::pin(async move {
            spot_effect_hold::effect::validate_transition(expected)?;
            let client = self.conn("transition_effect").await?;
            let id_param = convert::effect_id_param(id)?;
            let attempt = i64::from(next == EffectState::Releasing);
            // One statement: the row changes only if it is still in `expected`.
            let changed = client
                .execute(
                    sql::TRANSITION_EFFECT,
                    &[
                        &id_param,
                        &convert::state_text(expected),
                        &convert::state_text(next),
                        &note,
                        &attempt,
                    ],
                )
                .await
                .map_err(|e| error::pg("transition_effect", &e))?;
            if changed == 1 {
                return Ok(Transition::Applied);
            }
            // Nothing changed: someone else acted first, or the effect does not exist.
            let row = client
                .query_opt(sql::SELECT_EFFECT_STATUS, &[&id_param])
                .await
                .map_err(|e| error::pg("transition_effect", &e))?;
            match row {
                Some(row) => Ok(Transition::Stale {
                    actual: convert::state_from_text(&row.get::<_, String>("status"))?,
                }),
                None => Err(Error::InvalidInput {
                    what: format!("effect id {}", id.0),
                    reason: "is not in the ledger".to_owned(),
                }),
            }
        })
    }

    fn get_hold<'a>(&'a self, site: &'a SiteId) -> LedgerFuture<'a, HoldState> {
        Box::pin(async move {
            let client = self.conn("get_hold").await?;
            let row = client
                .query_opt(sql::SELECT_HOLD, &[&site.as_str()])
                .await
                .map_err(|e| error::pg("get_hold", &e))?;
            match row {
                // A site with no row has never been held. That is known, not assumed: the read
                // succeeded and the row is absent.
                None => Ok(HoldState::Open),
                Some(row) => convert::hold_from_json(&row.get::<_, String>("state")),
            }
        })
    }

    fn compare_and_set_hold<'a>(
        &'a self,
        site: &'a SiteId,
        expected: &'a HoldState,
        new: HoldState,
    ) -> LedgerFuture<'a, bool> {
        Box::pin(async move {
            let client = self.conn("compare_and_set_hold").await?;
            let site = site.as_str();
            let is_open = matches!(expected, HoldState::Open);
            let new = convert::hold_to_json(&new)?;
            let expected = convert::hold_to_json(expected)?;
            // Only a site that was never held may be created by a compare-and-set, and its
            // implicit state is `Open`. Every other expected state must already be stored;
            // otherwise a `Held -> …` compare-and-set on an unknown site would "succeed" by
            // inserting a row whose expected state never existed.
            let changed = if is_open {
                client
                    .execute(sql::CAS_INSERT_HOLD, &[&site, &new, &expected])
                    .await
                    .map_err(|e| error::pg("compare_and_set_hold", &e))?
            } else {
                client
                    .execute(sql::CAS_UPDATE_HOLD, &[&site, &new, &expected])
                    .await
                    .map_err(|e| error::pg("compare_and_set_hold", &e))?
            };
            Ok(changed == 1)
        })
    }

    fn checkpoint<'a>(&'a self, site: &'a SiteId, cursor: ReleaseCursor) -> LedgerFuture<'a, ()> {
        Box::pin(async move {
            let client = self.conn("checkpoint").await?;
            let dispatched = cursor.dispatched.to_string();
            let last_sequence = cursor.last_sequence.map(|sequence| sequence.0.to_string());
            let needs_confirmation = cursor.needs_confirmation.to_string();
            client
                .execute(
                    sql::UPSERT_CHECKPOINT,
                    &[
                        &site.as_str(),
                        &cursor.migration_id.as_str(),
                        &dispatched,
                        &last_sequence,
                        &needs_confirmation,
                    ],
                )
                .await
                .map_err(|e| error::pg("checkpoint", &e))?;
            Ok(())
        })
    }

    fn get_checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        migration_id: &'a MigrationId,
    ) -> LedgerFuture<'a, Option<ReleaseCursor>> {
        Box::pin(async move {
            let client = self.conn("get_checkpoint").await?;
            let row = client
                .query_opt(
                    sql::SELECT_CHECKPOINT,
                    &[&site.as_str(), &migration_id.as_str()],
                )
                .await
                .map_err(|e| error::pg("get_checkpoint", &e))?;
            let Some(row) = row else {
                return Ok(None);
            };
            let dispatched = convert::count("dispatch count", &row.get::<_, String>("dispatched"))?;
            let last_sequence = match row.get::<_, Option<String>>("last_sequence") {
                Some(text) => Some(convert::count("last sequence", &text)?),
                None => None,
            };
            let needs_confirmation = convert::count(
                "needs-confirmation count",
                &row.get::<_, String>("needs_confirmation"),
            )?;
            Ok(Some(ReleaseCursor {
                migration_id: migration_id.clone(),
                dispatched,
                last_sequence: last_sequence.map(Sequence),
                needs_confirmation,
            }))
        })
    }
}
