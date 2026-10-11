//! What each model accepts. What it costs is in [`crate::prices`].

use molt_api::model::Effort;

use crate::prices::base_id;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Profile {
    /// Send `thinking: {type: "adaptive"}`.
    pub adaptive_thinking: bool,
    /// Accepts `output_config.effort`.
    pub effort: bool,
    /// Effort sent when neither the request nor the config sets one.
    pub default_effort: Option<Effort>,
    /// Accepts `fallbacks: "default"`.
    pub fallbacks: bool,
    /// Largest `max_tokens` the model takes.
    pub max_output: Option<u32>,
}

const OPUS: Profile = Profile {
    adaptive_thinking: true,
    effort: true,
    // The API's own default here is medium too, but sending it keeps the request explicit.
    default_effort: Some(Effort::Medium),
    fallbacks: true,
    max_output: Some(128_000),
};

const SONNET: Profile = Profile {
    adaptive_thinking: true,
    effort: true,
    default_effort: Some(Effort::High),
    fallbacks: true,
    max_output: Some(128_000),
};

const HAIKU: Profile = Profile {
    adaptive_thinking: false,
    effort: false,
    default_effort: None,
    fallbacks: false,
    max_output: Some(64_000),
};

/// A model the gateway knows nothing about: send only what the caller asked for.
const OTHER: Profile =
    Profile { adaptive_thinking: false, effort: true, default_effort: None, fallbacks: false, max_output: None };

/// The profile for a full model id; a dated snapshot id gets its base model's.
pub(crate) fn profile(model: &str) -> Profile {
    match base_id(model) {
        "claude-opus-5-5" => OPUS,
        "claude-sonnet-5-5" => SONNET,
        "claude-haiku-4-5" => HAIKU,
        _ => OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dated_ids_match_their_base_and_nothing_else_does() {
        assert_eq!(profile("claude-haiku-4-5-20251001"), HAIKU);
        assert_eq!(profile("claude-opus-5-5"), OPUS);
        assert_eq!(profile("claude-opus-5-5-20260101"), OPUS);
        assert_eq!(profile("claude-opus-5"), OTHER);
        assert_eq!(profile("claude-opus-5-5-fast"), OTHER);
        assert_eq!(profile("claude-opus-5-5-2026"), OTHER);
        assert_eq!(profile("claude-sonnet-5-5"), SONNET);
        assert_eq!(profile(""), OTHER);
    }
}
