//! The engine-backed [`ReadAccess`] of the AI audit timeline (WP-AI-11.T9): what a viewer may see
//! of one document, decided by the permission engine and the field guard as the **viewer**, never
//! as the platform.
//!
//! # The decision
//!
//! [`EngineReadAccess::access`] answers, for `viewer`, `doctype` and `id`:
//!
//! - [`Access::None`] when the viewer may not read the document, the document does not exist, or
//!   the id is not one of this site's records (another tenant's id is simply not here). The three
//!   cases are the same answer, so the timeline of an unreadable document says nothing about
//!   whether it exists.
//! - [`Access::Masked`] when the viewer may read the document but the field guard does not show
//!   at least one of its protected fields as the real value (it is masked or hidden).
//! - [`Access::Full`] when the viewer may read the document and every protected field.
//!
//! The document is loaded through the live document engine with the viewer's own invocation: the
//! engine applies the role grants, the user permissions, the shares and the conditions of the
//! decision core served now, so a permission revoked a moment ago applies to the next timeline.
//! The field decision asks [`FieldGuard::columns`] on the version-history channel (the channel
//! the document timeline is) as the viewer's principal, resolved from the stored role
//! assignments. Only the **decision** is kept: the loaded document is dropped without being read
//! and no field value is returned or logged.
//!
//! # Who a viewer is
//!
//! `viewer` is `<user id>` or `user:<user id>` (a signed-in person), or `agent:<id>`. An agent is
//! judged by its **own** roles: the id after `agent:` is the agent's own user record (an agent
//! is a user whose registration marks it as one), so it never borrows the rights of the person it
//! acts for. A viewer that is neither is refused (`Err`), never allowed.
//!
//! # Failure
//!
//! An engine that is not running, a principal source that is gone, a document load that fails for
//! any reason but "no such document" or "not allowed", a DocType the guard has no facts for and a
//! guard that fails are all `Err` with a fixed sentence (no value, no id): the timeline then
//! refuses ([`super::TIMELINE_REFUSED`]) instead of showing entries. Nothing here defaults to
//! allow.

use std::sync::Arc;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::error::ErrorClass;
use spot_app_api::prelude::RecordId;
use spot_core::{Clock, SiteId};
use spot_document::engine::PrincipalSource;
use spot_field_guard::{Channel, ProtectionSource};

use super::{Access, DenyAll, Fut, ReadAccess};
use crate::live::SiteLive;

/// Why a decision could not be made. Fixed sentences: nothing here names a user, a document or a
/// value.
pub mod why {
    /// The live document engine is not running yet (or any more).
    pub const ENGINE_DOWN: &str = "the document engine is not running";
    /// No metadata is served, so the field facts cannot be read.
    pub const NO_METADATA: &str = "the site serves no metadata yet";
    /// The stored role assignments cannot be read.
    pub const NO_PRINCIPALS: &str = "the role assignments cannot be read";
    /// The engine failed for a reason other than "no such document" or "not allowed".
    pub const ENGINE_FAILED: &str = "the document engine could not answer";
    /// The field guard has no facts for the DocType or failed.
    pub const GUARD_FAILED: &str = "the field guard could not answer";
}

/// Who is asking, parsed from the viewer text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    /// A signed-in person.
    User(RecordId),
    /// An agent, by its own user record.
    Agent(RecordId),
}

impl Viewer {
    /// The viewer named by `text`: `<id>`, `user:<id>` or `agent:<id>`, the id in the canonical
    /// record-id form. `None` for anything else (a consumer, the platform, a name).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(id) = text.strip_prefix("agent:") {
            return spot_http::api::codec::parse_record_id(id).map(Self::Agent);
        }
        let id = text.strip_prefix("user:").unwrap_or(text);
        spot_http::api::codec::parse_record_id(id).map(Self::User)
    }

    /// The record id whose roles decide.
    #[must_use]
    pub fn id(self) -> RecordId {
        match self {
            Self::User(id) | Self::Agent(id) => id,
        }
    }
}

/// What the engine side decides for one viewer and one document. The production implementation
/// is [`LiveJudge`]; a test supplies a scripted one.
pub trait Judge: Send + Sync {
    /// The viewer's access to `doctype`/`id`, or why it cannot be decided.
    fn judge<'a>(
        &'a self,
        viewer: Viewer,
        doctype: &'a str,
        id: RecordId,
    ) -> Fut<'a, Result<Access, &'static str>>;
}

/// [`ReadAccess`] over a [`Judge`]: parses the viewer and the document id; a malformed document id
/// is "no such document" and a viewer that is not a user or an agent id may read nothing, both
/// [`Access::None`] without asking the engine.
pub struct EngineReadAccess<J> {
    judge: J,
}

impl<J: Judge> EngineReadAccess<J> {
    /// An access check over `judge`.
    #[must_use]
    pub fn new(judge: J) -> Self {
        Self { judge }
    }
}

impl<J: Judge> ReadAccess for EngineReadAccess<J> {
    fn access<'a>(
        &'a self,
        viewer: &'a str,
        doctype: &'a str,
        id: &'a str,
    ) -> Fut<'a, Result<Access, String>> {
        Box::pin(async move {
            // A viewer that is not a user or an agent record id (a name, the platform, a consumer)
            // holds no role of this site: it may read nothing.
            let Some(viewer) = Viewer::parse(viewer) else {
                return Ok(Access::None);
            };
            // An id that is not a record id of this site names no document of this site: the
            // same answer as a missing one.
            let Some(id) = spot_http::api::codec::parse_record_id(id) else {
                return Ok(Access::None);
            };
            self.judge
                .judge(viewer, doctype, id)
                .await
                .map_err(str::to_owned)
        })
    }
}

/// The production judge: the live engine, the live decision core and the stored assignments of
/// the site, read on every call so a swap or a revocation shows at once.
pub struct LiveJudge {
    live: Arc<SiteLive>,
    site: SiteId,
    clock: Arc<dyn Clock>,
}

impl LiveJudge {
    /// A judge over `live` for `site`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, site: SiteId, clock: Arc<dyn Clock>) -> Self {
        Self { live, site, clock }
    }

    async fn decide(
        &self,
        viewer: Viewer,
        doctype: &str,
        id: RecordId,
    ) -> Result<Access, &'static str> {
        let engine = self.live.engine().ok_or(why::ENGINE_DOWN)?;
        let served = self.live.served().ok_or(why::NO_METADATA)?;
        let principals = self.live.principals().ok_or(why::NO_PRINCIPALS)?;

        // 1. May the viewer read the document at all? The engine decides, as the viewer.
        let invocation = Invocation {
            site: self.site.clone(),
            actor: Actor::User {
                id: viewer.id(),
                via: None,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        };
        match engine.load(&invocation, doctype, id).await {
            // The document is dropped unread: only the fact that it loaded matters.
            Ok(_) => {}
            // Not there, not allowed and not a DocType are one answer.
            Err(error)
                if matches!(
                    error.class,
                    ErrorClass::NotFound
                        | ErrorClass::PermissionDenied
                        | ErrorClass::UnknownDocType
                ) =>
            {
                return Ok(Access::None);
            }
            Err(_) => return Err(why::ENGINE_FAILED),
        }

        // 2. Does the guard show every protected field as its real value to this viewer?
        let protection = spot_field_guard::MetaProtection::new(&served.meta)
            .protection(doctype)
            .ok_or(why::GUARD_FAILED)?;
        let names: Vec<String> = protection
            .protected_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            return Ok(Access::Full);
        }
        let principal = principals.resolve(&viewer.id()).await;
        let guard = crate::live::field_guard(&served, &self.clock);
        match guard.columns(Channel::Version, &principal, doctype, &names) {
            Ok(columns) if columns.masked.is_empty() && columns.refused.is_empty() => {
                Ok(Access::Full)
            }
            Ok(_) => Ok(Access::Masked),
            // The version channel refuses a hidden field instead of dropping it: the viewer
            // reads the document without it.
            Err(error) if error.code() == spot_field_guard::error::FIELD_DENIED => {
                Ok(Access::Masked)
            }
            Err(_) => Err(why::GUARD_FAILED),
        }
    }
}

impl Judge for LiveJudge {
    fn judge<'a>(
        &'a self,
        viewer: Viewer,
        doctype: &'a str,
        id: RecordId,
    ) -> Fut<'a, Result<Access, &'static str>> {
        Box::pin(self.decide(viewer, doctype, id))
    }
}

/// The access the timeline uses when no other provider was installed: engine-backed over the
/// site's live hub. A site name that is not a valid site id keeps [`DenyAll`].
#[must_use]
pub fn engine_backed(
    live: &Arc<SiteLive>,
    site: &str,
    clock: &Arc<dyn Clock>,
) -> Arc<dyn ReadAccess> {
    match SiteId::new(site) {
        Ok(site) => Arc::new(EngineReadAccess::new(LiveJudge::new(
            Arc::clone(live),
            site,
            Arc::clone(clock),
        ))),
        Err(_) => Arc::new(DenyAll),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::Mutex;

    /// A judge that answers a script and remembers what it was asked.
    struct Scripted {
        answer: Result<Access, &'static str>,
        asked: Mutex<Vec<(Viewer, String, RecordId)>>,
    }

    impl Scripted {
        fn new(answer: Result<Access, &'static str>) -> Self {
            Self {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Judge for Scripted {
        fn judge<'a>(
            &'a self,
            viewer: Viewer,
            doctype: &'a str,
            id: RecordId,
        ) -> Fut<'a, Result<Access, &'static str>> {
            self.asked
                .lock()
                .unwrap()
                .push((viewer, doctype.to_owned(), id));
            let answer = self.answer;
            Box::pin(async move { answer })
        }
    }

    #[test]
    fn a_viewer_is_a_user_or_an_agent_by_record_id() {
        let id = RecordId::new_v7();
        let text = id.to_string();
        assert_eq!(Viewer::parse(&text), Some(Viewer::User(id)));
        assert_eq!(
            Viewer::parse(&format!("user:{text}")),
            Some(Viewer::User(id))
        );
        assert_eq!(
            Viewer::parse(&format!("agent:{text}")),
            Some(Viewer::Agent(id))
        );
        for bad in [
            "",
            "agent:",
            "user:",
            "system",
            "consumer:0190-0000",
            "agent:not-an-id",
            "alice@example.test",
            "agent:agent:x",
        ] {
            assert_eq!(Viewer::parse(bad), None, "{bad}");
        }
        assert_eq!(Viewer::Agent(id).id(), id);
    }

    #[tokio::test]
    async fn the_decision_is_the_judges_for_a_good_viewer_and_id() {
        let viewer = RecordId::new_v7().to_string();
        let document = RecordId::new_v7();
        for answer in [Access::None, Access::Masked, Access::Full] {
            let access = EngineReadAccess::new(Scripted::new(Ok(answer)));
            let got = access
                .access(&viewer, "task", &document.to_string())
                .await
                .unwrap();
            assert_eq!(got, answer);
            let asked = access.judge.asked.lock().unwrap();
            assert_eq!(asked.len(), 1);
            assert_eq!(asked[0].1, "task");
            assert_eq!(asked[0].2, document);
        }
    }

    #[tokio::test]
    async fn a_malformed_document_id_is_no_document_and_never_reaches_the_judge() {
        let viewer = RecordId::new_v7().to_string();
        let access = EngineReadAccess::new(Scripted::new(Ok(Access::Full)));
        let long = "x".repeat(500);
        for id in ["", "not-an-id", "../etc", long.as_str()] {
            assert_eq!(
                access.access(&viewer, "task", id).await,
                Ok(Access::None),
                "{id}"
            );
        }
        assert!(access.judge.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_viewer_that_is_no_user_reads_nothing_and_never_reaches_the_judge() {
        let document = RecordId::new_v7().to_string();
        let access = EngineReadAccess::new(Scripted::new(Ok(Access::Full)));
        for viewer in ["", "system", "alice", "consumer:1", "l3-user"] {
            assert_eq!(
                access.access(viewer, "task", &document).await,
                Ok(Access::None),
                "{viewer}"
            );
        }
        assert!(access.judge.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_judge_failure_is_a_refusal_with_its_fixed_sentence() {
        let viewer = RecordId::new_v7().to_string();
        let document = RecordId::new_v7().to_string();
        for failure in [why::ENGINE_DOWN, why::ENGINE_FAILED, why::GUARD_FAILED] {
            let access = EngineReadAccess::new(Scripted::new(Err(failure)));
            assert_eq!(
                access.access(&viewer, "task", &document).await,
                Err(failure.to_owned())
            );
        }
    }

    #[test]
    fn no_sentence_names_a_value() {
        for sentence in [
            why::ENGINE_DOWN,
            why::NO_METADATA,
            why::NO_PRINCIPALS,
            why::ENGINE_FAILED,
            why::GUARD_FAILED,
        ] {
            assert!(!sentence.contains(':') && !sentence.chars().any(|c| c.is_ascii_digit()));
        }
    }
}
