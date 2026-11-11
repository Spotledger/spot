//! Typed errors with stable `journal_store.*` codes. Messages never carry object bytes.

/// Every error the store trait, the layout and both backends return.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A key, tenant id or key component is not in the layout's canonical form.
    #[error("the object key is not a canonical journal key: {0}")]
    KeyInvalid(&'static str),
    /// [`crate::JournalStore::put_new`] found the key already taken. Objects are immutable per
    /// generation, so this is a refusal, not a retry.
    #[error("the object {key} already exists")]
    AlreadyExists {
        /// The key that is already taken.
        key: String,
    },
    /// The key does not exist.
    #[error("no object {key} exists")]
    NotFound {
        /// The missing key.
        key: String,
    },
    /// [`crate::JournalStore::put_replace`] was given an etag that is not the stored one.
    #[error("the etag of {key} is not {expected}")]
    EtagMismatch {
        /// The key whose etag differed.
        key: String,
        /// The etag the caller expected.
        expected: String,
    },
    /// [`crate::JournalStore::put_replace`] was offered a key that is not a manifest: only the
    /// manifest is replaced within a generation, and only through a guarded replace.
    #[error("the object {key} is not a manifest and cannot be replaced")]
    ReplaceRefused {
        /// The key that may not be replaced.
        key: String,
    },
    /// [`crate::JournalStore::delete`] was offered a key under `segments/` or `snapshots/`.
    #[error("the object {key} is under a prefix that may not be deleted")]
    DeleteRefused {
        /// The key that may not be deleted.
        key: String,
    },
    /// A filesystem or provider operation failed.
    #[error("the object store failed to {operation} {key}: {message}")]
    Io {
        /// The operations that failed (`write`, `read`, `list`, ...).
        operation: &'static str,
        /// The key the operation was about.
        key: String,
        /// The operating system's or provider's message.
        message: String,
    },
    /// The backend cannot serve the call at all (for example a poisoned in-memory lock).
    #[error("the store is unavailable: {message}")]
    Backend {
        /// What is wrong with the backend.
        message: String,
    },
}

impl StoreError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyInvalid(_) => "journal_store.key_invalid",
            Self::AlreadyExists { .. } => "journal_store.already_exists",
            Self::NotFound { .. } => "journal_store.not_found",
            Self::EtagMismatch { .. } => "journal_store.etag_mismatch",
            Self::ReplaceRefused { .. } => "journal_store.replace_refused",
            Self::DeleteRefused { .. } => "journal_store.delete_refused",
            Self::Io { .. } => "journal_store.io",
            Self::Backend { .. } => "journal_store.backend",
        }
    }
}
