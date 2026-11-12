//! The blocking operations behind the read and yank routes, and the JSON they answer with.
//!
//! Every function here is synchronous and runs on a worker thread (see `pool`). The listing
//! fails closed: a version is public only with an explicit `listed` row in the state store, and
//! anything else (no row, under review, rejected, withdrawn, a state store that cannot answer)
//! answers exactly as an unknown version does, except to the owning publisher.

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use spot_bundle_signing::fresh::format_rfc3339;
use spot_registry::{
    AppName, BlobStore, IndexStore, PublisherStatus, Release, Removal, SearchQuery, Version,
    Visibility, collect_stream,
};
use spot_store_state::{AuditEntry, AuditKind, ListingState, MAX_CANDIDATES, ReviewStatus};

use crate::auth::{Principal, authenticate_sync, entitled};
use crate::error::{ApiError, Wire};
use crate::exec::block_on;
use crate::keys::{hex, is_publisher_token};
use crate::service::Core;

/// The most registry pages one search request reads (decision 2): the registry returns pages of
/// the newest release per app, the listing filter drops the unlisted ones, and a search refills
/// from the next registry page until its own page is full or this many were read.
pub(crate) const MAX_SEARCH_PAGES: usize = 5;

/// `text` cut to at most `max` characters.
pub(crate) fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// RFC 3339 UTC of `seconds`.
pub(crate) fn rfc3339(seconds: i64) -> Result<String, ApiError> {
    Ok(format_rfc3339(seconds)?)
}

/// `AppListing` of the contract.
pub(crate) fn listing_json(release: &Release) -> Result<Value, ApiError> {
    Ok(json!({
        "name": release.app.as_str(),
        "title": clip(&release.title, 128),
        "description": clip(&release.description, 2000),
        "category": clip(&release.category, 64),
        "publisher": release.publisher,
        "latest_version": release.version.to_string(),
        "published_at": rfc3339(release.published_at)?,
    }))
}

/// The opaque id of a release's stored signature: a digest over the release digest, the signing
/// key id and the signing time. It carries no key material.
pub(crate) fn signature_id(release: &Release) -> String {
    let signature = &release.signature_ref.signature;
    let mut hash = Sha256::new();
    for part in [
        release.digest.to_hex(),
        signature.key_id.as_str().to_owned(),
        signature.signed_at.to_string(),
    ] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hex(&hash.finalize()[..16])
}

/// `VersionDetail` of the contract.
pub(crate) fn version_json(release: &Release) -> Result<Value, ApiError> {
    let yanked = release.yanked.as_ref();
    Ok(json!({
        "app": release.app.as_str(),
        "version": release.version.to_string(),
        "digest": release.digest.to_hex(),
        "signature_id": signature_id(release),
        "size": release.size,
        "published_at": rfc3339(release.published_at)?,
        "requires_host": release.requires_host.as_ref().map(ToString::to_string),
        "yanked": yanked.is_some(),
        "yanked_reason": yanked.map(|removal| clip(&removal.reason, 2000)),
    }))
}

/// The parameters of one search request, already validated by the handler.
pub(crate) struct SearchParams {
    pub text: String,
    pub category: Option<String>,
    pub capability: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}

/// One page of public listings and the cursor of the next, when there is one.
pub(crate) struct SearchOutcome {
    pub releases: Vec<Release>,
    pub next_cursor: Option<String>,
}

fn query_fingerprint(params: &SearchParams) -> String {
    let mut hash = Sha256::new();
    for field in [
        params.text.clone(),
        format!("{:?}", params.category),
        format!("{:?}", params.capability),
        params.limit.to_string(),
    ] {
        hash.update((field.len() as u64).to_le_bytes());
        hash.update(field.as_bytes());
    }
    hex(&hash.finalize()[..8])
}

fn bad_cursor() -> ApiError {
    ApiError::invalid("the cursor is not valid for this query; restart the search")
}

/// The cursor is `<query hash>.<skip>.<registry cursor>`: the registry cursor that starts the
/// page, and how many listed entries of that registry page were already returned.
fn decode_cursor(cursor: &str, fingerprint: &str) -> Result<(Option<String>, usize), ApiError> {
    let mut parts = cursor.splitn(3, '.');
    let (Some(hash), Some(skip), Some(registry)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(bad_cursor());
    };
    if hash != fingerprint {
        return Err(bad_cursor());
    }
    let skip: usize = skip.parse().map_err(|_| bad_cursor())?;
    if skip > 100 {
        return Err(bad_cursor());
    }
    let registry = (!registry.is_empty()).then(|| registry.to_owned());
    Ok((registry, skip))
}

fn encode_cursor(fingerprint: &str, registry: Option<&str>, skip: usize) -> String {
    format!("{fingerprint}.{skip}.{}", registry.unwrap_or(""))
}

/// Public search. Asks the registry for a page (the newest eligible release per app), keeps the
/// entries the state store lists, and refills from the next registry page until the page is full
/// or [`MAX_SEARCH_PAGES`] were read; then it returns what it has with a cursor. An app whose
/// newest release is not listed is not offered, even when an older release is.
pub(crate) fn search_sync(core: &Core, params: &SearchParams) -> Result<SearchOutcome, ApiError> {
    let fingerprint = query_fingerprint(params);
    let (mut registry_cursor, mut skip) = match &params.cursor {
        Some(cursor) => decode_cursor(cursor, &fingerprint)?,
        None => (None, 0),
    };
    let now = core.clock.now();
    let mut releases: Vec<Release> = Vec::new();
    let mut next: Option<(Option<String>, usize)> = None;
    let mut pages = 0_usize;
    loop {
        let query = SearchQuery {
            text: params.text.clone(),
            category: params.category.clone(),
            capability: params.capability.clone(),
            limit: params.limit,
            cursor: registry_cursor.clone(),
            ..SearchQuery::default()
        };
        let page = core.registry.search(&query, now)?;
        pages += 1;
        let candidates: Vec<(AppName, Version)> = page
            .releases
            .iter()
            .map(|release| (release.app.clone(), release.version.clone()))
            .collect();
        let listed = core.state.listed_among(&candidates)?;
        let kept: Vec<Release> = page
            .releases
            .into_iter()
            .filter(|release| listed.contains(&(release.app.clone(), release.version.clone())))
            .collect();
        let available = kept.len().saturating_sub(skip);
        let room = params.limit - releases.len();
        let take = available.min(room);
        releases.extend(kept.into_iter().skip(skip).take(take));
        if available > take {
            // The page ends inside this registry page: resume after the entries taken.
            next = Some((registry_cursor.clone(), skip + take));
            break;
        }
        skip = 0;
        match page.next_cursor {
            Some(cursor) if releases.len() >= params.limit || pages >= MAX_SEARCH_PAGES => {
                next = Some((Some(cursor), 0));
                break;
            }
            Some(cursor) => registry_cursor = Some(cursor),
            None => break,
        }
    }
    let next_cursor =
        next.map(|(registry, skip)| encode_cursor(&fingerprint, registry.as_deref(), skip));
    Ok(SearchOutcome {
        releases,
        next_cursor,
    })
}

/// How a caller may see a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// The owning publisher: sees every release of their app.
    Owner,
    /// Anyone: only a listed, not withdrawn release.
    Public,
}

/// Whether the state store holds an explicit `listed` row for the version.
pub(crate) fn is_listed(core: &Core, app: &AppName, version: &Version) -> Result<bool, ApiError> {
    Ok(core
        .state
        .listing(app, version)?
        .is_some_and(|record| record.state == ListingState::Listed))
}

/// The caller's access to `release`, or `None` when it must answer as unknown.
pub(crate) fn access(
    core: &Core,
    release: &Release,
    owner: Option<&str>,
) -> Result<Option<Access>, ApiError> {
    if let Some(publisher) = owner
        && core
            .index
            .ownership(&release.app)?
            .is_some_and(|ownership| ownership.publisher == publisher)
    {
        return Ok(Some(Access::Owner));
    }
    if release.withdrawn.is_none() && is_listed(core, &release.app, &release.version)? {
        return Ok(Some(Access::Public));
    }
    Ok(None)
}

/// `app_get`: the newest listed, active, non-pre-release version; the owner also sees their own
/// newest version while it is under review.
pub(crate) fn app_sync(
    core: &Core,
    name: &AppName,
    owner: Option<&str>,
) -> Result<Release, ApiError> {
    let rows = core.index.releases(name, &Visibility::default(), None)?;
    let candidates: Vec<(AppName, Version)> = rows
        .iter()
        .take(MAX_CANDIDATES)
        .map(|release| (release.app.clone(), release.version.clone()))
        .collect();
    let listed = core.state.listed_among(&candidates)?;
    if let Some(found) = rows
        .into_iter()
        .find(|release| listed.contains(&(release.app.clone(), release.version.clone())))
    {
        return Ok(found);
    }
    if let Some(publisher) = owner
        && core
            .index
            .ownership(name)?
            .is_some_and(|ownership| ownership.publisher == publisher)
        && let Some(latest) = core.index.latest(name, true)?
    {
        return Ok(latest);
    }
    Err(ApiError::wire(Wire::AppNotFound))
}

/// `app_version_get`: one version, when the caller may see it.
pub(crate) fn version_sync(
    core: &Core,
    name: &AppName,
    version: &Version,
    owner: Option<&str>,
) -> Result<Release, ApiError> {
    let release = core
        .index
        .release(name, version)?
        .ok_or_else(|| ApiError::wire(Wire::VersionNotFound))?;
    match access(core, &release, owner)? {
        Some(_) => Ok(release),
        None => Err(ApiError::wire(Wire::VersionNotFound)),
    }
}

/// `app_bundle_download`, the part before any byte is read: the release, when the caller may see
/// it and, for a paid app, is entitled. A `spk_` bearer is a publisher key (the owner may fetch
/// their own unlisted bundle); any other bearer is a site licence, which a free app ignores.
pub(crate) fn resolve_download(
    core: &Core,
    name: &AppName,
    version: &Version,
    token: Option<&str>,
    now: i64,
) -> Result<Release, ApiError> {
    let owner = match token {
        Some(text) if is_publisher_token(text) => Some(authenticate_sync(core, text)?.publisher_id),
        _ => None,
    };
    let release = core
        .index
        .release(name, version)?
        .ok_or_else(|| ApiError::wire(Wire::VersionNotFound))?;
    let Some(granted) = access(core, &release, owner.as_deref())? else {
        return Err(ApiError::wire(Wire::VersionNotFound));
    };
    if granted == Access::Public
        && core.paid.contains(release.app.as_str())
        && !entitled(core, token, release.app.as_str(), now)
    {
        return Err(ApiError::wire(Wire::NotEntitled));
    }
    Ok(release)
}

/// The stored archive of `release`, read through the blob store (which re-validates it).
pub(crate) fn read_bundle(core: &Core, release: &Release) -> Result<Vec<u8>, ApiError> {
    let bytes = block_on(async {
        let stream = core.blobs.get(release.digest).await?;
        collect_stream(stream, core.policy.limits.max_compressed_bytes).await
    })?;
    if bytes.len() as u64 != release.size {
        return Err(ApiError::internal());
    }
    Ok(bytes)
}

fn refused(code: &str) -> String {
    clip(&format!("refused: {code}"), 512)
}

/// `app_version_yank`: marks the version yanked for its owner, audited whether it succeeds or
/// not. An unknown app or version and another publisher's version are the same `404`.
pub(crate) fn yank_sync(
    core: &Core,
    who: &Principal,
    name: &AppName,
    version: &Version,
    reason: &str,
    now: i64,
) -> Result<Release, ApiError> {
    let outcome = yank_inner(core, who, name, version, reason, now);
    let (digest, note) = match &outcome {
        Ok(release) => (Some(release.digest), clip(reason, 512)),
        Err(error) => (None, refused(&error.code)),
    };
    core.audit(&AuditEntry {
        kind: AuditKind::Yank,
        actor: who.publisher_id.clone(),
        publisher_id: Some(who.publisher_id.clone()),
        app: Some(name.clone()),
        version: Some(version.clone()),
        digest,
        reason: note,
        at: now,
    });
    outcome
}

fn yank_inner(
    core: &Core,
    who: &Principal,
    name: &AppName,
    version: &Version,
    reason: &str,
    now: i64,
) -> Result<Release, ApiError> {
    if core
        .index
        .publisher(&who.publisher_id)?
        .is_some_and(|publisher| publisher.status != PublisherStatus::Active)
    {
        return Err(ApiError::wire(Wire::PublisherRevoked));
    }
    let not_found = || ApiError::wire(Wire::VersionNotFound);
    match core.index.ownership(name)? {
        Some(ownership) if ownership.publisher == who.publisher_id => {}
        _ => return Err(not_found()),
    }
    let release = core.index.release(name, version)?.ok_or_else(not_found)?;
    if release.yanked.is_some() {
        return Err(ApiError::wire(Wire::AlreadyYanked));
    }
    let removal = Removal {
        at: now,
        reason: reason.to_owned(),
    };
    match core
        .index
        .remove(name, version, &who.publisher_id, removal, false)
    {
        Ok(Some(updated)) => Ok(updated),
        Ok(None) => Err(ApiError::wire(Wire::AlreadyYanked)),
        Err(error) if error.code == "registry.not_owner" => Err(not_found()),
        Err(error) if error.code == "registry.publisher_inactive" => {
            Err(ApiError::wire(Wire::PublisherRevoked))
        }
        Err(error) => Err(error.into()),
    }
}

/// `review_get`: the publisher's own review as counts and a verdict, never the findings. Anyone
/// else's review, and an unknown id, is the same `404`. A review that has no verdict yet (queued,
/// running or failed) reads as `needs_human_review` with no counts: the contract has no status
/// field, and the conservative reading is the true one (a person must look).
pub(crate) fn review_sync(core: &Core, who: &Principal, id: &str) -> Result<Value, ApiError> {
    let not_found = || ApiError::wire(Wire::ReviewNotFound);
    let record = core.state.get_review(id)?.ok_or_else(not_found)?;
    if record.publisher_id != who.publisher_id {
        return Err(not_found());
    }
    let mut verdict = "needs_human_review".to_owned();
    let (mut info, mut warning, mut blocker) = (0_u64, 0_u64, 0_u64);
    if record.status == ReviewStatus::Done
        && let Some(summary) = &record.summary
        && let Ok(value) = serde_json::from_str::<Value>(summary.as_str())
    {
        let word = value["verdict"].as_str().unwrap_or_default();
        if matches!(word, "rejected" | "needs_human_review" | "looks_clean") {
            word.clone_into(&mut verdict);
            info = value["findings"]["info"].as_u64().unwrap_or(0);
            warning = value["findings"]["warning"].as_u64().unwrap_or(0);
            blocker = value["findings"]["blocker"].as_u64().unwrap_or(0);
        }
    }
    Ok(json!({
        "id": record.review_id,
        "app": record.app.as_str(),
        "version": record.version.to_string(),
        "verdict": verdict,
        "findings": { "info": info, "warning": warning, "blocker": blocker },
        "created_at": rfc3339(record.created_at)?,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{decode_cursor, encode_cursor};

    #[test]
    fn a_cursor_round_trips_and_is_bound_to_its_query() {
        let text = encode_cursor("0123456789abcdef", Some("ab:1"), 3);
        let (registry, skip) = decode_cursor(&text, "0123456789abcdef").unwrap();
        assert_eq!(registry.as_deref(), Some("ab:1"));
        assert_eq!(skip, 3);
        assert!(decode_cursor(&text, "ffffffffffffffff").is_err());
        let (none, zero) = decode_cursor(&encode_cursor("aa", None, 0), "aa").unwrap();
        assert!(none.is_none());
        assert_eq!(zero, 0);
    }

    #[test]
    fn a_malformed_cursor_is_refused() {
        assert!(decode_cursor("nonsense", "aa").is_err());
        assert!(decode_cursor("aa.x.", "aa").is_err());
        assert!(decode_cursor("aa.101.", "aa").is_err());
    }
}
