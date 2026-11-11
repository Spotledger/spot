//! Opaque, versioned position tokens bound to a consumer name.
//!
//! The wire (`id:` in an SSE frame, `Last-Event-ID` on resume) carries a token, never a raw
//! `{generation, seq}`: the token is base64url of a small versioned record that names the
//! consumer it was issued for. A token from another consumer, a forged token, an unknown
//! version or a corrupt encoding is refused with `eventstream.position_invalid`, so a consumer
//! can never resume at another consumer's position.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use spot_events_contract::envelope::Position;
use spot_storage_contract::FeedPosition;

use crate::error::StreamError;

/// The token version; a token of any other version is refused.
const TOKEN_VERSION: &str = "v1";

/// The field separator inside the encoded record. A consumer name is a storage name
/// (`[a-z][a-z0-9_]*`), which never contains it.
const SEP: char = '|';

/// A [`FeedPosition`] as the contract's [`Position`].
#[must_use]
pub fn position_from_feed(p: FeedPosition) -> Position {
    Position {
        generation: p.generation(),
        seq: p.seq(),
    }
}

/// A [`Position`] as the storage [`FeedPosition`].
#[must_use]
pub fn position_to_feed(p: Position) -> FeedPosition {
    FeedPosition::new(p.generation, p.seq)
}

/// Whether a position is the feed's start (`generation 0, seq 0`).
#[must_use]
pub fn is_start(p: Position) -> bool {
    p.generation == 0 && p.seq == 0
}

/// Encodes an opaque position token for `consumer`.
#[must_use]
pub fn encode_position(consumer: &str, position: Position) -> String {
    let record = format!(
        "{TOKEN_VERSION}{SEP}{consumer}{SEP}{}{SEP}{}",
        position.generation, position.seq
    );
    URL_SAFE_NO_PAD.encode(record.as_bytes())
}

/// Parses an opaque position token, refusing one issued for another consumer.
///
/// # Errors
///
/// [`StreamError::PositionInvalid`] when the token is not base64url, not UTF-8, of another
/// version, malformed, or bound to a different consumer.
pub fn parse_position(consumer: &str, token: &str) -> Result<Position, StreamError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.trim())
        .map_err(|_| invalid("the token is not base64url"))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid("the token is not UTF-8"))?;
    let mut parts = text.split(SEP);
    let version = parts.next().ok_or_else(|| invalid("the token is empty"))?;
    if version != TOKEN_VERSION {
        return Err(invalid("the token version is not supported"));
    }
    let bound = parts
        .next()
        .ok_or_else(|| invalid("the token names no consumer"))?;
    if bound != consumer {
        return Err(invalid("the token was issued for another consumer"));
    }
    let generation = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| invalid("the token has no generation"))?;
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| invalid("the token has no sequence"))?;
    if parts.next().is_some() {
        return Err(invalid("the token has trailing fields"));
    }
    Ok(Position { generation, seq })
}

fn invalid(reason: &str) -> StreamError {
    StreamError::PositionInvalid {
        reason: reason.to_owned(),
    }
}
