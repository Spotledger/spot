//! The opt-in prompt-caching markers.
//!
//! Anthropic caches prompt prefixes marked with `cache_control: {"type": "ephemeral"}`. This
//! crate marks exactly two places, both opt-in per request: the end of the system prompt block
//! (the system block is a prefix of every turn, so caching it pays first) and the last tool
//! definition (so the whole tool schema array is cached). See the public Messages API
//! documentation.

use serde::{Deserialize, Serialize};

/// Which prompt prefix to mark for caching. Opt-in per request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMarker {
    /// Nothing is marked.
    #[default]
    None,
    /// The system prompt block is marked `cache_control: {"type": "ephemeral"}`.
    System,
    /// The last tool definition is marked, caching the whole tool array.
    LastTool,
    /// Both markers.
    SystemAndLastTool,
}

impl CacheMarker {
    /// Whether the system prompt block should carry the marker.
    pub fn marks_system(self) -> bool {
        matches!(self, CacheMarker::System | CacheMarker::SystemAndLastTool)
    }

    /// Whether the last tool definition should carry the marker.
    pub fn marks_last_tool(self) -> bool {
        matches!(self, CacheMarker::LastTool | CacheMarker::SystemAndLastTool)
    }
}
