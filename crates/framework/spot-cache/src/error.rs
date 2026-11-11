//! Typed, matchable cache errors.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// Result of every fallible cache command.
pub type CacheResult<T> = Result<T, CacheError>;

/// What went wrong, as a stable, matchable kind; callers never parse messages.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CacheError {
    /// A command met a key holding a value of another type: a structure command on plain
    /// bytes or on another structure, or `get`/`incr_by` on a hash, list or set.
    WrongType,
    /// The stored value is not an ASCII decimal integer ([`crate::store::CacheStore::incr_by`]).
    NotAnInteger,
    /// The increment would not fit a 64-bit integer ([`crate::store::CacheStore::incr_by`]).
    Overflow,
    /// The value — or the structure the write would produce — is larger than the site's
    /// quota, or the global cap, so nothing was stored and the key is unchanged.
    QuotaExceeded,
    /// The loader handed to [`crate::store::CacheStore::get_or_load`] failed; the failure reaches every
    /// caller waiting on the same load and is never cached. The boxed error is shared by all
    /// waiters, hence the `Arc`.
    Loader(Arc<dyn Error + Send + Sync + 'static>),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType => write!(f, "value has the wrong type for this command"),
            Self::NotAnInteger => write!(f, "value is not an integer"),
            Self::Overflow => write!(f, "increment would overflow a 64-bit integer"),
            Self::QuotaExceeded => write!(f, "value is larger than the site's cache quota"),
            Self::Loader(error) => write!(f, "cache loader failed: {error}"),
        }
    }
}

impl Error for CacheError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Loader(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
