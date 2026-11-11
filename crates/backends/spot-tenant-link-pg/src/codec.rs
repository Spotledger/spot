//! Rows to links and back.
//!
//! A link is stored as one `link` row plus child rows, each child carrying its searchable
//! columns (topic, direction, actor, party) beside the JSON document of the type itself. A row
//! that does not decode is a store failure, never a default: a corrupt link fails closed.

use std::collections::BTreeMap;

use deadpool_postgres::Transaction;
use spot_tenant_link::{
    Carrier, Consent, Error, Grant, Link, LinkId, LinkState, Party, PeerKey, Result, Revocation,
    RevokeReason,
};
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::{query_failed, store_failed};

pub(crate) const LIVE_FILTER: &str = "state NOT IN ('revoked', 'expired')";

const LINK_COLUMNS: &str = "id, party_a, party_b, local, carrier, state, created, expires, \
                            revoked_by, revoke_reason, revoked_at, version";

fn corrupt() -> Error {
    store_failed("a stored link row is not valid")
}

pub(crate) fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| store_failed("a time or version value is out of range"))
}

pub(crate) fn to_u64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt())
}

fn parse_state(text: &str) -> Result<LinkState> {
    Ok(match text {
        "proposed" => LinkState::Proposed,
        "accepted_by_peer" => LinkState::AcceptedByPeer,
        "active" => LinkState::Active,
        "revoked" => LinkState::Revoked,
        "expired" => LinkState::Expired,
        _ => return Err(corrupt()),
    })
}

fn parse_carrier(text: &str) -> Result<Carrier> {
    Ok(match text {
        "direct" => Carrier::Direct,
        "pull" => Carrier::Pull,
        "external" => Carrier::External,
        _ => return Err(corrupt()),
    })
}

fn parse_reason(text: &str) -> Result<RevokeReason> {
    Ok(match text {
        "terms_ended" => RevokeReason::TermsEnded,
        "security_concern" => RevokeReason::SecurityConcern,
        "superseded" => RevokeReason::Superseded,
        _ => return Err(corrupt()),
    })
}

pub(crate) fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| store_failed("a link value could not be encoded"))
}

fn from_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|_| corrupt())
}

/// The scalar part of a link row.
struct Head {
    id: LinkId,
    parties: [Party; 2],
    local: Party,
    carrier: Carrier,
    state: LinkState,
    created: u64,
    expires: Option<u64>,
    revocation: Option<Revocation>,
    version: u64,
}

fn head(row: &Row) -> Result<Head> {
    let id: Uuid = row.get(0);
    let party = |index: usize| -> Result<Party> {
        Party::try_from(row.get::<_, String>(index)).map_err(|_| corrupt())
    };
    let revoked_by: Option<String> = row.get(8);
    let reason: Option<String> = row.get(9);
    let revoked_at: Option<i64> = row.get(10);
    let revocation = match (revoked_by, reason, revoked_at) {
        (Some(by), Some(reason), Some(at)) => Some(Revocation {
            by: Party::try_from(by).map_err(|_| corrupt())?,
            reason: parse_reason(&reason)?,
            at: to_u64(at)?,
        }),
        (None, None, None) => None,
        _ => return Err(corrupt()),
    };
    Ok(Head {
        id: LinkId::from_uuid(id).map_err(|_| corrupt())?,
        parties: [party(1)?, party(2)?],
        local: party(3)?,
        carrier: parse_carrier(&row.get::<_, String>(4))?,
        state: parse_state(&row.get::<_, String>(5))?,
        created: to_u64(row.get(6))?,
        expires: row.get::<_, Option<i64>>(7).map(to_u64).transpose()?,
        revocation,
        version: to_u64(row.get(11))?,
    })
}

/// Loads full links for the given `link` rows, with their children, in the rows' order.
pub(crate) async fn assemble(client: &Transaction<'_>, rows: &[Row]) -> Result<Vec<Link>> {
    let heads = rows.iter().map(head).collect::<Result<Vec<_>>>()?;
    if heads.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = heads.iter().map(|head| head.id.as_uuid()).collect();

    let mut grants: BTreeMap<Uuid, Vec<Grant>> = BTreeMap::new();
    for row in client
        .query(
            "SELECT link, body::text FROM spot_tenant_link_grant WHERE link = ANY($1) ORDER BY link, ord",
            &[&ids],
        )
        .await
        .map_err(query_failed)?
    {
        grants
            .entry(row.get(0))
            .or_default()
            .push(from_json(&row.get::<_, String>(1))?);
    }
    let mut consents: BTreeMap<Uuid, Vec<Consent>> = BTreeMap::new();
    for row in client
        .query(
            "SELECT link, body::text FROM spot_tenant_link_consent WHERE link = ANY($1) ORDER BY link, ord",
            &[&ids],
        )
        .await
        .map_err(query_failed)?
    {
        consents
            .entry(row.get(0))
            .or_default()
            .push(from_json(&row.get::<_, String>(1))?);
    }
    let mut keys: BTreeMap<Uuid, Vec<(Party, PeerKey)>> = BTreeMap::new();
    for row in client
        .query(
            "SELECT link, party, public_key FROM spot_tenant_link_peer_key WHERE link = ANY($1)",
            &[&ids],
        )
        .await
        .map_err(query_failed)?
    {
        let bytes: Vec<u8> = row.get(2);
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| corrupt())?;
        keys.entry(row.get(0)).or_default().push((
            Party::try_from(row.get::<_, String>(1)).map_err(|_| corrupt())?,
            PeerKey::from_bytes(bytes).map_err(|_| corrupt())?,
        ));
    }

    heads
        .into_iter()
        .map(|head| {
            let uuid = head.id.as_uuid();
            let mut peer_keys = [None, None];
            for (party, key) in keys.remove(&uuid).unwrap_or_default() {
                let slot = head
                    .parties
                    .iter()
                    .position(|candidate| *candidate == party)
                    .ok_or_else(corrupt)?;
                peer_keys[slot] = Some(key);
            }
            Ok(Link {
                id: head.id,
                parties: head.parties,
                local: head.local,
                carrier: head.carrier,
                state: head.state,
                created: head.created,
                peer_keys,
                grants: grants.remove(&uuid).unwrap_or_default(),
                consents: consents.remove(&uuid).unwrap_or_default(),
                revocation: head.revocation,
                expires: head.expires,
                version: head.version,
            })
        })
        .collect()
}

/// The `link` rows matching `where_clause` (an SQL fragment this crate writes, with `$n`
/// parameters), ordered by id.
pub(crate) async fn select_rows(
    client: &Transaction<'_>,
    where_clause: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    lock: bool,
) -> Result<Vec<Row>> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT {LINK_COLUMNS} FROM spot_tenant_link WHERE {where_clause} ORDER BY id{suffix}"
    );
    client
        .query(sql.as_str(), params)
        .await
        .map_err(query_failed)
}

/// Writes the child rows of `link` (grants, consents, keys). The caller has cleared them first
/// on an update.
pub(crate) async fn write_children(client: &Transaction<'_>, link: &Link) -> Result<()> {
    let id = link.id.as_uuid();
    for (ord, grant) in link.grants.iter().enumerate() {
        let ord = i32::try_from(ord).map_err(|_| store_failed("too many grants"))?;
        client
            .execute(
                "INSERT INTO spot_tenant_link_grant (link, ord, topic, direction, expires, body) \
                 VALUES ($1, $2, $3, $4, $5, $6::text::jsonb)",
                &[
                    &id,
                    &ord,
                    &grant.topic.as_str(),
                    &grant.direction.as_str(),
                    &to_i64(grant.expires)?,
                    &json(grant)?,
                ],
            )
            .await
            .map_err(query_failed)?;
    }
    for (ord, consent) in link.consents.iter().enumerate() {
        let ord = i32::try_from(ord).map_err(|_| store_failed("too many consents"))?;
        client
            .execute(
                "INSERT INTO spot_tenant_link_consent (link, ord, party, actor, at, body) \
                 VALUES ($1, $2, $3, $4, $5, $6::text::jsonb)",
                &[
                    &id,
                    &ord,
                    &consent.party.canonical(),
                    &consent.actor,
                    &to_i64(consent.at)?,
                    &json(consent)?,
                ],
            )
            .await
            .map_err(query_failed)?;
    }
    for (party, key) in link.parties.iter().zip(link.peer_keys.iter()) {
        if let Some(key) = key {
            client
                .execute(
                    "INSERT INTO spot_tenant_link_peer_key (link, party, public_key) VALUES ($1, $2, $3)",
                    &[&id, &party.canonical(), &key.as_bytes().as_slice()],
                )
                .await
                .map_err(query_failed)?;
        }
    }
    Ok(())
}

/// Clears the child rows of a link before they are rewritten.
pub(crate) async fn clear_children(client: &Transaction<'_>, id: &Uuid) -> Result<()> {
    for sql in [
        "DELETE FROM spot_tenant_link_grant WHERE link = $1",
        "DELETE FROM spot_tenant_link_consent WHERE link = $1",
        "DELETE FROM spot_tenant_link_peer_key WHERE link = $1",
    ] {
        client.execute(sql, &[id]).await.map_err(query_failed)?;
    }
    Ok(())
}

/// The revocation columns of a link.
pub(crate) fn revocation_columns(
    link: &Link,
) -> Result<(Option<String>, Option<&'static str>, Option<i64>)> {
    match &link.revocation {
        Some(revocation) => Ok((
            Some(revocation.by.canonical()),
            Some(revocation.reason.as_str()),
            Some(to_i64(revocation.at)?),
        )),
        None => Ok((None, None, None)),
    }
}
