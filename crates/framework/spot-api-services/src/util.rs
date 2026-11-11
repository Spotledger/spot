//! Request-to-invocation mapping. Identity is resolved by the caller, not this crate.
use spot_app_api::context::{Actor, Invocation};
use spot_core::Clock;
use spot_events_contract::CommandId;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, RequestCtx};

/// Preserve site, resolved identity, command id, language and the clock's business time.
/// Invalid command ids fail rather than silently losing idempotency information.
pub fn build_invocation(
    ctx: &RequestCtx,
    clock: &dyn Clock,
    actor: Actor,
) -> Result<Invocation, ApiError> {
    let command_id = ctx
        .idempotency_key
        .as_deref()
        .map(CommandId::new)
        .transpose()
        .map_err(|_| ApiError::from_code(Code::ApiInvalidRequest))?;
    Ok(Invocation {
        site: ctx.site.clone(),
        actor,
        command_id,
        language: ctx.language.clone(),
        now: clock.now_utc(),
    })
}
