//! Canonical associated data for the sealing box and the reveal path.
//!
//! Every layer has its own domain tag and every variable-length part is length-prefixed, so no
//! two bindings collide by concatenation. A sealed value therefore cannot be moved to another
//! tenant, DocType, field or document.

use crate::class::SyncClass;
use spot_protected_keys::TenantId;

fn push(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// The place a sealed value belongs to: the associated data the box authenticates.
///
/// An unbound value (the plain `seal(value, key)` form) uses the empty place, which is what a
/// caller that seals something without a document wants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SealedAd {
    tenant: Option<TenantId>,
    doctype: Option<String>,
    field: Option<String>,
    document: Option<String>,
}

impl SealedAd {
    /// The empty place: a value sealed without a document binding.
    #[must_use]
    pub fn unbound() -> Self {
        Self::default()
    }

    /// A value bound to one field of one document of one tenant.
    #[must_use]
    pub fn of_field(
        tenant: TenantId,
        doctype: impl Into<String>,
        field: impl Into<String>,
        document: impl Into<String>,
    ) -> Self {
        Self {
            tenant: Some(tenant),
            doctype: Some(doctype.into()),
            field: Some(field.into()),
            document: Some(document.into()),
        }
    }

    /// The tenant, when bound.
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantId> {
        self.tenant.as_ref()
    }

    /// The DocType, when bound.
    #[must_use]
    pub fn doctype(&self) -> Option<&str> {
        self.doctype.as_deref()
    }

    /// The field, when bound.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// The document, when bound.
    #[must_use]
    pub fn document(&self) -> Option<&str> {
        self.document.as_deref()
    }

    /// The canonical bytes the box authenticates.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(b"spot-sealed-fields/box/v1");
        for part in [
            self.tenant.as_ref().map(TenantId::as_str),
            self.doctype.as_deref(),
            self.field.as_deref(),
            self.document.as_deref(),
        ] {
            match part {
                Some(text) => {
                    out.push(1);
                    push(&mut out, text.as_bytes());
                }
                None => out.push(0),
            }
        }
        out
    }
}

/// The canonical bytes a reveal audit record commits to: the tenant, the document, the field
/// and the API. It keeps a record from being replayed against another target.
#[must_use]
pub fn reveal_subject(tenant: &TenantId, doctype: &str, document: &str, field: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"spot-sealed-fields/reveal/v1");
    push(&mut out, tenant.as_str().as_bytes());
    push(&mut out, doctype.as_bytes());
    push(&mut out, document.as_bytes());
    push(&mut out, field.as_bytes());
    out
}

/// The canonical bytes of a sync-class table row, for stable hashing and logging.
#[must_use]
pub fn class_row(group: &str, class: SyncClass) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"spot-sealed-fields/class/v1");
    push(&mut out, group.as_bytes());
    out.push(match class {
        SyncClass::Everywhere => 1,
        SyncClass::Encrypted => 2,
        SyncClass::Sealed => 3,
        SyncClass::HomeOnly => 4,
    });
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_unbound_place_differs_from_a_bound_one() {
        let tenant = TenantId::new("acme").expect("tenant");
        let bound = SealedAd::of_field(tenant, "Bank Account", "iban", "BA-1");
        assert_ne!(
            bound.canonical_bytes(),
            SealedAd::unbound().canonical_bytes()
        );
        assert_eq!(bound.field(), Some("iban"));
        assert_eq!(bound.doctype(), Some("Bank Account"));
        assert_eq!(bound.document(), Some("BA-1"));
    }

    #[test]
    fn two_places_never_collide_by_concatenation() {
        let tenant = TenantId::new("acme").expect("tenant");
        let a = SealedAd::of_field(tenant.clone(), "AB", "c", "d");
        let b = SealedAd::of_field(tenant, "A", "Bc", "d");
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    }
}
