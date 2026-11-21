//! The run directory (WP-BPM-02.T10): provides the [`RunDirectory`] port of the process API over
//! [`PgRunDirectory`], so `run_list` answers instead of `503`, and turns the port's opaque cursor
//! into the directory's keyset position.
//!
//! The directory applies the viewer's visibility **inside the query** (see
//! `spot_bpm_pg::run_directory`); this module only narrows the *ids*: `run_get` and the port
//! still check each Run.
//!
//! **The cursor** is `v1.<hex payload>.<tag>`: the payload is the position as text
//! (`<started_at in microseconds>:<case id>`), the tag the first 32 hex characters of the SHA-256
//! of `site \n viewer \n process \n status \n mine \n sees_all \n position`. A cursor that does
//! not belong to **this** site, **this** viewer and **this** filter, a malformed one, one over
//! 256 bytes or one of another version is [`ProcessPortError::BadCursor`], and nothing says why.
//! The tag is integrity against client mistakes and cross-viewer replay, not a secret: the
//! position it carries is not confidential.
//!
//! Settings `[services.process_run_directory]` (`enabled`, default `true`; `false` leaves
//! `run_list` at 503 on purpose). Start order 1140: before `process_api` (1150), which looks the
//! port up while the site assembles. Operator page:
//! `docs/running-spot/process_run_directory.md`.

use std::sync::Arc;

use serde::Deserialize;
use spot_bpm_model::ContentHash;
use spot_bpm_pg::{CaseStatusFilter, ListingCursor, ListingQuery, PgRunDirectory};
use spot_dag_exec::RunStatus;
use spot_http::api::ports::process::{ListPage, ProcessPortError, ProcessResult};
use spot_http::api::service::BoxFuture;
use time::OffsetDateTime;

use crate::services::SiteCtx;
use crate::services::process::ProcessHost;
use crate::services::process_api::{DirectoryQuery, RunDirectory};
use crate::{Error, Result};

/// Start order: before the process API (1150), after the process engine (1100).
pub const ORDER: u32 = 1140;

/// The longest cursor accepted, in bytes.
const CURSOR_MAX: usize = 256;
/// The cursor format version.
const CURSOR_VERSION: &str = "v1";
/// Hex characters of the tag kept.
const TAG_HEX: usize = 32;

/// `[services.process_run_directory]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module provides the directory at all.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The [`RunDirectory`] of one site over [`PgRunDirectory`].
pub struct Directory {
    site: String,
    inner: PgRunDirectory,
}

/// The directory of `site` over `inner` (the tenant of `inner` should be `site`).
#[must_use]
pub fn directory(site: &str, inner: PgRunDirectory) -> Arc<dyn RunDirectory> {
    Arc::new(Directory {
        site: site.to_owned(),
        inner,
    })
}

fn status_token(status: Option<RunStatus>) -> &'static str {
    match status {
        None => "",
        Some(RunStatus::Running) => "running",
        Some(RunStatus::Completed) => "completed",
        Some(RunStatus::Failed) => "failed",
        Some(RunStatus::Cancelled) => "cancelled",
    }
}

fn filter_of(status: RunStatus) -> CaseStatusFilter {
    match status {
        RunStatus::Running => CaseStatusFilter::Running,
        RunStatus::Completed => CaseStatusFilter::Completed,
        RunStatus::Failed => CaseStatusFilter::Failed,
        RunStatus::Cancelled => CaseStatusFilter::Cancelled,
    }
}

/// What a cursor is bound to: the site, the viewer and the filter.
struct Binding<'a> {
    site: &'a str,
    viewer: &'a str,
    process: &'a str,
    status: &'static str,
    mine: bool,
    sees_all: bool,
}

impl<'a> Binding<'a> {
    fn of(site: &'a str, query: &'a DirectoryQuery) -> Self {
        Self {
            site,
            viewer: &query.viewer,
            process: query.process.as_deref().unwrap_or(""),
            status: status_token(query.status),
            mine: query.mine,
            sees_all: query.sees_all,
        }
    }

    /// The first 32 hex characters of the SHA-256 over the binding and the position text.
    fn tag(&self, position: &str) -> String {
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.site, self.viewer, self.process, self.status, self.mine, self.sees_all, position
        );
        let hash = ContentHash::of_canonical(text.as_bytes());
        hash.as_hex().chars().take(TAG_HEX).collect()
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

/// The cursor for `position`, bound to `binding`.
fn encode(binding: &Binding<'_>, position: &ListingCursor) -> String {
    let micros = position.started_at.unix_timestamp_nanos() / 1_000;
    let text = format!("{micros}:{}", position.case_id);
    format!(
        "{CURSOR_VERSION}.{}.{}",
        to_hex(text.as_bytes()),
        binding.tag(&text)
    )
}

/// The position of `cursor`, when it is well formed and bound to `binding`.
fn decode(binding: &Binding<'_>, cursor: &str) -> Option<ListingCursor> {
    if cursor.len() > CURSOR_MAX {
        return None;
    }
    let mut parts = cursor.split('.');
    let (version, payload, tag) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || version != CURSOR_VERSION {
        return None;
    }
    let text = String::from_utf8(from_hex(payload)?).ok()?;
    if tag != binding.tag(&text) {
        return None;
    }
    let (micros, case_id) = text.split_once(':')?;
    if case_id.is_empty() {
        return None;
    }
    let micros: i64 = micros.parse().ok()?;
    let started_at =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros).checked_mul(1_000)?).ok()?;
    Some(ListingCursor {
        started_at,
        case_id: case_id.to_owned(),
    })
}

impl RunDirectory for Directory {
    fn page<'a>(
        &'a self,
        query: &'a DirectoryQuery,
    ) -> BoxFuture<'a, ProcessResult<ListPage<String>>> {
        Box::pin(async move {
            let binding = Binding::of(&self.site, query);
            let after = match &query.cursor {
                Some(raw) => Some(decode(&binding, raw).ok_or(ProcessPortError::BadCursor)?),
                None => None,
            };
            let asked = ListingQuery {
                viewer: query.viewer.clone(),
                also_cases: query.also_cases.clone(),
                sees_all: query.sees_all,
                process: query.process.clone(),
                status: query.status.map(filter_of),
                mine: query.mine,
                after,
                limit: query.limit,
            };
            // A store error is `Unavailable`, never the SQL or driver text.
            let page = self
                .inner
                .page(&asked)
                .await
                .map_err(|_| ProcessPortError::Unavailable)?;
            Ok(ListPage {
                items: page.case_ids,
                next_cursor: page.next.as_ref().map(|next| encode(&binding, next)),
            })
        })
    }
}

/// Provides the [`RunDirectory`] port. Inert when the module is off or the process engine is not
/// hosted.
///
/// # Errors
/// A setting that does not decode; the listing indexes cannot be created (the message names the
/// site); a port provided twice.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_run_directory")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    if ctx.port::<ProcessHost>().is_none() {
        tracing::debug!("process_run_directory: the process engine is not hosted; no directory");
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let inner = PgRunDirectory::new(env.backend.pool()).with_tenant(site.clone());
    inner.prepare().await.map_err(|_| {
        Error(format!(
            "process_run_directory: the run listing indexes could not be created for site `{site}`"
        ))
    })?;
    ctx.provide::<dyn RunDirectory>(directory(&site, inner))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn query(viewer: &str) -> DirectoryQuery {
        DirectoryQuery {
            viewer: viewer.to_owned(),
            also_cases: Vec::new(),
            sees_all: false,
            process: Some("approvals".to_owned()),
            status: Some(RunStatus::Running),
            mine: false,
            cursor: None,
            limit: 10,
        }
    }

    fn position() -> ListingCursor {
        ListingCursor {
            started_at: OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000)
                .unwrap(),
            case_id: "case_abcdefghijklmnop".to_owned(),
        }
    }

    fn cursor_for(site: &str, q: &DirectoryQuery) -> String {
        encode(&Binding::of(site, q), &position())
    }

    #[test]
    fn a_cursor_round_trips_with_microsecond_precision() {
        let q = query("u1");
        let cursor = cursor_for("site-a", &q);
        assert!(cursor.starts_with("v1."));
        assert!(cursor.len() <= CURSOR_MAX);
        let back = decode(&Binding::of("site-a", &q), &cursor).unwrap();
        assert_eq!(back, position());
    }

    #[test]
    fn each_bound_field_changed_alone_refuses_the_cursor() {
        let q = query("u1");
        let cursor = cursor_for("site-a", &q);
        let refused = |site: &str, q: &DirectoryQuery| decode(&Binding::of(site, q), &cursor);
        assert!(refused("site-b", &q).is_none(), "another site");
        assert!(refused("site-a", &query("u2")).is_none(), "another viewer");
        let mut other = query("u1");
        other.process = Some("other".to_owned());
        assert!(refused("site-a", &other).is_none(), "another process");
        let mut other = query("u1");
        other.process = None;
        assert!(refused("site-a", &other).is_none(), "no process");
        let mut other = query("u1");
        other.status = Some(RunStatus::Completed);
        assert!(refused("site-a", &other).is_none(), "another status");
        let mut other = query("u1");
        other.status = None;
        assert!(refused("site-a", &other).is_none(), "no status");
        let mut other = query("u1");
        other.mine = true;
        assert!(refused("site-a", &other).is_none(), "mine");
        let mut other = query("u1");
        other.sees_all = true;
        assert!(refused("site-a", &other).is_none(), "sees_all");
        assert!(
            refused("site-a", &q).is_some(),
            "the unchanged binding passes"
        );
    }

    #[test]
    fn a_tampered_truncated_or_junk_cursor_is_refused() {
        let q = query("u1");
        let b = Binding::of("site-a", &q);
        let cursor = cursor_for("site-a", &q);
        // A different payload with the old tag.
        let parts: Vec<&str> = cursor.split('.').collect();
        let forged_payload = to_hex(b"1:other_case_id_value");
        assert!(decode(&b, &format!("v1.{forged_payload}.{}", parts[2])).is_none());
        // A different tag.
        assert!(decode(&b, &format!("v1.{}.{}", parts[1], "0".repeat(TAG_HEX))).is_none());
        // Truncated.
        for cut in [0, 1, 2, 10, cursor.len() - 1] {
            assert!(decode(&b, &cursor[..cut]).is_none(), "cut at {cut}");
        }
        // Unknown version, extra part, not hex, upper-case hex, empty, oversized.
        assert!(decode(&b, &format!("v2.{}.{}", parts[1], parts[2])).is_none());
        assert!(decode(&b, &format!("{cursor}.extra")).is_none());
        assert!(decode(&b, &format!("v1.zz.{}", parts[2])).is_none());
        assert!(decode(&b, &format!("v1.{}.{}", parts[1].to_uppercase(), parts[2])).is_none());
        assert!(decode(&b, "").is_none());
        assert!(decode(&b, "junk").is_none());
        assert!(decode(&b, &"a".repeat(CURSOR_MAX + 1)).is_none());
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("xy").is_none());
    }

    #[test]
    fn settings_default_to_enabled_and_refuse_unknown_keys() {
        assert!(Settings::default().enabled);
        let off: Settings = toml::from_str("enabled = false").unwrap();
        assert!(!off.enabled);
        let bad: std::result::Result<Settings, _> = toml::from_str("enabeld = false");
        assert!(bad.is_err());
    }

    #[test]
    fn statuses_map_to_the_table_values() {
        for (run, filter) in [
            (RunStatus::Running, CaseStatusFilter::Running),
            (RunStatus::Completed, CaseStatusFilter::Completed),
            (RunStatus::Failed, CaseStatusFilter::Failed),
            (RunStatus::Cancelled, CaseStatusFilter::Cancelled),
        ] {
            assert_eq!(filter_of(run), filter);
            assert_eq!(status_token(Some(run)), filter.as_str());
        }
    }
}
