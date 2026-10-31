//! What each model costs, and where that price came from.
//!
//! Prices decide when a run's budget stops it, so they are a trusted input,
//! like the API key: they come only from the user. In order, the first that
//! has the model wins:
//!
//! 1. `MOLT_MODEL_PRICES`, such as `glm-4.6=0.6,2.2;deepseek-chat=0.28,0.42`.
//! 2. The snapshot `molt prices update` wrote to the user's data directory
//!    (`MOLT_PRICES_FILE` names another), or, until there is one, the
//!    snapshot built into this release.
//! 3. The built-in Anthropic prices.
//!
//! Nothing in a project directory is read, the table is loaded once when the
//! gateway starts, and nothing is downloaded while a run is going. A model
//! none of them has stays unknown: it is never priced as free.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context};
pub use molt_api::model::PriceSource;
use molt_api::model::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Prices the user declares, ahead of every other source.
pub const OVERRIDE_VAR: &str = "MOLT_MODEL_PRICES";
/// A snapshot to use instead of the one in the user's data directory.
pub const FILE_VAR: &str = "MOLT_PRICES_FILE";
/// The snapshot format this release reads and writes.
const FORMAT: u32 = 1;
/// The snapshot built into this release, used until `molt prices update` writes one.
const BUNDLED: &str = include_str!("../prices/bundled.json");
/// A download larger than this is not a price table.
const MAX_DOWNLOAD: usize = 64 << 20;
/// A price that moves by more than this factor between snapshots is pointed out.
const BIG_CHANGE: f64 = 2.0;

/// US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prices {
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

    /// Prices from a source that may leave out the cache prices, erring high
    /// rather than low: a missing cache write costs what Anthropic charges
    /// (1.25 times input) and a missing cache read costs a full input token.
    /// No provider charges less for writing a token to the cache than for
    /// reading it uncached, so a cache write is never priced below input.
    pub fn stated(input: f64, output: f64, cache_write: Option<f64>, cache_read: Option<f64>) -> Self {
        Self::from_stated(Stated(input, output, cache_write, cache_read))
    }

    /// [`Prices::stated`], from what a source states.
    fn from_stated(stated: Stated) -> Self {
        let Stated(input, output, cache_write, cache_read) = stated;
        Self {
            input,
            output,
            cache_write: cache_write.map_or(input * 1.25, |w| w.max(input)),
            cache_read: cache_read.unwrap_or(input),
        }
    }

    fn max(self, other: Self) -> Self {
        Self {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_write: self.cache_write.max(other.cache_write),
            cache_read: self.cache_read.max(other.cache_read),
        }
    }
}

/// Input, output, cache write and cache read prices as a source states them,
/// in dollars per million tokens; a source may leave out the cache prices.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stated(pub f64, pub f64, pub Option<f64>, pub Option<f64>);

impl Stated {
    fn values(&self) -> impl Iterator<Item = f64> {
        [Some(self.0), Some(self.1), self.2, self.3].into_iter().flatten()
    }
}

/// A price table downloaded by `molt prices update`, or built into a release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    /// Where the prices come from, such as `litellm`.
    pub source: String,
    pub url: String,
    /// SHA-256 of the downloaded bytes the snapshot was made from.
    pub sha256: String,
    pub fetched_at_ms: u64,
    /// Lowercase model id to its prices.
    pub models: BTreeMap<String, Stated>,
}

impl Snapshot {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let snapshot: Self = serde_json::from_str(text).context("not a molt price snapshot")?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn bundled() -> Self {
        Self::parse(BUNDLED).expect("the bundled price snapshot is valid")
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.format == FORMAT, "price snapshot format {} is not {FORMAT}", self.format);
        ensure!(!self.models.is_empty(), "the price snapshot has no models");
        for (model, stated) in &self.models {
            ensure!(!model.is_empty() && *model == model.to_lowercase(), "model id {model:?} is not lowercase");
            for v in stated.values() {
                ensure!(v.is_finite() && v >= 0.0, "{model}: price {v} is not a finite, non-negative number");
            }
        }
        Ok(())
    }

    /// The snapshot as JSON, one model per line so that a diff between two
    /// snapshots shows the prices that changed.
    pub fn to_json(&self) -> String {
        let s = |v: &str| serde_json::to_string(v).unwrap_or_default();
        let mut out = format!(
            "{{\n  \"format\": {},\n  \"source\": {},\n  \"url\": {},\n  \"sha256\": {},\n  \"fetched_at_ms\": {},\n  \"models\": {{\n",
            self.format,
            s(&self.source),
            s(&self.url),
            s(&self.sha256),
            self.fetched_at_ms
        );
        let last = self.models.len().saturating_sub(1);
        for (i, (model, stated)) in self.models.iter().enumerate() {
            let prices = serde_json::to_string(stated).unwrap_or_default();
            out.push_str(&format!("    {}: {prices}{}\n", s(model), if i == last { "" } else { "," }));
        }
        out.push_str("  }\n}\n");
        out
    }

    /// Write the snapshot to `path` all at once: a reader sees the old one or
    /// the new one, never part of either.
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir).with_context(|| format!("writing in {}", dir.display()))?;
        tmp.write_all(self.to_json().as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Models priced in both whose input or output price moved by more than
    /// [`BIG_CHANGE`] times, up or down.
    pub fn big_changes(&self, newer: &Snapshot) -> Vec<String> {
        let moved = |a: f64, b: f64| {
            let (lo, hi) = (a.min(b), a.max(b));
            hi > 0.0 && (lo == 0.0 || hi / lo > BIG_CHANGE)
        };
        self.models
            .iter()
            .filter_map(|(model, old)| {
                let new = newer.models.get(model)?;
                (moved(old.0, new.0) || moved(old.1, new.1)).then(|| model.clone())
            })
            .collect()
    }
}

/// One source in the table, with the prices it gives.
#[derive(Clone, Debug)]
struct Level {
    /// The [`PriceSource`] of a price found here, without its `entry`.
    origin: PriceSource,
    models: BTreeMap<String, Prices>,
    /// The highest price among entries of the same model from several
    /// providers (`deepseek/deepseek-chat`, `openrouter/deepseek/deepseek-chat`),
    /// by the name after the last `/`, for names no entry has on its own.
    by_name: BTreeMap<String, Prices>,
}

impl Level {
    fn new(origin: PriceSource, models: BTreeMap<String, Prices>) -> Self {
        let mut by_name: BTreeMap<String, Prices> = BTreeMap::new();
        for (model, prices) in &models {
            let name = last_segment(model);
            if name != model && !models.contains_key(name) {
                by_name.entry(name.to_owned()).and_modify(|p| *p = p.max(*prices)).or_insert(*prices);
            }
        }
        Self { origin, models, by_name }
    }

    fn found(&self, entry: String, prices: Prices) -> (Prices, PriceSource) {
        (prices, PriceSource { entry, ..self.origin.clone() })
    }

    fn lookup(&self, model: &str) -> Option<(Prices, PriceSource)> {
        let names = names(model);
        if let Some(name) = names.iter().find(|n| self.models.contains_key(*n)) {
            return Some(self.found(name.clone(), self.models[name]));
        }
        names
            .iter()
            .find_map(|n| self.by_name.get(n).map(|p| (n, p)))
            .map(|(name, prices)| self.found(format!("*/{name}"), *prices))
    }
}

/// Every price the gateway knows, in the order it looks.
#[derive(Clone, Debug)]
pub struct PriceTable {
    levels: Vec<Level>,
}

impl Default for PriceTable {
    /// The built-in Anthropic prices alone.
    fn default() -> Self {
        Self { levels: vec![built_in()] }
    }
}

impl PriceTable {
    /// The table from the environment: see the module documentation.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The table from these variables. An empty variable counts as unset; a
    /// malformed `MOLT_MODEL_PRICES` or snapshot is an error, not a fallback.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let var = |name: &str| var(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let mut levels = Vec::new();
        if let Some(text) = var(OVERRIDE_VAR) {
            let origin = PriceSource { source: OVERRIDE_VAR.into(), ..Default::default() };
            levels.push(Level::new(origin, parse_overrides(&text)?));
        }
        let snapshot = match snapshot_path(var) {
            Some(path) if path.exists() || var(FILE_VAR).is_some() => {
                let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                Snapshot::parse(&text).with_context(|| {
                    format!("the price snapshot {} is not usable: run `molt prices update` again", path.display())
                })?
            }
            _ => Snapshot::bundled(),
        };
        levels.push(snapshot_level(snapshot));
        levels.push(built_in());
        Ok(Self { levels })
    }

    /// The prices for a model id, with where they came from. A dated
    /// snapshot id (`-YYYYMMDD`) and a provider prefix (`deepseek/`) fall
    /// back to the bare id.
    pub fn lookup(&self, model: &str) -> Option<(Prices, PriceSource)> {
        self.levels.iter().find_map(|level| level.lookup(model))
    }

    /// Each source the table holds, highest priority first, with its model count.
    pub fn sources(&self) -> Vec<(PriceSource, usize)> {
        self.levels.iter().map(|l| (l.origin.clone(), l.models.len())).collect()
    }
}

/// The names [`PriceTable::lookup`] tries for `model`, in order.
pub fn names(model: &str) -> Vec<String> {
    let model = model.trim().to_lowercase();
    let name = last_segment(&model).to_owned();
    let mut names = Vec::new();
    for n in [model.clone(), base_id(&model).to_owned(), name.clone(), base_id(&name).to_owned()] {
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    names
}

fn last_segment(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// `model` without a trailing `-YYYYMMDD` snapshot date.
pub(crate) fn base_id(model: &str) -> &str {
    match model.rsplit_once('-') {
        Some((base, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => model,
    }
}

fn built_in() -> Level {
    let price = |input, output, cache_write, cache_read| Prices { input, output, cache_write, cache_read };
    let models = [
        ("claude-opus-5-5", price(4.0, 20.0, 5.0, 0.20)),
        ("claude-sonnet-5-5", price(2.0, 10.0, 2.50, 0.20)),
        ("claude-haiku-4-5", price(1.0, 5.0, 1.25, 0.10)),
    ];
    let origin = PriceSource { source: "built-in".into(), ..Default::default() };
    Level::new(origin, models.into_iter().map(|(m, p)| (m.to_owned(), p)).collect())
}

fn snapshot_level(snapshot: Snapshot) -> Level {
    let origin = PriceSource {
        entry: String::new(),
        source: snapshot.source,
        url: Some(snapshot.url),
        sha256: Some(snapshot.sha256),
        fetched_at_ms: Some(snapshot.fetched_at_ms),
    };
    Level::new(origin, snapshot.models.into_iter().map(|(m, s)| (m, Prices::from_stated(s))).collect())
}

/// `model=input,output[,cache_write,cache_read];…`, in dollars per million
/// tokens. With two numbers the cache prices err high, as for a snapshot.
fn parse_overrides(text: &str) -> anyhow::Result<BTreeMap<String, Prices>> {
    let mut models = BTreeMap::new();
    for (i, item) in text.split(';').map(str::trim).enumerate().filter(|(_, item)| !item.is_empty()) {
        let fail = |why: &str| {
            anyhow::anyhow!(
                "{OVERRIDE_VAR}: item {} ({item:?}) {why}; write model=input,output or \
                 model=input,output,cache_write,cache_read in US dollars per million tokens, items separated by ;",
                i + 1
            )
        };
        let (model, numbers) = item.split_once('=').ok_or_else(|| fail("has no ="))?;
        let model = model.trim().to_lowercase();
        if model.is_empty() {
            return Err(fail("names no model"));
        }
        let numbers = numbers
            .split(',')
            .map(|n| n.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0))
            .collect::<Option<Vec<f64>>>()
            .ok_or_else(|| fail("has a price that is not a non-negative number"))?;
        let prices = match numbers[..] {
            [input, output] => Prices::from_stated(Stated(input, output, None, None)),
            [input, output, cache_write, cache_read] => Prices { input, output, cache_write, cache_read },
            _ => return Err(fail("does not have 2 or 4 prices")),
        };
        if models.insert(model.clone(), prices).is_some() {
            return Err(fail("prices a model an earlier item already prices"));
        }
    }
    Ok(models)
}

/// Where the user's price snapshot is: `MOLT_PRICES_FILE`, or
/// `molt/prices.json` in `XDG_DATA_HOME` (`~/.local/share` by default).
/// The sandbox hides home directories from the commands an agent runs, so
/// they can neither read nor change it.
pub fn snapshot_path(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(file) = var(FILE_VAR).filter(|f| !f.is_empty()) {
        return Some(file.into());
    }
    let data = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or_else(|| var("HOME").filter(|h| !h.is_empty()).map(|h| Path::new(&h).join(".local/share")))?;
    Some(data.join("molt").join("prices.json"))
}

/// The variables that give a molt started with another home directory (as
/// `molt bench` starts its runs) the same prices as this process.
pub fn forward_env() -> Vec<(String, String)> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let mut env = Vec::new();
    if let Some(prices) = var(OVERRIDE_VAR) {
        env.push((OVERRIDE_VAR.to_owned(), prices));
    }
    if let Some(path) = snapshot_path(var).filter(|p| p.exists()) {
        let path = std::path::absolute(&path).unwrap_or(path);
        env.push((FILE_VAR.to_owned(), path.display().to_string()));
    }
    env
}

/// The price lists `molt prices update` can download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// LiteLLM's `model_prices_and_context_window.json`, the widest coverage.
    Litellm,
    /// OpenRouter's model list, with the prices it charges.
    Openrouter,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Self::Litellm => "litellm",
            Self::Openrouter => "openrouter",
        }
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Self::Litellm => {
                "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
            }
            Self::Openrouter => "https://openrouter.ai/api/v1/models",
        }
    }
}

impl std::str::FromStr for Source {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "litellm" => Ok(Self::Litellm),
            "openrouter" => Ok(Self::Openrouter),
            other => Err(format!("{other:?} is not a price source: use litellm or openrouter")),
        }
    }
}

/// Download `url` (the source's own when `None`) and turn it into a
/// validated snapshot. Nothing is written.
pub async fn download(source: Source, url: Option<&str>) -> anyhow::Result<Snapshot> {
    let url = url.unwrap_or(source.default_url());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("molt/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().await.with_context(|| format!("downloading {url}"))?;
    let status = resp.status();
    ensure!(status.is_success(), "downloading {url}: HTTP {status}");
    ensure!(resp.content_length().is_none_or(|n| n <= MAX_DOWNLOAD as u64), "{url} is too large to be a price list");
    let bytes = resp.bytes().await.with_context(|| format!("downloading {url}"))?;
    ensure!(bytes.len() <= MAX_DOWNLOAD, "{url} is too large to be a price list");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    convert(source, url, &bytes, now)
}

/// A snapshot of what `bytes`, downloaded from `url`, says. Anything
/// malformed (not JSON, the wrong shape, a negative or non-finite price, no
/// model with both an input and an output price) fails the whole download.
pub fn convert(source: Source, url: &str, bytes: &[u8], fetched_at_ms: u64) -> anyhow::Result<Snapshot> {
    let json: Value = serde_json::from_slice(bytes).with_context(|| format!("{url} is not JSON"))?;
    let models = match source {
        Source::Litellm => litellm(&json),
        Source::Openrouter => openrouter(&json),
    }
    .with_context(|| format!("{url} is not a {} price list", source.name()))?;
    let snapshot = Snapshot {
        format: FORMAT,
        source: source.name().into(),
        url: url.into(),
        sha256: hex::encode(Sha256::digest(bytes)),
        fetched_at_ms,
        models,
    };
    snapshot.validate().with_context(|| format!("{url} is not a usable price list"))?;
    Ok(snapshot)
}

/// A price in dollars per token as dollars per million, rounded to a
/// billionth of a dollar so that the float noise of the conversion goes.
fn per_million(per_token: f64) -> f64 {
    (per_token * 1e15).round() / 1e9
}

fn litellm(json: &Value) -> anyhow::Result<BTreeMap<String, Stated>> {
    let entries = json.as_object().context("the top level is not an object")?;
    let mut models: BTreeMap<String, Stated> = BTreeMap::new();
    for (model, entry) in entries {
        let Some(entry) = entry.as_object() else { continue };
        let mode = entry.get("mode").and_then(Value::as_str);
        if model == "sample_spec" || model.contains('*') || !matches!(mode, Some("chat" | "responses")) {
            continue;
        }
        let price = |name: &str| -> anyhow::Result<Option<f64>> {
            match entry.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => {
                    let v = n.as_f64().unwrap_or(f64::NAN);
                    ensure!(v.is_finite() && v >= 0.0, "{model}: {name} is {v}");
                    Ok(Some(v))
                }
                Some(other) => bail!("{model}: {name} is {other}, not a number"),
            }
        };
        // Long prompts cost more on some models: take the highest tier, so a
        // long-context call is never undercounted.
        let tiered = |base: &str| -> anyhow::Result<Option<f64>> {
            let Some(mut v) = price(base)? else { return Ok(None) };
            let tier = format!("{base}_above_");
            for name in entry.keys().filter(|k| {
                k.strip_prefix(&tier)
                    .and_then(|rest| rest.strip_suffix("k_tokens"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            }) {
                v = v.max(price(name)?.unwrap_or(0.0));
            }
            Ok(Some(per_million(v)))
        };
        let (Some(input), Some(output)) = (tiered("input_cost_per_token")?, tiered("output_cost_per_token")?) else {
            continue;
        };
        let stated =
            Stated(input, output, tiered("cache_creation_input_token_cost")?, tiered("cache_read_input_token_cost")?);
        insert_highest(&mut models, model, stated);
    }
    ensure!(!models.is_empty(), "no chat model in it has both an input and an output price");
    Ok(models)
}

fn openrouter(json: &Value) -> anyhow::Result<BTreeMap<String, Stated>> {
    let items = json.get("data").and_then(Value::as_array).context("it has no `data` list")?;
    let mut models: BTreeMap<String, Stated> = BTreeMap::new();
    for (i, item) in items.iter().enumerate() {
        let model = item.get("id").and_then(Value::as_str).with_context(|| format!("model {} has no id", i + 1))?;
        let pricing = item.get("pricing").with_context(|| format!("{model} has no pricing"))?;
        // Prices are decimal strings in dollars per token.
        let price = |name: &str| -> anyhow::Result<Option<f64>> {
            let Some(v) = pricing.get(name).filter(|v| !v.is_null()) else { return Ok(None) };
            let text = v.as_str().with_context(|| format!("{model}: {name} is not a string"))?;
            let v: f64 = text.trim().parse().with_context(|| format!("{model}: {name} is {text:?}"))?;
            Ok(Some(v))
        };
        let (Some(input), Some(output)) = (price("prompt")?, price("completion")?) else {
            bail!("{model} has no prompt or completion price");
        };
        // OpenRouter marks a router whose price depends on the model it picks with -1.
        if input == -1.0 || output == -1.0 {
            continue;
        }
        let cache_write = price("input_cache_write")?;
        let cache_read = price("input_cache_read")?;
        for v in [Some(input), Some(output), cache_write, cache_read].into_iter().flatten() {
            ensure!(v.is_finite() && v >= 0.0, "{model}: price {v} is not a finite, non-negative number");
        }
        let stated =
            Stated(per_million(input), per_million(output), cache_write.map(per_million), cache_read.map(per_million));
        insert_highest(&mut models, model, stated);
    }
    ensure!(!models.is_empty(), "it lists no model");
    Ok(models)
}

/// Ids differing only in case are one model: keep the higher prices.
fn insert_highest(models: &mut BTreeMap<String, Stated>, model: &str, stated: Stated) {
    let higher = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        // An unstated cache price is estimated high anyway.
        _ => None,
    };
    models
        .entry(model.to_lowercase())
        .and_modify(|old| {
            *old = Stated(old.0.max(stated.0), old.1.max(stated.1), higher(old.2, stated.2), higher(old.3, stated.3))
        })
        .or_insert(stated);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;

    fn table(vars: &[(&str, &str)]) -> anyhow::Result<PriceTable> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        PriceTable::from_vars(|name| vars.get(name).cloned())
    }

    fn litellm_bytes(models: Value) -> Vec<u8> {
        serde_json::to_vec(&models).unwrap()
    }

    fn deepseek_snapshot() -> Snapshot {
        let bytes = litellm_bytes(json!({
            "deepseek-chat": {"mode": "chat", "input_cost_per_token": 2.8e-7, "output_cost_per_token": 4.2e-7,
                              "cache_read_input_token_cost": 2.8e-8},
        }));
        convert(Source::Litellm, "https://example.test/prices.json", &bytes, 1_000).unwrap()
    }

    #[test]
    fn cost_counts_every_kind_of_token() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_read_input_tokens: 1_000_000,
            reasoning_tokens: None,
        };
        let (opus, _) = PriceTable::default().lookup("claude-opus-5-5").unwrap();
        assert!((opus.cost(&usage) - (4.0 + 20.0 + 5.0 + 0.20)).abs() < 1e-9);
        let (haiku, _) = PriceTable::default().lookup("claude-haiku-4-5").unwrap();
        assert!((haiku.cost(&Usage { output_tokens: 200_000, ..Default::default() }) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dated_ids_and_provider_prefixes_fall_back_to_the_bare_id() {
        let t = PriceTable::default();
        assert!(t.lookup("claude-haiku-4-5-20251001").is_some());
        assert!(t.lookup("anthropic/claude-opus-5-5").is_some());
        assert!(t.lookup("Claude-Opus-5-5").is_some());
        assert!(t.lookup("claude-opus-5-5-fast").is_none());
        assert!(t.lookup("claude-opus-5-5-2026").is_none());
        assert!(t.lookup("").is_none());
        assert_eq!(
            names("openrouter/deepseek/deepseek-chat-20250101"),
            [
                "openrouter/deepseek/deepseek-chat-20250101",
                "openrouter/deepseek/deepseek-chat",
                "deepseek-chat-20250101",
                "deepseek-chat"
            ]
        );
    }

    #[test]
    fn a_name_only_providers_list_gets_the_highest_of_their_prices() {
        let bytes = litellm_bytes(json!({
            "zai/glm-4.6": {"mode": "chat", "input_cost_per_token": 6e-7, "output_cost_per_token": 2.2e-6},
            "openrouter/z-ai/glm-4.6": {"mode": "chat", "input_cost_per_token": 5e-7, "output_cost_per_token": 2.5e-6},
            "deepseek-chat": {"mode": "chat", "input_cost_per_token": 2.8e-7, "output_cost_per_token": 4.2e-7},
            "together/deepseek-chat": {"mode": "chat", "input_cost_per_token": 9e-7, "output_cost_per_token": 9e-7},
        }));
        let level = snapshot_level(convert(Source::Litellm, "u", &bytes, 0).unwrap());
        let (glm, source) = level.lookup("glm-4.6").unwrap();
        assert_eq!((glm.input, glm.output), (0.6, 2.5));
        assert_eq!(source.entry, "*/glm-4.6");
        // A provider's own bare id is used as it is.
        let (deepseek, source) = level.lookup("deepseek-chat").unwrap();
        assert_eq!((deepseek.input, source.entry.as_str()), (0.28, "deepseek-chat"));
        assert_eq!(level.lookup("together/deepseek-chat").unwrap().0.input, 0.9);
    }

    #[test]
    fn missing_cache_prices_err_high_and_a_cache_write_never_costs_less_than_input() {
        let p = Prices::from_stated(Stated(2.0, 8.0, None, None));
        assert_eq!((p.cache_write, p.cache_read), (2.5, 2.0));
        let p = Prices::from_stated(Stated(2.0, 8.0, Some(0.0), Some(0.2)));
        assert_eq!((p.cache_write, p.cache_read), (2.0, 0.2));
    }

    #[test]
    fn a_snapshot_prices_a_third_party_model_and_says_where_the_price_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.json");
        let snapshot = deepseek_snapshot();
        snapshot.write(&path).unwrap();
        let t = table(&[(FILE_VAR, path.to_str().unwrap())]).unwrap();
        let (prices, source) = t.lookup("deepseek-chat").unwrap();
        assert_eq!((prices.input, prices.output, prices.cache_read), (0.28, 0.42, 0.028));
        assert_eq!(source.source, "litellm");
        assert_eq!(source.url.as_deref(), Some("https://example.test/prices.json"));
        assert_eq!(source.sha256, Some(snapshot.sha256.clone()));
        assert_eq!(source.fetched_at_ms, Some(1_000));
        // The built-in prices stay behind the snapshot.
        assert_eq!(t.lookup("claude-opus-5-5").unwrap().1.source, "built-in");
        assert!(t.lookup("no-such-model").is_none());
    }

    #[test]
    fn the_data_directory_holds_the_snapshot_and_nothing_else_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        assert_eq!(
            snapshot_path(|n| (n == "HOME").then(|| home.to_owned())),
            Some(dir.path().join(".local/share/molt/prices.json"))
        );
        let xdg = |n: &str| match n {
            "HOME" => Some(home.to_owned()),
            "XDG_DATA_HOME" => Some("/data".to_owned()),
            _ => None,
        };
        assert_eq!(snapshot_path(xdg), Some(PathBuf::from("/data/molt/prices.json")));
        assert_eq!(snapshot_path(|_| None), None);

        // A price file in the current (project) directory is not a source.
        let project = tempfile::tempdir().unwrap();
        deepseek_snapshot().write(&project.path().join("prices.json")).unwrap();
        let t = table(&[("HOME", home)]).unwrap();
        assert_eq!(t.lookup("deepseek-chat").map(|(_, s)| s.url), Some(Some(Snapshot::bundled().url)));
        deepseek_snapshot().write(&dir.path().join(".local/share/molt/prices.json")).unwrap();
        let t = table(&[("HOME", home)]).unwrap();
        assert_eq!(t.lookup("deepseek-chat").unwrap().1.url.as_deref(), Some("https://example.test/prices.json"));
    }

    #[test]
    fn declared_prices_override_the_snapshot_and_the_built_in_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.json");
        deepseek_snapshot().write(&path).unwrap();
        let t = table(&[
            (FILE_VAR, path.to_str().unwrap()),
            (OVERRIDE_VAR, " DeepSeek-Chat = 1, 2 ; claude-opus-5-5=1,2,3,0.5;"),
        ])
        .unwrap();
        let (deepseek, source) = t.lookup("deepseek-chat").unwrap();
        assert_eq!((deepseek.input, deepseek.output, deepseek.cache_write, deepseek.cache_read), (1.0, 2.0, 1.25, 1.0));
        assert_eq!((source.source.as_str(), source.sha256), (OVERRIDE_VAR, None));
        let (opus, _) = t.lookup("claude-opus-5-5-20260101").unwrap();
        assert_eq!((opus.cache_write, opus.cache_read), (3.0, 0.5));
    }

    #[test]
    fn malformed_declared_prices_stop_startup_and_name_the_item() {
        for (value, item) in [
            ("a=1,2;b", "item 2"),
            ("a=1", "item 1"),
            ("a=1,2,3", "item 1"),
            ("a=-1,2", "item 1"),
            ("a=1,inf", "item 1"),
            ("=1,2", "item 1"),
            ("a=1,2;A=3,4", "item 2"),
        ] {
            let err = table(&[(OVERRIDE_VAR, value)]).unwrap_err().to_string();
            assert!(err.starts_with(OVERRIDE_VAR) && err.contains(item), "{value}: {err}");
        }
    }

    #[test]
    fn a_broken_snapshot_file_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.json");
        std::fs::write(&path, "{").unwrap();
        let err = table(&[(FILE_VAR, path.to_str().unwrap())]).unwrap_err();
        assert!(format!("{err:#}").contains("molt prices update"), "{err:#}");
        let missing = dir.path().join("missing.json");
        assert!(table(&[(FILE_VAR, missing.to_str().unwrap())]).is_err());
    }

    #[test]
    fn a_download_that_does_not_validate_is_refused() {
        let bad = [
            b"not json".to_vec(),
            b"[]".to_vec(),
            litellm_bytes(json!({"m": {"mode": "chat", "input_cost_per_token": -1e-6, "output_cost_per_token": 1e-6}})),
            litellm_bytes(json!({"m": {"mode": "chat", "input_cost_per_token": "1", "output_cost_per_token": 1e-6}})),
            litellm_bytes(json!({"m": {"mode": "chat", "max_tokens": 10}})),
            litellm_bytes(
                json!({"m": {"mode": "embedding", "input_cost_per_token": 1e-6, "output_cost_per_token": 0}}),
            ),
        ];
        for bytes in bad {
            assert!(convert(Source::Litellm, "u", &bytes, 0).is_err(), "{}", String::from_utf8_lossy(&bytes));
        }
        for body in [
            json!({"models": []}),
            json!({"data": [{"id": "m"}]}),
            json!({"data": [{"id": "m", "pricing": {"prompt": "-0.5", "completion": "0.1"}}]}),
            json!({"data": [{"id": "m", "pricing": {"prompt": "cheap", "completion": "0.1"}}]}),
        ] {
            assert!(convert(Source::Openrouter, "u", &litellm_bytes(body.clone()), 0).is_err(), "{body}");
        }
    }

    #[test]
    fn litellm_and_openrouter_lists_convert_to_prices_per_million() {
        let bytes = litellm_bytes(json!({
            "sample_spec": {"mode": "chat", "input_cost_per_token": 0.0, "output_cost_per_token": 0.0},
            "GPT-X": {"mode": "chat", "input_cost_per_token": 1.25e-6, "output_cost_per_token": 1e-5,
                      "input_cost_per_token_above_200k_tokens": 2.5e-6, "input_cost_per_token_priority": 9e-6,
                      "cache_read_input_token_cost": 1.25e-7},
            "bedrock/*/x": {"mode": "chat", "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6},
            "no-output": {"mode": "chat", "input_cost_per_token": 1e-6},
        }));
        let snapshot = convert(Source::Litellm, "u", &bytes, 0).unwrap();
        assert_eq!(snapshot.models.len(), 1);
        assert_eq!(snapshot.models["gpt-x"], Stated(2.5, 10.0, None, Some(0.125)));

        let bytes = litellm_bytes(json!({"data": [
            {"id": "deepseek/deepseek-chat", "pricing": {"prompt": "0.00000027", "completion": "0.0000011",
                                                         "input_cache_read": "0.00000007"}},
            {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}},
        ]}));
        let snapshot = convert(Source::Openrouter, "u", &bytes, 0).unwrap();
        assert_eq!(snapshot.models.len(), 1);
        assert_eq!(snapshot.models["deepseek/deepseek-chat"], Stated(0.27, 1.1, None, Some(0.07)));
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_and_big_changes_are_found() {
        let old = deepseek_snapshot();
        assert_eq!(Snapshot::parse(&old.to_json()).unwrap(), old);
        let mut new = old.clone();
        assert!(old.big_changes(&new).is_empty());
        new.models.insert("deepseek-chat".into(), Stated(0.28, 0.1, None, None));
        assert_eq!(old.big_changes(&new), ["deepseek-chat"]);
    }

    #[test]
    fn the_bundled_snapshot_is_valid_and_prices_the_anthropic_models() {
        let bundled = Snapshot::bundled();
        assert!(bundled.models.len() > 100);
        let t = table(&[]).unwrap();
        for model in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5"] {
            assert!(t.lookup(model).is_some(), "{model}");
        }
    }
}
