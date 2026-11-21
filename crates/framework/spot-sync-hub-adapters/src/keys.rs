//! The seam for the hub's own metadata: which scope keys a record requires.

use spot_sync_closure::RecordKey;
use spot_sync_master::ScopeKey;

/// The hub's metadata about the scope keys a record requires. The hub supplies the
/// implementation from its registry; a stream entry carries none.
///
/// Each key is text in the form `key_type:value`, split at the **first** colon (see
/// [`encode_scope_key`] and [`decode_scope_key`]).
pub trait ScopeKeysOf {
    /// The scope keys the record requires, or `None` when the hub does not know. `None` is
    /// treated as not covered. `Some(vec![])` means the record is for every subscriber of its
    /// entity.
    fn scope_keys(&self, entry_key: &RecordKey) -> Option<Vec<String>>;
}

impl<T: ScopeKeysOf + ?Sized> ScopeKeysOf for &T {
    fn scope_keys(&self, entry_key: &RecordKey) -> Option<Vec<String>> {
        (**self).scope_keys(entry_key)
    }
}

/// The text form of a scope key, or `None` when the key type holds a colon (the form would be
/// ambiguous, so it is refused rather than guessed).
#[must_use]
pub fn encode_scope_key(key: &ScopeKey) -> Option<String> {
    if key.key_type().contains(':') {
        return None;
    }
    Some(format!("{}:{}", key.key_type(), key.value()))
}

/// Parse the text form, splitting at the first colon. `None` for text without a colon or with an
/// invalid part.
#[must_use]
pub fn decode_scope_key(text: &str) -> Option<ScopeKey> {
    let (key_type, value) = text.split_once(':')?;
    ScopeKey::new(key_type, value).ok()
}
