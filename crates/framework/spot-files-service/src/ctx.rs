//! The request context the files operations run under.
//!
//! The native API's service traits pass `spot_http::api::RequestCtx`; the files trait does not
//! exist in `spot-http` yet (WP-CON-03's contract bump), so this crate carries the two things
//! the file service needs — the site and the raw bearer credential — in [`FileCtx`]. When the
//! trait moves to `spot-http`, a one-line adapter turns a `RequestCtx` into a `FileCtx` (or the
//! signatures take `RequestCtx` directly).

use spot_core::SiteId;

/// The site and credential of one files request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCtx {
    /// The site the request resolved to (ADR-34).
    pub site: SiteId,
    /// The raw bearer credential, when the request carried one. Interpreting it (session, API
    /// key, expiry) is [`CallerSource`](crate::CallerSource)'s job.
    pub credential: Option<String>,
}

impl FileCtx {
    /// A request with no credential (a guest).
    pub fn guest(site: SiteId) -> Self {
        FileCtx {
            site,
            credential: None,
        }
    }

    /// The same request with a bearer credential.
    pub fn with_credential(mut self, credential: impl Into<String>) -> Self {
        self.credential = Some(credential.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::FileCtx;
    use spot_core::SiteId;

    #[test]
    fn builds_a_guest_or_credentialed_context() {
        let site = SiteId::new("files.test").unwrap_or_else(|e| panic!("site: {e:?}"));
        let guest = FileCtx::guest(site.clone());
        assert_eq!(guest.credential, None);
        let signed = guest.with_credential("token-1");
        assert_eq!(signed.credential.as_deref(), Some("token-1"));
        assert_eq!(signed.site, site);
    }
}
