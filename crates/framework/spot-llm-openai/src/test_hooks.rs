//! Test hooks: the outbound request and internal mappings, exposed for the integration
//! tests so they can assert on the wire shapes without a real HTTP call. Not a stable
//! API — tests only.

use spot_llm::request::Request;
use spot_llm::response::Response;
use spot_llm::stream::StreamEvent;
use spot_llm::transport::HttpRequest;

use crate::profile::Profile;
use crate::provider::OpenAiProvider;

/// Builds the outbound HTTP request for `request` exactly as `complete` and `stream`
/// would. `streaming` sets the wire body's `stream` flag. Failing builds panic: the test
/// only sends buildable requests.
pub fn outbound_for(provider: &OpenAiProvider, request: &Request, streaming: bool) -> HttpRequest {
    match provider.outbound(request, streaming) {
        Ok(outbound) => outbound,
        Err(error) => panic!("the test request must build: {error}"),
    }
}

/// The request-only `Outbound` the request builder produces.
pub fn outbound_body_for(provider: &OpenAiProvider, request: &Request) -> crate::wire::Outbound {
    match crate::wire::outbound(request, provider.profile(), provider.models(), false) {
        Ok(outbound) => outbound,
        Err(error) => panic!("the test request must build: {error}"),
    }
}

/// Formats a temperature permille the way the wire builder does.
pub fn temperature_text_for(permille: u16) -> Option<String> {
    crate::wire::temperature_text(permille)
}

/// The wire's `finish_reason` mapping.
pub fn finish_reason_of_str(name: &str) -> Result<spot_llm::response::StopReason, String> {
    crate::finish_reason::map(name)
}

/// Builds the synthetic event list a parsed response replays as.
pub fn events_of_for_test(response: &Response) -> Vec<StreamEvent> {
    crate::provider::events_of(response)
}

/// The profile the provider holds.
pub fn profile_of(provider: &OpenAiProvider) -> Profile {
    provider.profile().clone()
}
