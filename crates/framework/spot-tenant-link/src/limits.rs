//! The byte bound on the link wire (WP-CTL-09.T11).
//!
//! The wire types are plain `serde` types that a transport decodes. A transport that parses first
//! and checks later has already allocated whatever the peer declared, so the check comes first:
//! [`WireLimits::check`] answers from a length alone, and [`WireLimits::read`] reads at most the
//! bound plus one byte from a reader, so an oversize message is refused after at most that many
//! bytes were held, never after its declared size was allocated.

use std::io::Read;

use crate::error::{Error, Result};

/// The default maximum encoded size of one inbound message, in bytes (256 KiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// The largest bound that can be configured (16 MiB).
pub const MAX_CONFIGURABLE_BYTES: usize = 16 * 1024 * 1024;

/// How big an inbound message may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireLimits {
    max_message_bytes: usize,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

impl WireLimits {
    /// A bound of `max_message_bytes`, between 1 and [`MAX_CONFIGURABLE_BYTES`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidOffer`] when the bound is zero or above the maximum.
    pub fn new(max_message_bytes: usize) -> Result<Self> {
        if max_message_bytes == 0 || max_message_bytes > MAX_CONFIGURABLE_BYTES {
            return Err(Error::InvalidOffer {
                reason: "the message size bound is between 1 byte and 16 MiB",
            });
        }
        Ok(Self { max_message_bytes })
    }

    /// The bound, in bytes.
    #[must_use]
    pub fn max_message_bytes(&self) -> usize {
        self.max_message_bytes
    }

    /// Refuses a message of `encoded_len` bytes (a length the transport knows before it reads:
    /// a frame header, a content length, the length of a buffer it already holds).
    ///
    /// # Errors
    ///
    /// [`Error::MessageTooLarge`] above the bound; a message of exactly the bound passes.
    pub fn check(&self, encoded_len: u64) -> Result<()> {
        if encoded_len > self.max_message_bytes as u64 {
            return Err(Error::MessageTooLarge {
                limit: self.max_message_bytes as u64,
            });
        }
        Ok(())
    }

    /// Reads one message from `reader`, at most the bound. Nothing larger is returned and the
    /// reader is never asked for more than the bound plus one byte.
    ///
    /// # Errors
    ///
    /// [`Error::MessageTooLarge`] when the reader holds more than the bound;
    /// [`Error::StoreFailed`] is not used: a read failure is reported as
    /// [`Error::InvalidOffer`] with a fixed reason.
    pub fn read(&self, reader: impl Read) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let cap = self.max_message_bytes as u64 + 1;
        reader
            .take(cap)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::InvalidOffer {
                reason: "the message could not be read",
            })?;
        self.check(bytes.len() as u64)?;
        Ok(bytes)
    }
}
