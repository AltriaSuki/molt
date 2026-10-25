//! Explicit provider routing and model capabilities. Unknown OpenAI models
//! need a trusted declaration; an endpoint URL does not declare capabilities.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{ensure, Context};
use molt_api::model::Effort;
use molt_proto::{ErrorCode, RemoteError};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Anthropic,
    Openai,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        })
    }
}

/// The wire protocol an OpenAI model is called with. It comes only from a
/// trusted declaration; an endpoint's errors never switch it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// `POST /v1/chat/completions`.
    #[default]
    ChatCompletions,
    /// `POST /v1/responses`, always with `store: false` (see `responses.rs`).
    Responses,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub tools: bool,
    pub streaming: bool,
    pub output_schema: bool,
    #[serde(default)]
    pub efforts: Vec<Effort>,
    pub max_output: u32,
    #[serde(default)]
    pub protocol: Protocol,
    /// The context window in tokens, the reply included, which the planner
    /// is told so it fits conversations to it. `None`: not declared.
    #[serde(default)]
    pub context: Option<u32>,
}

#[derive(Clone)]
pub struct Providers {
    pub default: Provider,
    pub openai_key: Option<String>,
    /// API origin, with no /v1 suffix, like ANTHROPIC_BASE_URL.
    pub openai_base_url: String,
    pub aliases: BTreeMap<String, String>,
    pub openai_models: BTreeMap<String, Capabilities>,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            default: Provider::Anthropic,
            openai_key: None,
            openai_base_url: "https://api.openai.com".into(),
            aliases: BTreeMap::new(),
            openai_models: BTreeMap::new(),
        }
    }
}

impl fmt::Debug for Providers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Providers")
            .field("default", &self.default)
            .field("openai_key", &self.openai_key.as_ref().map(|_| "<redacted>"))
            .field("openai_base_url", &self.openai_base_url)
            .field("aliases", &self.aliases)
            .field("openai_models", &self.openai_models)
            .finish()
    }
}

impl Providers {
    pub(crate) fn from_vars(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut p = Self::default();
        if let Some(name) = get("MOLT_PROVIDER") {
            p.default =
                serde_json::from_value(serde_json::json!(name)).context("MOLT_PROVIDER: use anthropic or openai")?;
        }
        p.openai_key = get("OPENAI_API_KEY");
        if let Some(url) = get("OPENAI_BASE_URL") {
            p.openai_base_url = url.trim_end_matches('/').into();
        }
        if let Some(aliases) = get("MOLT_MODEL_ALIASES") {
            p.aliases =
                serde_json::from_str(&aliases).context("MOLT_MODEL_ALIASES must map aliases to provider/model")?;
            for (alias, target) in &p.aliases {
                ensure!(!alias.is_empty() && !alias.contains('/'), "model alias must be a nonempty plain name");
                ensure!(
                    target
                        .strip_prefix("anthropic/")
                        .or_else(|| target.strip_prefix("openai/"))
                        .is_some_and(|id| !id.trim().is_empty()),
                    "alias {alias} must name anthropic/model or openai/model"
                );
            }
        }
        if let Some(models) = get("MOLT_OPENAI_MODELS") {
            p.openai_models = serde_json::from_str(&models).context("MOLT_OPENAI_MODELS has invalid capabilities")?;
            ensure!(
                p.openai_models.iter().all(|(id, c)| !id.trim().is_empty() && c.max_output > 0),
                "OpenAI model declarations need an id and positive max_output"
            );
            ensure!(
                p.openai_models.values().all(|c| c.context.is_none_or(|n| n > c.max_output)),
                "an OpenAI model's context must be larger than its max_output"
            );
        }
        Ok(p)
    }

    pub(crate) fn resolve(&self, named: &str) -> Result<(Provider, String), RemoteError> {
        let target = self.aliases.get(named).map(String::as_str).unwrap_or(named);
        let (provider, id) = if let Some(id) = target.strip_prefix("openai/") {
            (Provider::Openai, id)
        } else if let Some(id) = target.strip_prefix("anthropic/") {
            (Provider::Anthropic, id)
        } else {
            (self.default, target)
        };
        if id.trim().is_empty() {
            return Err(invalid("model id is empty"));
        }
        let id = if provider == Provider::Anthropic { crate::resolve_model(id) } else { id.to_owned() };
        Ok((provider, id))
    }

    pub(crate) fn capabilities(&self, model: &str) -> Result<Capabilities, RemoteError> {
        if let Some(c) = self.openai_models.get(model) {
            return Ok(c.clone());
        }
        let family = model.strip_suffix("-2025-04-14").unwrap_or(model);
        if matches!(family, "gpt-4.1" | "gpt-4.1-mini" | "gpt-4.1-nano") {
            return Ok(Capabilities {
                tools: true,
                streaming: true,
                output_schema: true,
                efforts: Vec::new(),
                max_output: 32_768,
                protocol: Protocol::ChatCompletions,
                context: Some(1_047_576),
            });
        }
        Err(invalid("unknown OpenAI model capabilities: declare it in MOLT_OPENAI_MODELS"))
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError { code: ErrorCode::Invalid, message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_aliases_capabilities_and_provider_names_are_refused() {
        for (name, value) in [
            ("MOLT_PROVIDER", "anything"),
            ("MOLT_MODEL_ALIASES", r#"{"a":"gpt-4.1"}"#),
            ("MOLT_MODEL_ALIASES", r#"{"a":"openai/"}"#),
            ("MOLT_OPENAI_MODELS", r#"{"x":{"tools":true,"streaming":true,"output_schema":true,"max_output":0}}"#),
            (
                "MOLT_OPENAI_MODELS",
                r#"{"x":{"tools":true,"streaming":true,"output_schema":true,"max_output":1,"typo":true}}"#,
            ),
            (
                "MOLT_OPENAI_MODELS",
                r#"{"x":{"tools":true,"streaming":true,"output_schema":true,"max_output":1,"protocol":"realtime"}}"#,
            ),
            (
                "MOLT_OPENAI_MODELS",
                r#"{"x":{"tools":true,"streaming":true,"output_schema":true,"max_output":10,"context":10}}"#,
            ),
        ] {
            assert!(Providers::from_vars(&|n| (n == name).then(|| value.into())).is_err(), "{name}: {value}");
        }
    }

    #[test]
    fn the_protocol_is_chat_completions_unless_a_declaration_names_responses() {
        let models = r#"{"r":{"tools":true,"streaming":true,"output_schema":true,"max_output":8,"protocol":"responses","context":400000},
            "c":{"tools":true,"streaming":true,"output_schema":true,"max_output":8}}"#;
        let p = Providers::from_vars(&|n| (n == "MOLT_OPENAI_MODELS").then(|| models.into())).unwrap();
        let r = p.capabilities("r").unwrap();
        assert_eq!((r.protocol, r.context), (Protocol::Responses, Some(400_000)));
        assert_eq!(p.capabilities("c").unwrap().protocol, Protocol::ChatCompletions);
        assert_eq!(p.capabilities("gpt-4.1").unwrap().protocol, Protocol::ChatCompletions);
    }
}
