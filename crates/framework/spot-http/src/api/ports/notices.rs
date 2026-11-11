//! The notice feed seam of `GET /api/v1/notifications` (`notification_list`, contract 0.10.0,
//! WP-CON-03.T14): the durable side of the realtime `user_notice` channel.
//!
//! `spot-http` does not depend on the notification storage. It defines [`NoticeSource`], the one
//! read the route needs, and the host writes the adapter over ADR-085's stored rows (a separate
//! task: the storage crate is not wired here). The port resolves the caller from the request
//! context and returns **that caller's** notices only; the route never passes a user, so there
//! is no way to ask for another user's feed. [`MemoryNoticeSource`] is the in-memory fake the
//! tests use.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Map, Value as Json};

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`NoticeSource`].
pub const KEY: PortKey = PortKey::of::<dyn NoticeSource>();

/// One stored notice, as the contract's `NotificationItem` (the same `event_id`, `event` and
/// `data` the realtime channel delivered, plus `created_at`).
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    /// The notice's id, 1 to 128 characters; opaque to the client, which sends it back as `after`.
    pub event_id: String,
    /// A short lowercase token, `user_notice` today.
    pub event: String,
    /// The notice's data, at most 2 KiB serialised.
    pub data: Map<String, Json>,
    /// When it was stored: a UTC RFC 3339 instant ending in `Z`.
    pub created_at: String,
}

/// Why the feed could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeError {
    /// `after` names no notice in the caller's own feed. A forged id, another user's id and a
    /// pruned one are this one answer, so the feed is never an existence oracle.
    UnknownAfter,
    /// The credential is missing, unknown or ended.
    Unauthenticated,
    /// The store cannot answer now; the same request may succeed later.
    Unavailable,
}

/// The caller's own notices.
pub trait NoticeSource: Send + Sync + 'static {
    /// The caller's notices after `after` (all of them from the oldest kept when `None`),
    /// oldest first, at most `limit`. The caller is the one `ctx.credential` names on
    /// `ctx.site`; a notice of another user or another site is never returned or consulted.
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        after: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Notice>, NoticeError>>;
}

#[derive(Default)]
struct Inner {
    /// (site, bearer token) to user.
    tokens: HashMap<(String, String), String>,
    /// (site, user) to the feed, oldest first.
    feeds: HashMap<(String, String), Vec<Notice>>,
}

/// An in-memory [`NoticeSource`] for tests: tokens map to users and every (site, user) has its
/// own feed.
#[derive(Default)]
pub struct MemoryNoticeSource {
    inner: Mutex<Inner>,
}

impl MemoryNoticeSource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Lets `token` act as `user` on `site`.
    pub fn grant(&self, site: &str, token: &str, user: &str) {
        self.lock()
            .tokens
            .insert((site.to_owned(), token.to_owned()), user.to_owned());
    }

    /// Appends a notice to `user`'s feed on `site`.
    pub fn push(&self, site: &str, user: &str, notice: Notice) {
        self.lock()
            .feeds
            .entry((site.to_owned(), user.to_owned()))
            .or_default()
            .push(notice);
    }
}

impl NoticeSource for MemoryNoticeSource {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        after: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Notice>, NoticeError>> {
        Box::pin(async move {
            let site = ctx.site.as_str().to_owned();
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or(NoticeError::Unauthenticated)?;
            let inner = self.lock();
            let user = inner
                .tokens
                .get(&(site.clone(), token))
                .ok_or(NoticeError::Unauthenticated)?;
            let empty = Vec::new();
            let feed = inner
                .feeds
                .get(&(site, user.clone()))
                .unwrap_or(&empty);
            let start = match after {
                None => 0,
                Some(id) => match feed.iter().position(|n| n.event_id == id) {
                    Some(index) => index + 1,
                    None => return Err(NoticeError::UnknownAfter),
                },
            };
            Ok(feed.iter().skip(start).take(limit).cloned().collect())
        })
    }
}
