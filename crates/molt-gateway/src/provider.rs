//! Explicit provider routing and model capabilities. Unknown OpenAI models
//! need a trusted declaration; an endpoint URL does not declare capabilities.
//!
//! Besides the built-in Anthropic and OpenAI endpoints (`ANTHROPIC_BASE_URL`,
//! `OPENAI_BASE_URL`), the user may name endpoints of their own in
//! [`ENDPOINTS_VAR`] (`[models.endpoints]` of the user config): each with its
//! protocol, base URL, the variable that holds its key, and the models it
//! serves. A call reaches one as `NAME/MODEL`.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{ensure, Context};
use molt_api::model::Effort;
use molt_proto::{ErrorCode, RemoteError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

/// The user's named endpoints, as JSON: endpoint name to [`Endpoint`].
pub const ENDPOINTS_VAR: &str = "MOLT_ENDPOINTS";

/// The protocol a named endpoint speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointProtocol {
    /// Anthropic Messages compatible: `POST {base_url}/v1/messages`.
    Anthropic,
    /// OpenAI Chat Completions compatible: `POST {base_url}/v1/chat/completions`.
    ChatCompletions,
    /// OpenAI Responses: `POST {base_url}/v1/responses`, with `store: false`.
    Responses,
}

impl EndpointProtocol {
    pub fn provider(self) -> Provider {
        match self {
            Self::Anthropic => Provider::Anthropic,
            Self::ChatCompletions | Self::Responses => Provider::Openai,
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
}

/// How a named endpoint is given its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// `x-api-key: KEY`, as the Anthropic API takes it. Anthropic protocol only.
    XApiKey,
    /// `Authorization: Bearer KEY`.
    Bearer,
}

/// An endpoint the user names, with the models it serves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub protocol: EndpointProtocol,
    /// The API origin, with no `/v1`: molt adds the protocol's path.
    pub base_url: String,
    /// The environment variable that holds the key. Only the gateway is
    /// given it; the config holds its name, never the key.
    pub key_env: String,
    /// Default: `x-api-key` for the Anthropic protocol, `bearer` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// The model ids it serves, as sent; no other model is called there.
    pub models: BTreeMap<String, EndpointModel>,
}

/// What a model on a named endpoint does, in the user's word. A probe can
/// still find a feature declared here missing; it never turns one on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointModel {
    pub tools: bool,
    pub streaming: bool,
    pub output_schema: bool,
    /// The efforts it takes; none: a request with an effort is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<Effort>,
    pub max_output: u32,
    /// The context window in tokens, the reply included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Without one, the price table is looked up by the model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<EndpointPrice>,
}

/// US dollars per million tokens. A cache price left out errs high, as in
/// `MOLT_MODEL_PRICES`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointPrice {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

impl Endpoint {
    /// The origin, without a trailing slash.
    pub fn origin(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    pub fn auth(&self) -> Auth {
        self.auth.unwrap_or(match self.protocol {
            EndpointProtocol::Anthropic => Auth::XApiKey,
            _ => Auth::Bearer,
        })
    }

    /// What is wrong with endpoint `name`, saying what to change.
    pub fn check(&self, name: &str) -> Result<(), String> {
        let at = |what: String| format!("endpoint {name}: {what}");
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return Err(format!(
                "endpoint name {name:?} must be 1 to 64 lowercase letters, digits, - or _, as calls name it NAME/MODEL"
            ));
        }
        if name == "anthropic" || name == "openai" {
            return Err(at("anthropic and openai name the built-in endpoints; choose another name".into()));
        }
        let origin = self.origin();
        let url: reqwest::Url = format!("{origin}{}", self.protocol.path())
            .parse()
            .map_err(|e| at(format!("base_url {:?} is not a URL: {e}", self.base_url)))?;
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return Err(at("base_url cannot hold credentials, a query or a fragment; name the key in key_env".into()));
        }
        if url.scheme() != "https" && !(url.scheme() == "http" && crate::api::is_loopback(&url)) {
            return Err(at(format!("base_url {origin:?} must be https (http only on loopback): the key goes with it")));
        }
        if crate::api::ends_in_v1(origin) {
            return Err(at(format!(
                "base_url {origin:?} ends in /v1, and molt adds {} itself: remove the trailing /v1",
                self.protocol.path()
            )));
        }
        let key = &self.key_env;
        if key.is_empty()
            || key.as_bytes()[0].is_ascii_digit()
            || !key.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(at(format!("key_env {key:?} must be a variable name such as GLM_API_KEY, not the key")));
        }
        if key.starts_with("MOLT_") || ["HOME", "PATH", "TMPDIR", "USER", "SHELL"].contains(&key.as_str()) {
            return Err(at(format!("key_env {key} is not a variable for a key: Molt's settings and {key} are not")));
        }
        if self.auth() == Auth::XApiKey && self.protocol != EndpointProtocol::Anthropic {
            return Err(at("auth x-api-key is for the anthropic protocol; OpenAI protocols take bearer".into()));
        }
        if self.models.is_empty() {
            return Err(at("declare the models it serves under models".into()));
        }
        for (id, m) in &self.models {
            let at = |what: &str| at(format!("model {id:?}: {what}"));
            if id.trim().is_empty() || id.trim() != id {
                return Err(at("the id is empty or has spaces around it"));
            }
            if m.max_output == 0 {
                return Err(at("max_output must be positive"));
            }
            if m.context.is_some_and(|n| n <= m.max_output) {
                return Err(at("context must be larger than max_output"));
            }
            if self.protocol != EndpointProtocol::Anthropic && m.efforts.contains(&Effort::Max) {
                return Err(at("effort max is not an OpenAI effort"));
            }
            if let Some(p) = &m.price {
                let all = [Some(p.input), Some(p.output), p.cache_write, p.cache_read];
                if !all.into_iter().flatten().all(|v| v.is_finite() && v >= 0.0) {
                    return Err(at("prices must be finite and not negative"));
                }
            }
        }
        Ok(())
    }
}

/// Parse and check the named endpoints of [`ENDPOINTS_VAR`].
pub fn parse_endpoints(text: &str) -> Result<BTreeMap<String, Endpoint>, String> {
    let endpoints: BTreeMap<String, Endpoint> =
        serde_json::from_str(text).map_err(|e| format!("{ENDPOINTS_VAR} is not a table of endpoints: {e}"))?;
    for (name, endpoint) in &endpoints {
        endpoint.check(name)?;
    }
    Ok(endpoints)
}

/// Where one call goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub provider: Provider,
    /// The named endpoint; `None` for the built-in one of `provider`.
    pub endpoint: Option<String>,
    /// The model id as sent, aliases resolved.
    pub model: String,
}

impl Route {
    /// How a call names this model and nothing else: `glm/glm-4.6`,
    /// `anthropic/claude-opus-5-5`.
    pub fn qualified(&self) -> String {
        match &self.endpoint {
            Some(name) => format!("{name}/{}", self.model),
            None => format!("{}/{}", self.provider, self.model),
        }
    }

    /// Where its calls go, in words: `endpoint glm` or `anthropic`.
    pub fn place(&self) -> String {
        match &self.endpoint {
            Some(name) => format!("endpoint {name}"),
            None => self.provider.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct Providers {
    pub default: Provider,
    pub openai_key: Option<String>,
    /// API origin, with no /v1 suffix, like ANTHROPIC_BASE_URL.
    pub openai_base_url: String,
    pub aliases: BTreeMap<String, String>,
    pub openai_models: BTreeMap<String, Capabilities>,
    /// The user's named endpoints, by name.
    pub endpoints: BTreeMap<String, Endpoint>,
    /// Each named endpoint's key, from its `key_env`, held only here.
    pub endpoint_keys: BTreeMap<String, String>,
}

impl Default for Providers {
    fn default() -> Self {
        Self {
            default: Provider::Anthropic,
            openai_key: None,
            openai_base_url: "https://api.openai.com".into(),
            aliases: BTreeMap::new(),
            openai_models: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            endpoint_keys: BTreeMap::new(),
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
            .field("endpoints", &self.endpoints)
            .field("endpoint_keys", &self.endpoint_keys.keys().map(|name| (name, "<redacted>")).collect::<Vec<_>>())
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
        if let Some(text) = get(ENDPOINTS_VAR) {
            p.endpoints = parse_endpoints(&text).map_err(anyhow::Error::msg)?;
            for (name, endpoint) in &p.endpoints {
                if let Some(key) = get(&endpoint.key_env) {
                    p.endpoint_keys.insert(name.clone(), key);
                }
            }
        }
        if let Some(aliases) = get("MOLT_MODEL_ALIASES") {
            p.aliases = serde_json::from_str(&aliases)
                .context("MOLT_MODEL_ALIASES must map aliases to provider/model or endpoint/model")?;
            for (alias, target) in &p.aliases {
                ensure!(!alias.is_empty() && !alias.contains('/'), "model alias must be a nonempty plain name");
                let named = target.split_once('/').filter(|(place, id)| {
                    !id.trim().is_empty()
                        && (*place == "anthropic" || *place == "openai" || p.endpoints.contains_key(*place))
                });
                ensure!(
                    named.is_some(),
                    "alias {alias} must name anthropic/model, openai/model or endpoint/model for an endpoint of \
                     {ENDPOINTS_VAR}"
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

    /// Where a call naming `named` (an alias, `provider/model`,
    /// `endpoint/model` or a model id) goes. A named endpoint serves only
    /// the models declared for it.
    pub fn resolve(&self, named: &str) -> Result<Route, RemoteError> {
        let target = self.aliases.get(named).map(String::as_str).unwrap_or(named);
        if let Some((name, id)) = target.split_once('/').filter(|(name, _)| self.endpoints.contains_key(*name)) {
            let endpoint = &self.endpoints[name];
            if !endpoint.models.contains_key(id) {
                let served: Vec<&str> = endpoint.models.keys().map(String::as_str).collect();
                return Err(invalid(format!(
                    "endpoint {name} serves no model {id:?}: it serves {}; declare others under its models",
                    served.join(", ")
                )));
            }
            return Ok(Route {
                provider: endpoint.protocol.provider(),
                endpoint: Some(name.to_owned()),
                model: id.to_owned(),
            });
        }
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
        Ok(Route { provider, endpoint: None, model: id })
    }

    /// What the user declared of the model `route` goes to on a named endpoint.
    pub fn declared(&self, route: &Route) -> Option<&EndpointModel> {
        self.endpoints.get(route.endpoint.as_deref()?)?.models.get(&route.model)
    }

    /// The origin `route`'s calls go to; `anthropic` is `ANTHROPIC_BASE_URL`'s.
    pub fn base_url<'a>(&'a self, route: &Route, anthropic: &'a str) -> &'a str {
        match (&route.endpoint, route.provider) {
            (Some(name), _) => self.endpoints.get(name).map_or("", Endpoint::origin),
            (None, Provider::Anthropic) => anthropic,
            (None, Provider::Openai) => &self.openai_base_url,
        }
    }

    /// What an OpenAI-protocol model of `route` does: its declaration on a
    /// named endpoint, else [`Providers::capabilities`].
    pub(crate) fn openai_capabilities(&self, route: &Route) -> Result<Capabilities, RemoteError> {
        let Some(name) = &route.endpoint else { return self.capabilities(&route.model) };
        let declared = self.declared(route).ok_or_else(|| invalid(format!("endpoint {name} is not declared")))?;
        let protocol = match self.endpoints[name].protocol {
            EndpointProtocol::Responses => Protocol::Responses,
            _ => Protocol::ChatCompletions,
        };
        Ok(Capabilities {
            tools: declared.tools,
            streaming: declared.streaming,
            output_schema: declared.output_schema,
            efforts: declared.efforts.clone(),
            max_output: declared.max_output,
            protocol,
            context: declared.context,
        })
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
