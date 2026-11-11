//! Typed, explained errors with stable machine-readable codes (ADR-26).
//!
//! Providers map their wire errors onto [`LlmError`]; the caller's retry policy acts on
//! [`LlmError::retryable`] and [`LlmError::retry_after`] instead of parsing prose.

use std::fmt;
use std::time::Duration;

use serde::de::{Error as DeError, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Every failure a provider can hand back, with a stable [`LlmError::code`] and a
/// [`LlmError::retryable`] flag.
///
/// The codes are the contract:
/// `llm.auth`, `llm.rate_limited`, `llm.overloaded`, `llm.context_too_long`, `llm.bad_request`,
/// `llm.timeout`, `llm.unavailable`, `llm.content_filtered`, `llm.budget_exceeded`,
/// `llm.protocol`. Error text never contains a secret or the request body (ADR-26); variants
/// with a `detail` carry a producer-written explanation obeying the same rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmError {
    /// The key was missing, refused or lacks access (`llm.auth`).
    Auth,
    /// The provider rate limited the request (`llm.rate_limited`); retryable.
    RateLimited {
        /// Seconds to wait before retrying, when the provider said so.
        retry_after: Option<u64>,
    },
    /// The provider is momentarily overloaded (`llm.overloaded`); retryable.
    Overloaded,
    /// The conversation does not fit the model's context window (`llm.context_too_long`).
    ContextTooLong,
    /// The request was rejected as malformed (`llm.bad_request`).
    BadRequest {
        /// What was malformed. Must never quote the request body.
        detail: String,
    },
    /// The exchange did not finish inside the per-request timeout (`llm.timeout`); retryable.
    Timeout,
    /// The endpoint could not be reached (`llm.unavailable`); retryable.
    Unavailable,
    /// The provider refused the content (`llm.content_filtered`).
    ContentFiltered,
    /// Raised only by the router, never by a provider (`llm.budget_exceeded`).
    BudgetExceeded,
    /// The provider answered nonsense against its own protocol (`llm.protocol`).
    Protocol {
        /// Why the answer is nonsense. Must never quote the request body.
        detail: String,
    },
}

impl LlmError {
    /// All codes, for the deserializer's unknown-variant message.
    const CODES: &'static [&'static str] = &[
        "llm.auth",
        "llm.rate_limited",
        "llm.overloaded",
        "llm.context_too_long",
        "llm.bad_request",
        "llm.timeout",
        "llm.unavailable",
        "llm.content_filtered",
        "llm.budget_exceeded",
        "llm.protocol",
    ];

    /// The dotted machine-readable code (`llm.*`), stable across providers.
    pub fn code(&self) -> &'static str {
        match self {
            LlmError::Auth => "llm.auth",
            LlmError::RateLimited { .. } => "llm.rate_limited",
            LlmError::Overloaded => "llm.overloaded",
            LlmError::ContextTooLong => "llm.context_too_long",
            LlmError::BadRequest { .. } => "llm.bad_request",
            LlmError::Timeout => "llm.timeout",
            LlmError::Unavailable => "llm.unavailable",
            LlmError::ContentFiltered => "llm.content_filtered",
            LlmError::BudgetExceeded => "llm.budget_exceeded",
            LlmError::Protocol { .. } => "llm.protocol",
        }
    }

    /// Whether the caller should retry the same request later.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            LlmError::RateLimited { .. }
                | LlmError::Overloaded
                | LlmError::Timeout
                | LlmError::Unavailable
        )
    }

    /// The delay the provider asked for, on `llm.rate_limited` only.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            LlmError::RateLimited { retry_after } => retry_after.map(Duration::from_secs),
            _ => None,
        }
    }

    /// Rebuilds a unit-shaped error from its code, for fixtures and recorded exchanges.
    /// Returns `None` for a code outside the contract.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "llm.auth" => Some(LlmError::Auth),
            "llm.rate_limited" => Some(LlmError::RateLimited { retry_after: None }),
            "llm.overloaded" => Some(LlmError::Overloaded),
            "llm.context_too_long" => Some(LlmError::ContextTooLong),
            "llm.bad_request" => Some(LlmError::BadRequest {
                detail: String::new(),
            }),
            "llm.timeout" => Some(LlmError::Timeout),
            "llm.unavailable" => Some(LlmError::Unavailable),
            "llm.content_filtered" => Some(LlmError::ContentFiltered),
            "llm.budget_exceeded" => Some(LlmError::BudgetExceeded),
            "llm.protocol" => Some(LlmError::Protocol {
                detail: String::new(),
            }),
            _ => None,
        }
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())?;
        match self {
            LlmError::RateLimited {
                retry_after: Some(secs),
            } => {
                write!(f, ": rate limited; retry after {secs}s")
            }
            LlmError::RateLimited { retry_after: None } => write!(f, ": rate limited"),
            LlmError::BadRequest { detail } | LlmError::Protocol { detail } => {
                write!(f, ": {detail}")
            }
            LlmError::Auth => write!(f, ": the key was refused"),
            LlmError::Overloaded => write!(f, ": the provider is overloaded"),
            LlmError::ContextTooLong => {
                write!(f, ": the conversation does not fit the context window")
            }
            LlmError::Timeout => write!(f, ": the request did not finish in time"),
            LlmError::Unavailable => write!(f, ": the endpoint is unreachable"),
            LlmError::ContentFiltered => write!(f, ": the provider filtered the content"),
            LlmError::BudgetExceeded => write!(f, ": the model budget is exhausted"),
        }
    }
}

impl std::error::Error for LlmError {}

impl Serialize for LlmError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let extra = matches!(
            self,
            LlmError::RateLimited {
                retry_after: Some(_)
            } | LlmError::BadRequest { .. }
                | LlmError::Protocol { .. }
        ) as usize;
        let mut out = serializer.serialize_struct("LlmError", 1 + extra)?;
        out.serialize_field("code", self.code())?;
        match self {
            LlmError::RateLimited {
                retry_after: Some(secs),
            } => out.serialize_field("retry_after", secs)?,
            LlmError::BadRequest { detail } => out.serialize_field("detail", detail)?,
            LlmError::Protocol { detail } => out.serialize_field("detail", detail)?,
            _ => {}
        }
        out.end()
    }
}

/// Reads the fixed wire shape: `{"code": "llm.*", "retry_after"?: u64, "detail"?: String}`.
impl<'de> Deserialize<'de> for LlmError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LlmErrorVisitor;

        impl<'de> Visitor<'de> for LlmErrorVisitor {
            type Value = LlmError;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an LlmError object with a `code` field")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LlmError, A::Error> {
                let mut code: Option<String> = None;
                let mut retry_after: Option<u64> = None;
                let mut detail: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "code" => code = Some(map.next_value()?),
                        "retry_after" => retry_after = Some(map.next_value()?),
                        "detail" => detail = Some(map.next_value()?),
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                let code = code
                    .as_deref()
                    .ok_or_else(|| DeError::missing_field("code"))?;
                match code {
                    "llm.auth" => Ok(LlmError::Auth),
                    "llm.rate_limited" => Ok(LlmError::RateLimited { retry_after }),
                    "llm.overloaded" => Ok(LlmError::Overloaded),
                    "llm.context_too_long" => Ok(LlmError::ContextTooLong),
                    "llm.bad_request" => Ok(LlmError::BadRequest {
                        detail: detail.ok_or_else(|| DeError::missing_field("detail"))?,
                    }),
                    "llm.timeout" => Ok(LlmError::Timeout),
                    "llm.unavailable" => Ok(LlmError::Unavailable),
                    "llm.content_filtered" => Ok(LlmError::ContentFiltered),
                    "llm.budget_exceeded" => Ok(LlmError::BudgetExceeded),
                    "llm.protocol" => Ok(LlmError::Protocol {
                        detail: detail.ok_or_else(|| DeError::missing_field("detail"))?,
                    }),
                    other => Err(DeError::unknown_variant(other, LlmError::CODES)),
                }
            }
        }
        deserializer.deserialize_map(LlmErrorVisitor)
    }
}
