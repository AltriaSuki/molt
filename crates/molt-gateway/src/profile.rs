//! What each model accepts and what it costs.

use molt_api::model::{Effort, Usage};

/// US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Prices {
    pub input: f64,
    pub output: f64,
    /// Five-minute cache writes.
    pub cache_write: f64,
    pub cache_read: f64,
}

impl Prices {
    pub fn cost(&self, usage: &Usage) -> f64 {
        let dollars = usage.input_tokens as f64 * self.input
            + usage.output_tokens as f64 * self.output
            + usage.cache_creation_input_tokens as f64 * self.cache_write
            + usage.cache_read_input_tokens as f64 * self.cache_read;
        dollars / 1_000_000.0
    }
}

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
    pub prices: Option<Prices>,
    /// Largest `max_tokens` the model takes.
    pub max_output: Option<u32>,
}

const OPUS: Profile = Profile {
    adaptive_thinking: true,
    effort: true,
    // The API's own default here is medium too, but sending it keeps the request explicit.
    default_effort: Some(Effort::Medium),
    fallbacks: true,
    prices: Some(Prices { input: 4.0, output: 20.0, cache_write: 5.0, cache_read: 0.20 }),
    max_output: Some(128_000),
};

const SONNET: Profile = Profile {
    adaptive_thinking: true,
    effort: true,
    default_effort: Some(Effort::High),
    fallbacks: true,
    prices: Some(Prices { input: 2.0, output: 10.0, cache_write: 2.50, cache_read: 0.20 }),
    max_output: Some(128_000),
};

const HAIKU: Profile = Profile {
    adaptive_thinking: false,
    effort: false,
    default_effort: None,
    fallbacks: false,
    prices: Some(Prices { input: 1.0, output: 5.0, cache_write: 1.25, cache_read: 0.10 }),
    max_output: Some(64_000),
};

/// A model the gateway knows nothing about: send only what the caller asked for.
const OTHER: Profile = Profile {
    adaptive_thinking: false,
    effort: true,
    default_effort: None,
    fallbacks: false,
    prices: None,
    max_output: None,
};

/// The profile for a full model id; a dated snapshot id gets its base model's.
pub(crate) fn profile(model: &str) -> Profile {
    match base_id(model) {
        "claude-opus-5-5" => OPUS,
        "claude-sonnet-5-5" => SONNET,
        "claude-haiku-4-5" => HAIKU,
        _ => OTHER,
    }
}

/// `model` without a trailing `-YYYYMMDD` snapshot date.
fn base_id(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
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

    #[test]
    fn cost_counts_every_kind_of_token() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
        };
        let opus = OPUS.prices.unwrap();
        assert!((opus.cost(&usage) - (4.0 + 20.0 + 5.0 + 0.20)).abs() < 1e-9);
        let haiku = HAIKU.prices.unwrap();
        assert!((haiku.cost(&Usage { output_tokens: 200_000, ..Default::default() }) - 1.0).abs() < 1e-9);
    }
}
