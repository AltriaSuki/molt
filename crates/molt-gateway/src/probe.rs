//! What a model endpoint was found to do, by `molt models probe`, and the
//! probe itself.
//!
//! A probe makes a few small, real model calls, so it runs only when a
//! person asks for it: never at startup or during a run. Its findings are
//! kept in the data dir ([`records_path`]) and read once when the gateway
//! starts; the gateway then shapes each call for what the endpoint does
//! (see [`Probed`]). A record holds for the endpoint it was made on: once
//! the base URL changes, it no longer applies.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use molt_api::model::{user_text, CompleteRequest, Effort, StreamContext};
use molt_proto::{ErrorCode, RemoteError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::provider::Provider;
use crate::{Config, Gateway};

/// Names a records file in place of the default one.
pub const FILE_VAR: &str = "MOLT_MODELS_FILE";
const FORMAT: u32 = 1;
/// The output cap of each probe call: enough for a short answer or one
/// tool call, so a probe costs little.
pub const MAX_TOKENS: u32 = 512;

/// One model on one endpoint, as a probe found it. `None` for a feature
/// the probe could not tell, or did not try; the gateway then does what it
/// did before.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Probed {
    pub provider: Provider,
    /// The origin the calls went to, without a trailing slash.
    pub base_url: String,
    /// The model id as sent, aliases resolved.
    pub model: String,
    pub probed_at_ms: u64,
    /// It answered a request for a tool call with one.
    pub tools: Option<bool>,
    /// It answered a streamed request with a stream.
    pub streaming: Option<bool>,
    /// Its answer to a request with `output_schema` matched the schema.
    pub output_schema: Option<bool>,
    /// It took a request that sets an effort.
    pub effort: Option<bool>,
    /// What the probe's calls cost, when every one of them is known.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// The named endpoint the calls went to; `None` for a built-in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The tokens the probe's calls spent reasoning, when every one of them
    /// said: unknown otherwise, never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Probed {
    fn is(&self, provider: Provider, base_url: &str, model: &str) -> bool {
        self.provider == provider && self.base_url == base_url.trim_end_matches('/') && self.model == model
    }
}

/// Every probe record, newest probe of a model on an endpoint only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Records {
    pub format: u32,
    pub models: Vec<Probed>,
}

impl Records {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let records: Self = serde_json::from_str(text).context("not a molt model probe file")?;
        ensure!(records.format == FORMAT, "model probe file format {} is not {FORMAT}", records.format);
        Ok(records)
    }

    /// The records from the file the environment names, or none when there
    /// is no file. A file that does not read is an error, not a fallback.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        match records_path(var) {
            Some(path) if path.exists() || var(FILE_VAR).is_some() => Self::load(&path),
            _ => Ok(Self::default()),
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| {
            format!("the model probe file {} is not usable: delete it, or probe the models again", path.display())
        })
    }

    /// The record of `model` on that endpoint, if it was probed there.
    pub fn lookup(&self, provider: Provider, base_url: &str, model: &str) -> Option<&Probed> {
        self.models.iter().find(|p| p.is(provider, base_url, model))
    }

    /// Keep `probed` in place of any earlier record of the same model and endpoint.
    pub fn put(&mut self, probed: Probed) {
        self.format = FORMAT;
        self.models.retain(|p| !p.is(probed.provider, &probed.base_url, &probed.model));
        self.models.push(probed);
        self.models.sort_by(|a, b| (&a.model, &a.base_url).cmp(&(&b.model, &b.base_url)));
    }

    /// Write the records to `path` all at once: a gateway starting meanwhile
    /// reads the old file or the new one, never part of either.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("writing in {}", dir.display()))?;
        tmp.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        tmp.write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

/// Where the probe records are: `MOLT_MODELS_FILE`, or `molt/models.json`
/// in `XDG_DATA_HOME` (`~/.local/share` by default), beside the prices.
/// The sandbox hides home directories from the commands an agent runs, so
/// they cannot change it.
pub fn records_path(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(file) = var(FILE_VAR).filter(|f| !f.is_empty()) {
        return Some(file.into());
    }
    let data = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or_else(|| var("HOME").filter(|h| !h.is_empty()).map(|h| Path::new(&h).join(".local/share")))?;
    Some(data.join("molt").join("models.json"))
}

/// The tool the gateway answers through for a model that ignores
/// `output_schema`, with the schema as its input.
pub const SCHEMA_TOOL: &str = "molt_output";

/// One call a probe makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    Tools,
    Streaming,
    OutputSchema,
    Effort(Effort),
}

impl Check {
    pub fn describe(self) -> String {
        match self {
            Self::Tools => "a tool call".into(),
            Self::Streaming => "a streamed reply".into(),
            Self::OutputSchema => "an answer in a JSON schema (output_schema)".into(),
            Self::Effort(e) => format!("a request with effort {}", serde_json::to_value(e).unwrap_or_default()),
        }
    }
}

/// What a probe of one model will call, worked out before anything is sent.
#[derive(Clone, Debug)]
pub struct Plan {
    pub named: String,
    pub provider: Provider,
    /// The user's endpoint it is on; `None` for the built-in one of `provider`.
    pub endpoint: Option<String>,
    pub model: String,
    pub base_url: String,
    /// The calls, in order.
    pub checks: Vec<Check>,
    /// What is not called, and why.
    pub skipped: Vec<String>,
}

/// What one check found.
#[derive(Clone, Debug)]
pub struct Finding {
    pub check: Check,
    pub works: Option<bool>,
    /// What was seen, in a few words.
    pub detail: String,
}

/// The outcome of a probe: the record to keep and what each check found.
#[derive(Clone, Debug)]
pub struct Report {
    pub probed: Probed,
    pub findings: Vec<Finding>,
    /// The calls made, of [`Plan::checks`]: fewer when one showed the
    /// endpoint or credential is wrong.
    pub calls: usize,
    /// Why the probe stopped early.
    pub stopped: Option<String>,
}

impl Gateway {
    /// The calls a probe of `named` (an alias or a model id) would make.
    pub fn probe_plan(&self, named: &str) -> Result<Plan, RemoteError> {
        let route = self.cfg.providers.resolve(named)?;
        let base_url = self.base_url(&route).to_owned();
        self.api(&route)?;
        let (provider, model) = (route.provider, route.model.clone());
        let mut checks = vec![Check::Tools, Check::Streaming, Check::OutputSchema];
        let mut skipped = Vec::new();
        let declared = match (provider, self.cfg.providers.declared(&route)) {
            (Provider::Anthropic, None) => None,
            // A model on a named endpoint is probed for what its declaration says it does, as an OpenAI one.
            _ => Some(self.cfg.providers.openai_capabilities(&route)?),
        };
        match declared {
            None => {
                if crate::profile::profile(&model).effort {
                    checks.push(Check::Effort(Effort::Low));
                } else {
                    skipped.push(format!("effort: {model} takes none, so the gateway never sends one"));
                }
            }
            Some(caps) => {
                checks.retain(|c| match c {
                    Check::Tools => caps.tools,
                    Check::Streaming => caps.streaming,
                    Check::OutputSchema => caps.output_schema,
                    Check::Effort(_) => true,
                });
                for (declared, what) in
                    [(caps.tools, "tools"), (caps.streaming, "streaming"), (caps.output_schema, "output_schema")]
                {
                    if !declared {
                        skipped.push(format!("{what}: declared unsupported, so the gateway never sends it"));
                    }
                }
                match caps.efforts.iter().copied().find(|e| *e != Effort::Max) {
                    Some(effort) => checks.push(Check::Effort(effort)),
                    None => skipped.push("effort: none is declared, so the gateway never sends one".into()),
                }
            }
        }
        Ok(Plan { named: named.to_owned(), provider, endpoint: route.endpoint, model, base_url, checks, skipped })
    }

    /// Make the calls of `plan`, one at a time, with no retries and at most
    /// [`MAX_TOKENS`] of output each. It stops at the first answer that
    /// says the endpoint, model or credential is wrong, since every other
    /// call would fail the same way.
    pub async fn probe(&self, plan: &Plan, now_ms: u64) -> Report {
        let mut findings = Vec::new();
        let mut costs = Some(0.0);
        let mut reasoning = Some(0);
        let mut calls = 0;
        let mut stopped = None;
        for &check in &plan.checks {
            calls += 1;
            let (finding, cost, thought, fatal) = self.check(plan, check).await;
            costs = costs.zip(cost).map(|(a, b)| a + b);
            reasoning = reasoning.zip(thought).map(|(a, b)| a + b);
            findings.push(finding);
            if let Some(why) = fatal {
                stopped = Some(why);
                break;
            }
        }
        let found = |want: fn(Check) -> bool| findings.iter().find(|f| want(f.check)).and_then(|f| f.works);
        let probed = Probed {
            provider: plan.provider,
            base_url: plan.base_url.clone(),
            model: plan.model.clone(),
            probed_at_ms: now_ms,
            tools: found(|c| c == Check::Tools),
            streaming: found(|c| c == Check::Streaming),
            output_schema: found(|c| c == Check::OutputSchema),
            effort: found(|c| matches!(c, Check::Effort(_))),
            cost_usd: costs,
            endpoint: plan.endpoint.clone(),
            reasoning_tokens: reasoning,
        };
        Report { probed, findings, calls, stopped }
    }

    /// One check: what it found, what it cost and the tokens it spent
    /// reasoning (`None`: unknown), and why to stop probing, if this answer
    /// means every call would fail.
    async fn check(&self, plan: &Plan, check: Check) -> (Finding, Option<f64>, Option<u64>, Option<String>) {
        let mut req =
            CompleteRequest { model: Some(plan.qualified()), max_tokens: Some(MAX_TOKENS), ..Default::default() };
        match check {
            Check::Tools => {
                req.messages = vec![user_text("Call the record_number tool with n = 7. Do not answer in text.")];
                req.tools = vec![json!({
                    "name": "record_number",
                    "description": "Record a number.",
                    "input_schema": {
                        "type": "object",
                        "properties": { "n": { "type": "integer" } },
                        "required": ["n"]
                    }
                })];
            }
            Check::Streaming => {
                req.messages = vec![user_text("Say hello in one word.")];
                req.stream = Some(StreamContext { attempt: None, turn: 0 });
            }
            Check::OutputSchema => {
                // Asked for prose, so an endpoint that ignores the schema answers in prose.
                req.messages = vec![user_text("What is 2 + 3? Answer in one short sentence.")];
                req.output_schema = Some(answer_schema());
            }
            Check::Effort(effort) => {
                req.messages = vec![user_text("Say ok.")];
                req.effort = Some(effort);
            }
        }
        let mut chunks = 0usize;
        let result = match check {
            Check::Streaming => self.complete_streamed(req, None, |_| chunks += 1).await,
            _ => self.complete(req).await,
        };
        let reply = match result {
            Ok(reply) => reply,
            Err(e) => {
                let (works, fatal) = classify(check, &e);
                let finding = Finding { check, works, detail: shorten(&e.message) };
                // A refusal by status was not billed (see model::COMPLETE); anything else may have been.
                let unbilled = matches!(e.code, ErrorCode::Invalid | ErrorCode::Denied);
                return (finding, unbilled.then_some(0.0), unbilled.then_some(0), fatal);
            }
        };
        let cost = reply.cost_usd;
        let (works, detail) = match check {
            Check::Tools => {
                let called = reply.content.iter().any(|b| b["type"] == "tool_use" && b["name"] == "record_number");
                (
                    called,
                    if called { "called the tool".into() } else { format!("answered {}", summary(&reply.content)) },
                )
            }
            Check::Streaming => (true, format!("streamed in {chunks} text events")),
            Check::OutputSchema => {
                let text = text_of(&reply.content);
                let matched = serde_json::from_str::<Value>(text.trim())
                    .is_ok_and(|v| v["answer"].is_i64() && v.as_object().is_some_and(|o| o.len() == 1));
                let detail =
                    if matched { "answered in the schema".into() } else { format!("answered {:?}", cut(&text)) };
                (matched, detail)
            }
            Check::Effort(_) => (true, "took it".into()),
        };
        (Finding { check, works: Some(works), detail }, cost, reply.usage.reasoning_tokens, None)
    }
}

impl Plan {
    /// How a call names the probed model and nothing else.
    pub fn qualified(&self) -> String {
        crate::provider::Route { provider: self.provider, endpoint: self.endpoint.clone(), model: self.model.clone() }
            .qualified()
    }
}

/// Settings for a gateway that probes: no retries, so each check is one
/// call, and no effort but the effort check's own. Earlier records are left
/// out, so that what was found before does not shape the calls.
pub fn probing(mut cfg: Config) -> Config {
    cfg.max_retries = 0;
    cfg.effort = None;
    cfg.probed = Default::default();
    cfg
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    })
}

/// What a failed check says about the feature, and whether it means every
/// call would fail. The endpoint refusing the request (400, 422) is its
/// answer about the feature; a credential refused, a model or path not
/// found, or an endpoint out of reach says nothing about it.
fn classify(check: Check, e: &RemoteError) -> (Option<bool>, Option<String>) {
    let status = |code: u16| e.message.contains(&format!("returned {code}"));
    if e.code == ErrorCode::Denied || status(404) {
        return (None, Some(format!("the endpoint refused the call: {}", shorten(&e.message))));
    }
    if e.code == ErrorCode::Unavailable && e.message.contains("could not reach") {
        return (None, Some(format!("the endpoint is out of reach: {}", shorten(&e.message))));
    }
    if status(400) || status(422) {
        return (Some(false), None);
    }
    // A success status with a body that is not a stream, for a streamed request.
    if check == Check::Streaming && e.code == ErrorCode::Failed && e.message.contains("stream") {
        return (Some(false), None);
    }
    (None, None)
}

fn text_of(content: &[Value]) -> String {
    content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")
}

fn summary(content: &[Value]) -> String {
    let text = text_of(content);
    if text.is_empty() {
        "with no tool call".into()
    } else {
        format!("in text: {:?}", cut(&text))
    }
}

fn cut(text: &str) -> String {
    let mut out: String = text.trim().chars().take(80).collect();
    if text.trim().chars().count() > 80 {
        out.push('…');
    }
    out
}

fn shorten(message: &str) -> String {
    let mut out: String = message.chars().take(200).collect();
    if message.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// For a model that ignores `output_schema`: the request asks for the
/// schema as the input of a tool it must call, and thinking is left out,
/// which a forced tool call does not take. `Err` when the request has tools
/// of its own, since forcing this one would keep the model from them.
pub(crate) fn schema_as_tool(body: &mut serde_json::Map<String, Value>, schema: Value) -> Result<(), RemoteError> {
    if body.contains_key("tools") {
        return Err(RemoteError {
            code: ErrorCode::Invalid,
            message: "this model ignores output_schema (found by `molt models probe`), and a request with tools of \
                      its own cannot have it answered through a tool instead: leave out output_schema or the tools"
                .into(),
        });
    }
    body.remove("thinking");
    body.insert(
        "tools".into(),
        json!([{
            "name": SCHEMA_TOOL,
            "description": "Give your answer. Its input is the whole answer.",
            "input_schema": schema
        }]),
    );
    body.insert("tool_choice".into(), json!({ "type": "tool", "name": SCHEMA_TOOL }));
    Ok(())
}

/// The answer of a call made through [`schema_as_tool`], as the caller
/// asked for it: the tool's input as the reply's JSON text. A reply without
/// the call is left as it came, for the caller to find it does not match.
pub(crate) fn tool_as_answer(content: &mut Vec<Value>, stop_reason: &mut Option<String>) {
    let Some(input) = content.iter().find(|b| b["type"] == "tool_use" && b["name"] == SCHEMA_TOOL).map(|b| &b["input"])
    else {
        return;
    };
    *content = vec![json!({ "type": "text", "text": input.to_string() })];
    *stop_reason = Some(molt_api::model::STOP_END_TURN.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probed(model: &str, base_url: &str) -> Probed {
        Probed {
            provider: Provider::Anthropic,
            base_url: base_url.into(),
            model: model.into(),
            probed_at_ms: 1,
            tools: Some(true),
            streaming: Some(true),
            output_schema: Some(false),
            effort: None,
            cost_usd: None,
            endpoint: None,
            reasoning_tokens: None,
        }
    }

    #[test]
    fn a_record_holds_for_its_own_endpoint_and_a_newer_probe_replaces_it() {
        let mut records = Records::default();
        records.put(probed("glm-4.6", "https://a.example"));
        records.put(probed("glm-4.6", "https://b.example"));
        let mut newer = probed("glm-4.6", "https://a.example");
        newer.output_schema = Some(true);
        records.put(newer.clone());
        assert_eq!(records.models.len(), 2);
        assert_eq!(records.lookup(Provider::Anthropic, "https://a.example/", "glm-4.6"), Some(&newer));
        assert!(records.lookup(Provider::Openai, "https://a.example", "glm-4.6").is_none());
        assert!(records.lookup(Provider::Anthropic, "https://c.example", "glm-4.6").is_none());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("molt/models.json");
        records.write(&path).unwrap();
        assert_eq!(Records::load(&path).unwrap(), records);
        let var = |name: &str| (name == FILE_VAR).then(|| path.display().to_string());
        assert_eq!(Records::from_vars(var).unwrap(), records);
    }

    #[test]
    fn the_records_live_in_the_data_dir_and_a_bad_file_is_an_error() {
        let home = |name: &str| (name == "HOME").then(|| "/home/u".to_owned());
        assert_eq!(records_path(home), Some(PathBuf::from("/home/u/.local/share/molt/models.json")));
        let xdg = |name: &str| match name {
            "HOME" => Some("/home/u".to_owned()),
            "XDG_DATA_HOME" => Some("/data".to_owned()),
            _ => None,
        };
        assert_eq!(records_path(xdg), Some(PathBuf::from("/data/molt/models.json")));

        let dir = tempfile::tempdir().unwrap();
        let none = dir.path().display().to_string();
        assert_eq!(Records::from_vars(|n| (n == "HOME").then(|| none.clone())).unwrap(), Records::default());
        let path = dir.path().join("models.json");
        std::fs::write(&path, "not json").unwrap();
        let named = |name: &str| (name == FILE_VAR).then(|| path.display().to_string());
        let error = format!("{:#}", Records::from_vars(named).unwrap_err());
        assert!(error.contains("probe the models again"), "{error}");
        std::fs::write(&path, r#"{"format":2,"models":[]}"#).unwrap();
        assert!(Records::from_vars(named).is_err());
    }

    #[test]
    fn a_status_that_says_nothing_about_the_feature_records_nothing() {
        let error = |code, message: &str| RemoteError { code, message: message.into() };
        let refused = error(ErrorCode::Invalid, "the Messages API returned 400: output_config is not supported");
        assert_eq!(classify(Check::OutputSchema, &refused), (Some(false), None));
        let missing = error(ErrorCode::Invalid, "the Messages API returned 404: no such model");
        assert!(matches!(classify(Check::Tools, &missing), (None, Some(_))));
        assert!(matches!(classify(Check::Tools, &error(ErrorCode::Denied, "returned 401")), (None, Some(_))));
        let busy = error(ErrorCode::Busy, "the Messages API returned 429");
        assert_eq!(classify(Check::Tools, &busy), (None, None));
        let plain = error(ErrorCode::Failed, "invalid Messages API stream: expected text/event-stream");
        assert_eq!(classify(Check::Streaming, &plain), (Some(false), None));
        assert_eq!(classify(Check::Tools, &plain), (None, None));
    }
}
