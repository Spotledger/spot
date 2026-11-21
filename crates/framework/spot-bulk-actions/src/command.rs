//! The deterministic command id of one document's call.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use spot_app_api::prelude::{CommandId, RecordId};

/// The command id for `action_key` on `id` in the batch `label`.
///
/// The readable form is `bulk:<label>:<action>:<record id>`. A form that is not a valid command
/// id (longer than 128 characters, or with a character outside visible ASCII, which a label may
/// have) is replaced by `bulkh:` and the SHA-256 of the length-prefixed parts, hex encoded.
/// Both forms are pure functions of the three inputs: no clock, no randomness, so a re-run
/// after a crash derives the same ids.
pub(crate) fn derive(label: &str, action_key: &str, id: &RecordId) -> Option<CommandId> {
    let plain = format!("bulk:{label}:{action_key}:{id}");
    if let Ok(command) = CommandId::new(&plain) {
        return Some(command);
    }
    let mut hasher = Sha256::new();
    for part in [label, action_key, &id.to_string()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let mut hashed = String::with_capacity(6 + 64);
    hashed.push_str("bulkh:");
    for byte in hasher.finalize() {
        // Writing to a String cannot fail.
        let _ = write!(hashed, "{byte:02x}");
    }
    CommandId::new(&hashed).ok()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn rid(n: u8) -> RecordId {
        let mut bytes = [0u8; 16];
        bytes[0] = 0x01;
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = n;
        RecordId::from_bytes(bytes).expect("a v7 id")
    }

    #[test]
    fn readable_form_is_used_when_it_fits() {
        let id = derive("march", "post", &rid(1)).expect("valid");
        assert!(id.as_str().starts_with("bulk:march:post:"));
    }

    #[test]
    fn long_or_spaced_labels_are_hashed_and_stable() {
        let spaced = derive("post march drafts", "post", &rid(1)).expect("valid");
        assert!(spaced.as_str().starts_with("bulkh:"));
        assert_eq!(spaced.as_str().len(), 6 + 64);
        let again = derive("post march drafts", "post", &rid(1)).expect("valid");
        assert_eq!(spaced, again);
        let other = derive("post march drafts", "post", &rid(2)).expect("valid");
        assert_ne!(spaced, other);
    }

    #[test]
    fn label_action_and_document_all_matter() {
        let a = derive("a", "post", &rid(1));
        assert_ne!(a, derive("b", "post", &rid(1)));
        assert_ne!(a, derive("a", "cancel", &rid(1)));
        assert_ne!(a, derive("a", "post", &rid(2)));
    }
}
