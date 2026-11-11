use crate::crypto::NONCE_LEN;
use crate::error::SecretError;
use crate::provider::{KeyRef, WrappedKey};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::fmt;
use std::str::FromStr;

const MAGIC: &[u8; 4] = b"SPS1";
const TEXT_PREFIX: &str = "spsec:v1:";
/// Poly1305 tag length.
const TAG_LEN: usize = 16;

/// A sealed secret: versioned, self-describing and safe to store.
///
/// Binary layout: magic `SPS1`, KEK id (length byte + UTF-8), KEK version (u32 big endian),
/// wrapped data key (u16 big endian length + bytes), then the value's nonce and ciphertext.
/// The text form is `spsec:v1:<base64url>` of the binary form.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub(crate) wrapped: WrappedKey,
    /// Nonce followed by ciphertext of the value.
    pub(crate) body: Vec<u8>,
}

impl Sealed {
    /// The KEK id and version that wrap this value's data key.
    pub fn key_ref(&self) -> &KeyRef {
        &self.wrapped.key
    }

    /// The binary form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let id = self.wrapped.key.id.as_bytes();
        let mut out =
            Vec::with_capacity(11 + id.len() + self.wrapped.bytes.len() + self.body.len());
        out.extend_from_slice(MAGIC);
        // Lengths are validated by `SecretBox` (id) and fixed by the providers (wrapped key).
        out.push(u8::try_from(id.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(id);
        out.extend_from_slice(&self.wrapped.key.version.to_be_bytes());
        out.extend_from_slice(
            &u16::try_from(self.wrapped.bytes.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(&self.wrapped.bytes);
        out.extend_from_slice(&self.body);
        out
    }

    /// Parses the binary form. Never panics on arbitrary input.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SecretError> {
        let mut r = Reader(bytes);
        if r.take(4)? != MAGIC {
            return Err(SecretError::Malformed("bad magic"));
        }
        let id_len = usize::from(r.take(1)?[0]);
        let id = std::str::from_utf8(r.take(id_len)?)
            .map_err(|_| SecretError::Malformed("key id is not UTF-8"))?
            .to_owned();
        let version = u32::from_be_bytes(array(r.take(4)?));
        let wrapped_len = usize::from(u16::from_be_bytes(array(r.take(2)?)));
        let wrapped = r.take(wrapped_len)?.to_vec();
        let body = r.0.to_vec();
        if body.len() < NONCE_LEN + TAG_LEN {
            return Err(SecretError::Malformed("value shorter than nonce and tag"));
        }
        Ok(Self {
            wrapped: WrappedKey {
                key: KeyRef { id, version },
                bytes: wrapped,
            },
            body,
        })
    }
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(bytes);
    out
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SecretError> {
        if self.0.len() < n {
            return Err(SecretError::Malformed("truncated"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
}

impl fmt::Display for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{TEXT_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(self.to_bytes())
        )
    }
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("key", &self.wrapped.key)
            .finish_non_exhaustive()
    }
}

impl FromStr for Sealed {
    type Err = SecretError;

    fn from_str(text: &str) -> Result<Self, SecretError> {
        let rest = text
            .strip_prefix(TEXT_PREFIX)
            .ok_or(SecretError::Malformed("missing spsec:v1: prefix"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(rest)
            .map_err(|_| SecretError::Malformed("not base64url"))?;
        Self::from_bytes(&bytes)
    }
}
