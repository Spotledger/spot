//! API key verifiers: parsing and the at-rest digest encoding. No key secret is persisted or logged.
use crate::config::Key;
use crate::{Error, Result};
use spot_auth::{StoredApiKey, TokenDigest};

pub(crate) fn stored(key: &Key) -> Result<StoredApiKey> {
    if key.key_id.len() != 12
        || !key
            .key_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        || key.digest.len() != 64
        || !key.digest.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error("invalid key verifier".into()));
    }
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&key.digest[2 * i..2 * i + 2], 16)
            .map_err(|_| Error("invalid digest".into()))?;
    }
    spot_http::api::codec::parse_record_id(&key.user)
        .ok_or_else(|| Error("invalid API key user".into()))?;
    Ok(StoredApiKey {
        key_id: key.key_id.clone(),
        digest: TokenDigest::from_bytes(&bytes).map_err(|_| Error("invalid digest".into()))?,
    })
}
pub(crate) fn encode_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
