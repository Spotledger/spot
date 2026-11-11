//! The one function every edge calls on a client-supplied command id.
//!
//! A command id is an idempotency key the caller chooses, and a grant travels as one. Without a
//! check, a caller could pass a **leaked grant** as its idempotency key and the engine would
//! register it as an ordinary key — the grant would then be replayed by every retry of that call
//! instead of dying with its attempt. Worse, the old plain `request:node:attempt` shape is
//! harmless only because the authority refuses to read a command id as a request context; an edge
//! that ever parsed one would be trusting the caller.
//!
//! So both shapes are **reserved**. An edge refuses them outright and loudly
//! ([`GrantError::CommandIdReserved`]) instead of stripping them: a stripped command id silently
//! loses idempotency, and the caller that sent a grant as a key has a bug that must surface.
//!
//! [`GrantError::CommandIdReserved`]: crate::GrantError::CommandIdReserved

use crate::error::GrantError;
use crate::grant::GRANT_PREFIX;

/// Whether `command_id` is shaped like a request grant and must not be accepted as a command id.
///
/// True for anything starting with `rg1.` — a prefix is enough, because a caller has no business
/// sending one at all — and for anything shaped like the old plain `request:node:attempt`
/// (`request-id:step:3`), which is the form the pre-authority code read as a request context.
///
/// False for ordinary idempotency keys: a UUID, `import-2026-10-21-0042`, an `order-4711-retry`.
#[must_use]
pub fn is_grant_shaped(command_id: &str) -> bool {
    command_id.starts_with(GRANT_PREFIX) || is_legacy_context_shaped(command_id)
}

/// The old plain `request:node:attempt` shape: three colon-separated, non-blank parts whose last
/// is the attempt number.
fn is_legacy_context_shaped(command_id: &str) -> bool {
    let mut parts = command_id.split(':');
    let (Some(request), Some(node), Some(attempt), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let shaped = |part: &str| !part.is_empty() && !part.chars().any(char::is_whitespace);
    shaped(request)
        && shaped(node)
        && !attempt.is_empty()
        && attempt.bytes().all(|byte| byte.is_ascii_digit())
        && attempt.parse::<u32>().is_ok()
}

/// Refuses `command_id` when it is shaped like a grant, and accepts it otherwise.
///
/// An edge calls this before it uses a client-supplied command id, and returns the error
/// unchanged: it carries `bpm_request_authority.command_id_reserved` and its message never
/// repeats the id, which may itself be a credential.
///
/// # Errors
/// [`GrantError::CommandIdReserved`] when [`is_grant_shaped`] is true.
pub fn refuse_grant_shaped(command_id: &str) -> Result<(), GrantError> {
    if is_grant_shaped(command_id) {
        return Err(GrantError::CommandIdReserved);
    }
    Ok(())
}
