//! The effective metadata a report resolves against, at one version.
//!
//! It wraps the metadata the loader built, so a report over a DocType the site does not have is
//! refused (`report.base_missing`) rather than compiled against nothing.
//!
//! A **version is not an identity**. The version rises with every change a node makes to its
//! metadata, but an adopted snapshot or a rollback can serve different DocTypes at a version
//! this process has already used, so two different metadata views can report one version. What
//! a compiled report may be reused for is a [`MetaIdentity`]: the version together with a
//! content digest of the DocTypes the set holds.

use std::sync::{Arc, OnceLock};

use sha2::{Digest as _, Sha256};
use spot_app_api::meta::DocTypeDef;
use spot_meta::{Definition, EffectiveMeta};

/// The effective metadata a report resolves against, at one version.
#[derive(Clone, Debug)]
pub struct EffectiveMetaSet {
    meta: Arc<EffectiveMeta>,
    /// Computed once per set: the digest covers every definition, and a set is cloned per use,
    /// so computing it eagerly would hash the whole metadata on every request.
    digest: OnceLock<String>,
}

impl EffectiveMetaSet {
    /// A set over one effective metadata snapshot.
    #[must_use]
    pub fn new(meta: Arc<EffectiveMeta>) -> Self {
        EffectiveMetaSet {
            meta,
            digest: OnceLock::new(),
        }
    }

    /// The metadata version.
    ///
    /// It rises whenever this node's metadata changes, and it says which version the site was
    /// at. It does **not** promise that a report compiles once per version, and nothing may key
    /// a compiled report by it alone: a rollback or a snapshot adopted from another node serves
    /// different DocTypes at a version this node already used. Use [`EffectiveMetaSet::identity`]
    /// (the version and [`EffectiveMetaSet::digest`]) for that.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.meta.version()
    }

    /// A content digest of the DocTypes this set holds.
    ///
    /// Every definition is hashed in name order, so two sets with equal content share a digest
    /// whatever their `Arc` or their version, and two sets with different content never do.
    /// This is what makes a compiled report safe to reuse: the report was built from these
    /// definitions and no others.
    #[must_use]
    pub fn digest(&self) -> &str {
        self.digest
            .get_or_init(|| content_digest(self.meta.definitions()))
    }

    /// The identity a compiled report may be cached by: the version and the content digest.
    #[must_use]
    pub fn identity(&self) -> MetaIdentity {
        MetaIdentity {
            version: self.version(),
            digest: self.digest().to_owned(),
        }
    }

    /// The app view of one DocType, when the site has it.
    #[must_use]
    pub fn doctype(&self, name: &str) -> Option<Arc<DocTypeDef>> {
        self.meta.doctype(name)
    }

    /// The underlying metadata.
    #[must_use]
    pub fn meta(&self) -> &EffectiveMeta {
        &self.meta
    }
}

/// The identity of one metadata view: the version it reports and a content digest of the
/// DocTypes it holds.
///
/// Two sets with equal identities compile a report to the same thing; equal versions alone do
/// not promise that.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetaIdentity {
    version: u64,
    digest: String,
}

impl MetaIdentity {
    /// The identity of `set`.
    #[must_use]
    pub fn of(set: &EffectiveMetaSet) -> Self {
        set.identity()
    }

    /// The metadata version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The content digest of the DocTypes.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// The SHA-256 of every definition's canonical form, in the order given (name order).
fn content_digest(definitions: &[Definition]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-reports/effective-meta\0");
    for definition in definitions {
        hasher.update(definition.name.as_bytes());
        hasher.update([0]);
        // A `Definition` is strings, enums, booleans and vectors, and the builder keeps them in
        // name order, so its JSON is the same bytes for equal content. A definition that ever
        // failed to serialise still separates its DocType by name, which is all a cache key
        // needs: it must differ when the content differs, never silently match.
        match serde_json::to_vec(definition) {
            Ok(bytes) => hasher.update(&bytes),
            Err(error) => {
                hasher.update(b"\0unserialisable\0");
                hasher.update(error.to_string().as_bytes());
            }
        }
        hasher.update([0]);
    }
    hex(&hasher.finalize())
}

/// Lowercase hex of the digest, as the platform's content hashes print.
fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use spot_meta::{FieldSpec, FieldType, Registry};

    fn set(fields: Vec<FieldSpec>) -> EffectiveMetaSet {
        let mut journal = Definition::new("journal");
        journal.fields = fields;
        // Two independently built registries report the same version, as two sites that have
        // made the same number of changes do.
        EffectiveMetaSet::new(
            Registry::new(vec![journal])
                .expect("the fixture metadata builds")
                .effective(),
        )
    }

    fn total() -> FieldSpec {
        let mut total = FieldSpec::new("total", FieldType::Decimal);
        total.scale = Some(2);
        total
    }

    #[test]
    fn the_digest_is_content_bound_not_version_bound() {
        let bare = set(vec![total()]);
        let extended = set(vec![total(), FieldSpec::new("memo", FieldType::Text)]);
        assert_eq!(bare.version(), extended.version(), "one version");
        assert_ne!(
            bare.digest(),
            extended.digest(),
            "different DocTypes, different digests"
        );
        assert_ne!(bare.identity(), extended.identity());

        let again = set(vec![total()]);
        assert_eq!(
            bare.digest(),
            again.digest(),
            "equal content shares a digest whatever the Arc"
        );
        assert_eq!(bare.identity(), again.identity());
        assert_eq!(bare.digest().len(), 64, "a full SHA-256 in hex");
    }
}
