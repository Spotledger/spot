//! Test hooks: the outbound request and a couple of internal mappings, exposed for the
//! integration tests so they can assert on the wire shapes without a real HTTP call. Not a
//! stable API — tests only.

use spot_llm::request::Request;
use spot_llm::response::Response;
use spot_llm::stream::StreamEvent;
use spot_llm::transport::HttpRequest;

use crate::provider::{AnthropicOptions, AnthropicProvider};

/// Builds the outbound HTTP request for `request` exactly as `complete` and `stream`
/// would. `streaming` sets the wire body's `stream` flag. Failing builds panic: the tests
/// only send buildable requests.
pub fn outbound_for(
    provider: &AnthropicProvider,
    request: &Request,
    options: &AnthropicOptions,
) -> HttpRequest {
    match provider.outbound(request, options, false) {
        Ok(outbound) => outbound,
        Err(error) => panic!("the test request must build: {error}"),
    }
}

/// Formats a temperature permille the way the wire builder does.
pub fn temperature_text_for(permille: u16) -> Option<String> {
    crate::wire::temperature_text(permille)
}

/// The base URL the provider holds (the debug echo's source).
pub fn provider_base_url(provider: &AnthropicProvider) -> String {
    provider.debug_base_url()
}

/// Builds the synthetic event list a parsed response replays as.
pub fn events_of_for_test(response: &Response) -> Vec<StreamEvent> {
    crate::provider::events_of(response)
}
