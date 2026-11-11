//! The endpoint profile: what a compatible server supports, as plain data the caller owns.
//!
//! Servers speaking the OpenAI-compatible Chat Completions protocol differ: some need a bearer
//! key, some are local and trust their caller; some accept tool definitions, some do not; some
//! report usage in the final streaming chunk, some need asking; the token-cap field changed
//! name across OpenAI model generations; and small local models emit tool-call arguments as
//! partial JSON that has to be closed before it parses. A [`Profile`] states all of it — no
//! behaviour hidden behind a constructor, only data the caller can build, inspect and change.

/// How the provider authenticates against the endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthStyle {
    /// `authorization: Bearer <key>`. A call without a key fails fast with `llm.auth`.
    Bearer,
    /// No auth header at all — a local server that trusts its caller (Ollama). A key passed
    /// to the provider is ignored when the profile says `None`.
    None,
}

/// Which body field caps the generated tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxTokensField {
    /// `max_tokens` — the classic Chat Completions name.
    MaxTokens,
    /// `max_completion_tokens` — the newer name (OpenAI's reasoning-model generations).
    MaxCompletionTokens,
}

/// What a compatible endpoint supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// How to authenticate.
    pub auth: AuthStyle,
    /// Extra headers every request carries, in order (OpenRouter's `http-referer` and
    /// `x-title` are the canonical use — caller-supplied per app, so the constructors ship
    /// none).
    pub extra_headers: Vec<(String, String)>,
    /// The endpoint accepts tool definitions and answers with tool calls.
    pub supports_tools: bool,
    /// The streaming request asks for usage with `stream_options.include_usage`; usage
    /// present in a final chunk is read either way.
    pub supports_streaming_usage: bool,
    /// Which body field caps the generated tokens.
    pub max_tokens_field: MaxTokensField,
    /// The endpoint's models may emit tool-call arguments as partial JSON (a truncated or
    /// unclosed object) that the provider closes and parses. When `false`, a malformed
    /// argument string is `llm.protocol`.
    pub tool_args_may_be_partial_json: bool,
}

impl Profile {
    /// The OpenRouter profile: bearer key, streaming usage on, strict tool arguments. The
    /// OpenRouter-specific referer and title headers are caller-added data
    /// (`profile.extra_headers`), not defaults here.
    pub fn openrouter() -> Self {
        Self {
            auth: AuthStyle::Bearer,
            extra_headers: Vec::new(),
            supports_tools: true,
            supports_streaming_usage: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            tool_args_may_be_partial_json: false,
        }
    }

    /// The Ollama profile: no auth header, streaming usage on, and tool arguments parsed
    /// leniently — small local models routinely cut the arguments object short.
    pub fn ollama() -> Self {
        Self {
            auth: AuthStyle::None,
            extra_headers: Vec::new(),
            supports_tools: true,
            supports_streaming_usage: true,
            max_tokens_field: MaxTokensField::MaxTokens,
            tool_args_may_be_partial_json: true,
        }
    }

    /// The generic profile for any other compatible endpoint (OpenAI, vLLM, llama.cpp
    /// server, LM Studio): bearer key, tools on, streaming usage off (the request does not
    /// ask for it; usage present in a final chunk is still read), strict tool arguments.
    pub fn generic() -> Self {
        Self {
            auth: AuthStyle::Bearer,
            extra_headers: Vec::new(),
            supports_tools: true,
            supports_streaming_usage: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            tool_args_may_be_partial_json: false,
        }
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self::generic()
    }
}
