//! The key scheme: one place that turns a site, a cache key, a lease name, a channel or a node
//! into the exact Redis key the plugin uses.
//!
//! Every key is built here so the scheme is testable without a server and cannot drift between
//! the cache, the coordinator, the bus and presence. Keys are colon-separated (`spot:...`), never
//! dotted, so the error-code scan cannot mistake a key for an error code.
//!
//! # Shape
//!
//! ```text
//! {prefix}:cache:{site}:data:{key}     one cached value (string, hash, list or set)
//! {prefix}:cache:{site}:sizes          hash: key -> accounted bytes (the size index)
//! {prefix}:cache:{site}:bytes          integer: accounted bytes of the site right now
//! {prefix}:cache:{site}:gen            integer: the site's namespace generation
//! {prefix}:coord:lease:{name}          hash: holder, token, id, expires_us
//! {prefix}:coord:token:{name}          integer: the token high-water mark for a name
//! {prefix}:coord:counter:{name}        decimal string: the last value a counter issued
//! {prefix}:coord:rate:{key}            hash: tat, capacity, unit (one GCRA bucket)
//! {prefix}:coord:node:{node}           presence key with a TTL (second-node detection)
//! {prefix}:bus:{site}                  the pub/sub channel of one site
//! {prefix}:meta:head                   the version store's `version` and `hash` record
//! ```
//!
//! The prefix defaults to [`DEFAULT_NAMESPACE`]. A test or a second logical cluster passes its
//! own prefix, so two suites can share one server without colliding.

use spot_core::SiteId;

/// The default key prefix of the plugin.
pub const DEFAULT_NAMESPACE: &str = "spot:cluster:redis:v1";

/// Builds every key the plugin uses from one prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySpace {
    prefix: String,
}

impl Default for KeySpace {
    fn default() -> Self {
        Self::with_prefix(DEFAULT_NAMESPACE)
    }
}

impl KeySpace {
    /// A key space under `prefix` (for example `spot:cluster:redis:v1`).
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// The prefix every key starts with.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    // ---- cache ---------------------------------------------------------------------------

    /// One cached value of `site` under `key`.
    #[must_use]
    pub fn cache_data(&self, site: &SiteId, key: &str) -> String {
        format!("{}:cache:{}:data:{}", self.prefix, site, key)
    }

    /// The `MATCH` pattern of one cached prefix, for cursor-based deletion.
    #[must_use]
    pub fn cache_pattern(&self, site: &SiteId, prefix: &str) -> String {
        format!(
            "{}:cache:{}:data:{}*",
            self.prefix,
            site,
            escape_glob(prefix)
        )
    }

    /// The `MATCH` pattern of every cached value of `site`.
    #[must_use]
    pub fn cache_all_pattern(&self, site: &SiteId) -> String {
        format!("{}:cache:{}:data:*", self.prefix, site)
    }

    /// The hash that maps each live key of `site` to its accounted bytes.
    #[must_use]
    pub fn cache_sizes(&self, site: &SiteId) -> String {
        format!("{}:cache:{}:sizes", self.prefix, site)
    }

    /// The integer holding the site's accounted bytes right now.
    #[must_use]
    pub fn cache_bytes(&self, site: &SiteId) -> String {
        format!("{}:cache:{}:bytes", self.prefix, site)
    }

    /// The site's namespace generation: bumped whenever the namespace is invalidated.
    #[must_use]
    pub fn cache_generation(&self, site: &SiteId) -> String {
        format!("{}:cache:{}:gen", self.prefix, site)
    }

    // ---- coordinator ---------------------------------------------------------------------

    /// The hash of one lease (holder, token, id, expires_us).
    #[must_use]
    pub fn lease(&self, name: &str) -> String {
        format!("{}:coord:lease:{}", self.prefix, name)
    }

    /// The token high-water mark of one lease name.
    #[must_use]
    pub fn token(&self, name: &str) -> String {
        format!("{}:coord:token:{}", self.prefix, name)
    }

    /// The last value one counter issued.
    #[must_use]
    pub fn counter(&self, name: &str) -> String {
        format!("{}:coord:counter:{}", self.prefix, name)
    }

    /// One rate-limit bucket.
    #[must_use]
    pub fn rate(&self, key: &str) -> String {
        format!("{}:coord:rate:{}", self.prefix, key)
    }

    /// The presence key of one node (given a TTL when written).
    #[must_use]
    pub fn node(&self, node: &str) -> String {
        format!("{}:coord:node:{}", self.prefix, node)
    }

    /// The `MATCH` pattern of every presence key.
    #[must_use]
    pub fn node_pattern(&self) -> String {
        format!("{}:coord:node:*", self.prefix)
    }

    // ---- bus and version store -----------------------------------------------------------

    /// The pub/sub channel of one site.
    #[must_use]
    pub fn bus_channel(&self, site: &SiteId) -> String {
        format!("{}:bus:{}", self.prefix, site)
    }

    /// The version store's head record: a hash with `version` and `hash` fields.
    #[must_use]
    pub fn meta_head(&self) -> String {
        format!("{}:meta:head", self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_NAMESPACE, KeySpace};
    use spot_core::SiteId;

    fn site() -> SiteId {
        SiteId::new("acme.example").unwrap_or_else(|error| panic!("valid site: {error}"))
    }

    #[test]
    fn every_key_starts_with_the_prefix() {
        let ks = KeySpace::with_prefix("p:1");
        let s = site();
        for key in [
            ks.cache_data(&s, "meta:x"),
            ks.cache_sizes(&s),
            ks.cache_bytes(&s),
            ks.cache_generation(&s),
            ks.lease("job"),
            ks.token("job"),
            ks.counter("seq"),
            ks.rate("api"),
            ks.node("n1"),
            ks.bus_channel(&s),
            ks.meta_head(),
        ] {
            assert!(key.starts_with("p:1:"), "{key}");
        }
    }

    #[test]
    fn the_default_prefix_is_the_documented_one() {
        assert_eq!(KeySpace::default().prefix(), DEFAULT_NAMESPACE);
    }

    #[test]
    fn a_site_scopes_its_cache_keys() {
        let ks = KeySpace::default();
        let a = SiteId::new("a").unwrap_or_else(|e| panic!("{e}"));
        let b = SiteId::new("b").unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(ks.cache_data(&a, "k"), ks.cache_data(&b, "k"));
        assert_eq!(
            ks.cache_data(&a, "k"),
            "spot:cluster:redis:v1:cache:a:data:k"
        );
        assert_eq!(
            ks.cache_pattern(&a, "meta:"),
            "spot:cluster:redis:v1:cache:a:data:meta:*"
        );
        assert_eq!(
            ks.cache_all_pattern(&a),
            "spot:cluster:redis:v1:cache:a:data:*"
        );
    }

    #[test]
    fn the_bus_channel_is_per_site() {
        let ks = KeySpace::default();
        assert_eq!(
            ks.bus_channel(&site()),
            "spot:cluster:redis:v1:bus:acme.example"
        );
    }

    #[test]
    fn coordinator_keys_are_namespaced_by_name() {
        let ks = KeySpace::default();
        assert_eq!(ks.lease("a"), "spot:cluster:redis:v1:coord:lease:a");
        assert_ne!(ks.lease("a"), ks.lease("b"));
        assert_eq!(ks.token("a"), "spot:cluster:redis:v1:coord:token:a");
        assert_eq!(ks.counter("a"), "spot:cluster:redis:v1:coord:counter:a");
        assert_eq!(ks.rate("a"), "spot:cluster:redis:v1:coord:rate:a");
        assert_eq!(ks.node("n1"), "spot:cluster:redis:v1:coord:node:n1");
    }
}

/// Escapes the glob metacharacters of `SCAN ... MATCH`, so a caller-supplied key prefix matches
/// only keys that literally start with it and cannot widen a prefix invalidation.
fn escape_glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn a_prefix_with_glob_characters_is_matched_literally() {
        let keys = KeySpace::default();
        let site = SiteId::new("acme").unwrap_or_else(|e| panic!("valid site: {e}"));
        let pattern = keys.cache_pattern(&site, "a*b[1]?");
        assert!(pattern.ends_with("data:a\\*b\\[1\\]\\?*"), "{pattern}");
    }
}
