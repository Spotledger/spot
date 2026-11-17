//! Version 2 of the tail chunk (ADR-061 A.2.2, decisions D2 and D3).
//!
//! ```text
//! chunk = "SPOTJTL2" u32(count) { u64(generation) u64(seq) u32(ct_len) ct }* [32](mac)
//! ct    = the sealed form (spot-secrets) of the envelope line, associated data
//!         generation || seq || batch_id (three big-endian u64)
//! mac   = HMAC-SHA-256(k_tail, "spot.journal.tail.v2\0" || every byte before mac || object key)
//! ```
//!
//! `k_tail` is derived from the tenant journal key with the purpose `tail-mac`. The data key of
//! each entry is wrapped by the tenant journal key (inside `ct`). The MAC is verified before
//! anything is parsed or decrypted.

use spot_events_contract::envelope::Position;
use spot_secrets::{Sealed, SecretString};

use super::error::{ChunkError, EncodeError, Rule};
use super::keys::TailKeys;
use super::{Entry, MAGIC_V2, MAX_CHUNK_BYTES};

/// The MAC domain string (the NUL included).
pub(super) const DOMAIN: &[u8] = b"spot.journal.tail.v2\0";
const ENTRY_HEAD: usize = 8 + 8 + 4;

/// The associated data of one entry: `generation || seq || batch_id`.
fn aad(generation: u64, seq: u64, batch_id: u64) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[..8].copy_from_slice(&generation.to_be_bytes());
    out[8..16].copy_from_slice(&seq.to_be_bytes());
    out[16..].copy_from_slice(&batch_id.to_be_bytes());
    out
}

/// Encodes `entries` as a version 2 chunk to be stored under `object_key` as batch `batch_id`.
///
/// Stops with [`EncodeError::TooLarge`] as soon as the chunk would pass the 64 MiB limit, so an
/// oversize batch costs no more encryption than it must and nothing is written.
///
/// # Errors
/// [`EncodeError`].
pub async fn encode(
    keys: &TailKeys,
    object_key: &str,
    batch_id: u64,
    entries: &[Entry],
) -> Result<Vec<u8>, EncodeError> {
    let count = u32::try_from(entries.len()).map_err(|_| EncodeError::Count)?;
    let sealer = keys.secret_box();
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(MAGIC_V2);
    out.extend_from_slice(&count.to_be_bytes());
    for (position, line) in entries {
        let text = String::from_utf8(line.clone()).map_err(|_| EncodeError::NotUtf8)?;
        let ad = aad(position.generation, position.seq, batch_id);
        let sealed = sealer
            .seal(&SecretString::new(text), &ad)
            .await
            .map_err(|_| EncodeError::Key)?;
        let ct = sealed.to_bytes();
        let ct_len = u32::try_from(ct.len()).map_err(|_| EncodeError::Count)?;
        out.extend_from_slice(&position.generation.to_be_bytes());
        out.extend_from_slice(&position.seq.to_be_bytes());
        out.extend_from_slice(&ct_len.to_be_bytes());
        out.extend_from_slice(&ct);
        let size = u64::try_from(out.len())
            .unwrap_or(u64::MAX)
            .saturating_add(32);
        if size > MAX_CHUNK_BYTES {
            return Err(EncodeError::TooLarge { bytes: size });
        }
    }
    let mac = keys
        .mac_current(&[DOMAIN, &out, object_key.as_bytes()])
        .await
        .map_err(|_| EncodeError::Key)?;
    out.extend_from_slice(&mac);
    let size = u64::try_from(out.len()).unwrap_or(u64::MAX);
    if size > MAX_CHUNK_BYTES {
        return Err(EncodeError::TooLarge { bytes: size });
    }
    Ok(out)
}

/// Verifies the MAC (A.3 (b)) before anything is parsed, then parses (A.3 (c)) and decrypts.
/// `bytes` is the whole object and starts with the version 2 magic.
pub(super) async fn decode(
    keys: Option<&TailKeys>,
    object_key: &str,
    batch_id: u64,
    bytes: &[u8],
) -> Result<Vec<Entry>, ChunkError> {
    let body_len = bytes.len().checked_sub(32).ok_or(ChunkError::new(
        Rule::B,
        "the object is shorter than 32 bytes",
    ))?;
    let (body, mac) = bytes.split_at(body_len);
    let keys = keys.ok_or(ChunkError::new(
        Rule::B,
        "a version 2 chunk cannot be verified without the tail keys",
    ))?;
    if !keys
        .verify_mac(&[DOMAIN, body, object_key.as_bytes()], mac)
        .await
    {
        return Err(ChunkError::new(Rule::B, "the MAC does not verify"));
    }
    let raw = parse(body)?;
    let sealer = keys.secret_box();
    let mut entries = Vec::with_capacity(raw.len());
    for (position, ct) in raw {
        let sealed = Sealed::from_bytes(ct)
            .map_err(|_| ChunkError::new(Rule::C, "an entry is not a sealed value"))?;
        let ad = aad(position.generation, position.seq, batch_id);
        let line = sealer
            .open(&sealed, &ad)
            .await
            .map_err(|_| ChunkError::new(Rule::C, "an entry does not open at its position"))?;
        entries.push((position, line.expose().as_bytes().to_vec()));
    }
    Ok(entries)
}

/// The structure of a verified body: positions and the sealed bytes, nothing decrypted yet.
fn parse(body: &[u8]) -> Result<Vec<(Position, &[u8])>, ChunkError> {
    let bad = |reason| ChunkError::new(Rule::C, reason);
    let rest = body
        .strip_prefix(MAGIC_V2.as_slice())
        .ok_or_else(|| bad("the magic is not version 2"))?;
    let (count, mut rest) = rest
        .split_first_chunk::<4>()
        .ok_or_else(|| bad("the entry count is missing"))?;
    let count = u32::from_be_bytes(*count);
    let mut raw = Vec::new();
    for _ in 0..count {
        if rest.len() < ENTRY_HEAD {
            return Err(bad("an entry is cut short"));
        }
        let (head, tail) = rest.split_at(ENTRY_HEAD);
        let (generation, seq, len) =
            head_fields(head).ok_or_else(|| bad("an entry is cut short"))?;
        let len = usize::try_from(len).map_err(|_| bad("a length does not fit"))?;
        if tail.len() < len {
            return Err(bad("a sealed line runs past the end"));
        }
        let (ct, after) = tail.split_at(len);
        raw.push((Position { generation, seq }, ct));
        rest = after;
    }
    if rest.is_empty() {
        Ok(raw)
    } else {
        Err(bad("bytes remain after the last entry"))
    }
}

fn head_fields(head: &[u8]) -> Option<(u64, u64, u32)> {
    let generation = u64::from_be_bytes(head.get(0..8)?.try_into().ok()?);
    let seq = u64::from_be_bytes(head.get(8..16)?.try_into().ok()?);
    let len = u32::from_be_bytes(head.get(16..20)?.try_into().ok()?);
    Some((generation, seq, len))
}
