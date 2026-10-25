//! The models a gateway started with some environment knows, for
//! `molt models show`: where a model's calls go, what the gateway knows it
//! does, what a probe found it does, and what it costs. Reading it calls
//! nothing.

use molt_proto::RemoteError;

use crate::prices::{PriceSource, PriceTable, Prices};
use crate::probe::{Probed, Records};
use crate::provider::{Provider, Providers};

/// Everything known about models in one environment.
pub struct Catalog {
    pub default_model: String,
    /// Alias to what it names: the built-in ones, then `MOLT_MODEL_ALIASES`.
    pub aliases: Vec<(String, String)>,
    /// OpenAI models declared in `MOLT_OPENAI_MODELS`.
    pub declared: Vec<String>,
    pub probed: Records,
    providers: Providers,
    anthropic_base_url: String,
    prices: PriceTable,
}

/// One model, as [`Catalog::entry`] finds it.
pub struct Entry {
    pub provider: Provider,
    pub model: String,
    pub base_url: String,
    /// What the gateway itself knows the model does, in words.
    pub known: String,
    /// What a probe on this endpoint found, if one did.
    pub probed: Option<Probed>,
    pub price: Option<(Prices, PriceSource)>,
}

impl Catalog {
    /// The catalog of a gateway started with these variables. Unlike the
    /// gateway, it needs no credential.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let providers = Providers::from_vars(&get)?;
        let mut aliases: Vec<(String, String)> =
            ["opus", "sonnet", "haiku"].iter().map(|a| ((*a).to_owned(), crate::resolve_model(a))).collect();
        aliases.extend(providers.aliases.iter().map(|(a, t)| (a.clone(), t.clone())));
        Ok(Self {
            default_model: crate::default_model(&get),
            aliases,
            declared: providers.openai_models.keys().cloned().collect(),
            probed: Records::from_vars(get)?,
            anthropic_base_url: get("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|| crate::DEFAULT_BASE_URL.into())
                .trim_end_matches('/')
                .to_owned(),
            providers,
            prices: PriceTable::from_vars(&var)?,
        })
    }

    /// Where calls naming `named` go and what is known of that model.
    pub fn entry(&self, named: &str) -> Result<Entry, RemoteError> {
        let (provider, model) = self.providers.resolve(named)?;
        let base_url = match provider {
            Provider::Anthropic => self.anthropic_base_url.clone(),
            Provider::Openai => self.providers.openai_base_url.clone(),
        };
        let known = match provider {
            Provider::Anthropic => known_anthropic(&model),
            Provider::Openai => match self.providers.capabilities(&model) {
                Ok(c) => {
                    let declared =
                        if self.providers.openai_models.contains_key(&model) { "declared" } else { "built in" };
                    let efforts: Vec<String> =
                        c.efforts.iter().map(|e| serde_json::to_value(e).unwrap_or_default().to_string()).collect();
                    format!(
                        "{declared}: tools {}, streaming {}, output_schema {}, efforts [{}], at most {} output tokens",
                        yes(c.tools),
                        yes(c.streaming),
                        yes(c.output_schema),
                        efforts.join(", ").replace('"', ""),
                        c.max_output
                    )
                }
                Err(e) => e.message,
            },
        };
        let probed = self.probed.lookup(provider, &base_url, &model).cloned();
        let price = self.prices.lookup(&model);
        Ok(Entry { provider, model, base_url, known, probed, price })
    }
}

fn known_anthropic(model: &str) -> String {
    let p = crate::profile::profile(model);
    if p == crate::profile::profile("") {
        return "unknown to the gateway: a call gets only what it asks for (no thinking, no default effort)".into();
    }
    let effort = match (p.effort, p.default_effort) {
        (false, _) => "no effort".to_owned(),
        (true, Some(e)) => {
            format!("effort (default {})", serde_json::to_value(e).unwrap_or_default().as_str().unwrap_or(""))
        }
        (true, None) => "effort".to_owned(),
    };
    let mut parts = vec![effort];
    if p.adaptive_thinking {
        parts.push("adaptive thinking".into());
    }
    if p.fallbacks {
        parts.push("server-side fallbacks".into());
    }
    if let Some(max) = p.max_output {
        parts.push(format!("at most {max} output tokens"));
    }
    format!("built in: {}", parts.join(", "))
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// A probe's finding, in a word.
pub fn finding(found: Option<bool>) -> &'static str {
    match found {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_says_where_calls_go_what_is_known_and_what_a_probe_found() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("models.json");
        let mut records = Records::default();
        records.put(Probed {
            provider: Provider::Anthropic,
            base_url: "https://glm.example/api".into(),
            model: "glm-4.6".into(),
            probed_at_ms: 1,
            tools: Some(true),
            streaming: Some(true),
            output_schema: Some(false),
            effort: None,
            cost_usd: Some(0.001),
        });
        records.write(&file).unwrap();
        let path = file.display().to_string();
        let var = |name: &str| match name {
            "ANTHROPIC_BASE_URL" => Some("https://glm.example/api/".to_owned()),
            "MOLT_MODELS_FILE" => Some(path.clone()),
            "MOLT_MODEL_ALIASES" => Some(r#"{"fast":"openai/gpt-4.1-mini"}"#.to_owned()),
            "MOLT_MODEL_PRICES" => Some("glm-4.6=0.6,2.2".to_owned()),
            _ => None,
        };
        let catalog = Catalog::from_vars(var).unwrap();
        assert_eq!(catalog.default_model, "opus");
        assert!(catalog.aliases.contains(&("fast".into(), "openai/gpt-4.1-mini".into())));
        let glm = catalog.entry("glm-4.6").unwrap();
        assert_eq!((glm.provider, glm.base_url.as_str()), (Provider::Anthropic, "https://glm.example/api"));
        assert!(glm.known.starts_with("unknown to the gateway"), "{}", glm.known);
        assert_eq!(glm.probed.unwrap().output_schema, Some(false));
        assert_eq!(glm.price.unwrap().0.input, 0.6);
        let haiku = catalog.entry("haiku").unwrap();
        assert_eq!(haiku.model, "claude-haiku-4-5");
        assert!(haiku.known.contains("no effort") && haiku.probed.is_none(), "{}", haiku.known);
        let fast = catalog.entry("fast").unwrap();
        assert_eq!((fast.provider, fast.model.as_str()), (Provider::Openai, "gpt-4.1-mini"));
        assert!(fast.known.starts_with("built in: tools yes"), "{}", fast.known);
        assert!(catalog.entry("openai/mystery").unwrap().known.contains("MOLT_OPENAI_MODELS"));
    }
}
