//! `molt env prepare`: a project's locked dependencies, downloaded from the
//! registries the user allows and installed offline, in the sandbox, into a
//! private environment that later commands get read-only.
//!
//! Python's pip is the first package manager. The lock is a hash-pinned
//! `requirements.txt` (`pip-compile --generate-hashes`). The steps are kept
//! apart, each with its own grant:
//!
//! - **Download**, by Molt itself, never by the package manager: each
//!   package's file is looked up in the index of the user's config and
//!   fetched only from the origins it allows, redirects included, then
//!   checked against the lock's hashes before it enters the cache. No
//!   project code runs while the network is in use.
//! - **Install**, by pip in the sandbox without network: the virtual
//!   environment is the only host directory it may write, and its
//!   environment is the sandbox's, with no service credential. A source
//!   distribution, whose build scripts would run, is used only for the
//!   packages the user's config names in `scripts`.
//!
//! The cache is content-addressed and has a quota. A file enters it only
//! whole and verified, so a cancelled or broken download leaves nothing
//! that is taken for complete, and is retried. The install is not retried:
//! a failure is recorded and the environment is left unfinished, and the
//! next preparation starts it over from nothing. The environment is keyed
//! by the lock, the interpreter, the index and the script grants, so a
//! change in any of them is a new preparation. Every preparation, and every
//! failure, is recorded in `<data dir>/deps/records.jsonl`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure, Context};
use molt_tools::{env_profile, EnvProfile, SandboxPolicy};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::readiness::{Item, Kind, Status};

/// The environment profile that gives commands a prepared environment.
pub const PROFILE: &str = "python-prepared";
/// The lock file read, relative to the project.
pub const LOCK: &str = "requirements.txt";
/// What marks an environment complete; written last.
const MANIFEST: &str = "molt-prepared.json";
/// The largest lock file read.
const MAX_LOCK: u64 = 1 << 20;
/// The most packages one lock may list.
const MAX_PACKAGES: usize = 2000;
/// The largest index page read.
const MAX_PAGE: usize = 64 << 20;
/// How many times a download is tried.
const ATTEMPTS: u32 = 3;
/// How many redirects one request may follow.
const MAX_REDIRECTS: usize = 5;
const VENV_MS: u64 = 300_000;
const INSTALL_MS: u64 = 1_800_000;

/// `[prepare]` of the user's config: where `molt env prepare` may download
/// from, and what may run while installing. A project's config has no such
/// section, so it cannot widen either.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrepareSection {
    pub python: PythonPrepare,
}

impl PrepareSection {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.python.validate().context("in [prepare.python]")
    }
}

/// `[prepare.python]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PythonPrepare {
    /// The package index (the PEP 503 or 691 simple API) packages are looked up in.
    pub index: String,
    /// Other origins (`https://host[:port]`) files and redirects may come from.
    pub hosts: Vec<String>,
    /// Packages that may be built from a source distribution, which runs
    /// their build scripts (in the sandbox, without network).
    pub scripts: Vec<String>,
    /// The download cache's quota, in MiB.
    pub max_mb: u64,
}

impl Default for PythonPrepare {
    fn default() -> Self {
        Self {
            index: "https://pypi.org/simple/".to_owned(),
            hosts: vec!["https://files.pythonhosted.org".to_owned()],
            scripts: Vec::new(),
            max_mb: 2048,
        }
    }
}

impl PythonPrepare {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.index_url()?;
        self.origins()?;
        ensure!(self.scripts.len() <= 256, "scripts names more than 256 packages");
        for name in &self.scripts {
            ensure!(valid_name(name), "scripts: `{name}` is not a package name");
        }
        ensure!(self.max_mb > 0 && self.max_mb <= 1_000_000, "max_mb must be between 1 and 1000000");
        Ok(())
    }

    fn index_url(&self) -> anyhow::Result<Url> {
        let mut url = Url::parse(&self.index).with_context(|| format!("index `{}` is not a URL", self.index))?;
        web_url(&url).with_context(|| format!("index `{}`", self.index))?;
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(url)
    }

    /// The origins requests may go to: the index's and `hosts`.
    fn origins(&self) -> anyhow::Result<Vec<String>> {
        let mut out = vec![self.index_url()?.origin().ascii_serialization()];
        ensure!(self.hosts.len() <= 32, "hosts names more than 32 origins");
        for host in &self.hosts {
            let url = Url::parse(host).with_context(|| format!("hosts: `{host}` is not an origin"))?;
            web_url(&url).with_context(|| format!("hosts: `{host}`"))?;
            ensure!(
                url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
                "hosts: `{host}` has a path: give only scheme, host and port, such as https://files.pythonhosted.org"
            );
            out.push(url.origin().ascii_serialization());
        }
        Ok(out)
    }

    fn may_build(&self, name: &str) -> bool {
        self.scripts.iter().any(|s| normalize(s) == name)
    }
}

/// An http(s) URL without credentials in it.
fn web_url(url: &Url) -> anyhow::Result<()> {
    ensure!(matches!(url.scheme(), "https" | "http"), "only http and https are allowed");
    ensure!(url.host().is_some(), "has no host");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "carries a user or password, which Molt does not send: use an index that needs none"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The lock.

/// One package of the lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locked {
    /// Normalized (PEP 503).
    pub name: String,
    pub version: String,
    /// SHA-256 digests in lowercase hex, any of which a file may have.
    pub hashes: Vec<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        && name.as_bytes()[0].is_ascii_alphanumeric()
}

/// A package name as PEP 503 compares it: lowercase, each run of `-_.` one `-`.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if "-_.".contains(c) {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// The packages of a hash-pinned `requirements.txt`: every line one
/// `name==version` with at least one `--hash=sha256:...`, as
/// `pip-compile --generate-hashes` writes it. Anything that would make pip
/// look elsewhere, or pick by the environment, is refused.
pub fn parse_lock(text: &str) -> anyhow::Result<Vec<Locked>> {
    let mut out: Vec<Locked> = Vec::new();
    let mut logical = String::new();
    let mut first = 0;
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        if logical.is_empty() {
            first = i + 1;
        }
        let line = match raw.find(" #").or_else(|| raw.trim_start().starts_with('#').then_some(0)) {
            Some(at) => &raw[..at],
            None => raw,
        };
        match line.trim_end().strip_suffix('\\') {
            Some(rest) => {
                logical.push_str(rest);
                logical.push(' ');
                if i + 1 < lines.len() {
                    continue;
                }
            }
            None => logical.push_str(line),
        }
        let entry = std::mem::take(&mut logical);
        if entry.trim().is_empty() {
            continue;
        }
        let locked = parse_entry(entry.trim()).with_context(|| format!("{LOCK} line {first}"))?;
        ensure!(!out.iter().any(|l| l.name == locked.name), "{LOCK} line {first}: {} is listed twice", locked.name);
        out.push(locked);
        ensure!(out.len() <= MAX_PACKAGES, "{LOCK} lists more than {MAX_PACKAGES} packages");
    }
    Ok(out)
}

fn parse_entry(entry: &str) -> anyhow::Result<Locked> {
    let mut words = entry.split_whitespace();
    let spec = words.next().unwrap_or_default();
    if spec.starts_with('-') {
        if ["-i", "--index-url", "--extra-index-url", "-f", "--find-links", "--trusted-host"]
            .iter()
            .any(|o| spec == *o || spec.starts_with(&format!("{o}=")))
        {
            bail!(
                "`{spec}` names where packages come from, which only your config does ([prepare.python] index and \
                 hosts): remove it from the lock"
            );
        }
        bail!("`{spec}` is not supported: molt env prepare reads one file of `name==version --hash=...` lines");
    }
    ensure!(
        !entry.contains(';'),
        "environment markers (`;`) are not supported yet: compile the lock for this platform without them"
    );
    let (name, version) = spec.split_once("==").with_context(|| {
        format!("`{spec}` is not pinned: every package must be `name==version`, as pip-compile writes it")
    })?;
    let name = name.split_once('[').map_or(name, |(n, _)| n);
    ensure!(valid_name(name), "`{name}` is not a package name");
    ensure!(
        !version.is_empty()
            && !version.starts_with('=')
            && version.bytes().all(|c| c.is_ascii_alphanumeric() || b".+!_-".contains(&c)),
        "`{spec}` is not pinned to one version"
    );
    let mut hashes = Vec::new();
    let mut rest = words.peekable();
    while let Some(word) = rest.next() {
        let value = match word.strip_prefix("--hash=") {
            Some(value) => value.to_owned(),
            None if word == "--hash" => rest.next().unwrap_or_default().to_owned(),
            None => bail!("`{word}` is not supported after {spec}: only --hash=sha256:... may follow"),
        };
        let digest = value.strip_prefix("sha256:").with_context(|| {
            format!("--hash={value}: only sha256 hashes are supported (pip-compile --generate-hashes writes them)")
        })?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_hexdigit()),
            "--hash={value} is not a SHA-256 digest"
        );
        hashes.push(digest.to_ascii_lowercase());
    }
    ensure!(
        !hashes.is_empty(),
        "{spec} has no --hash: generate the lock with hashes (pip-compile --generate-hashes), so that what is \
         downloaded can be checked"
    );
    Ok(Locked { name: normalize(name), version: version.to_owned(), hashes })
}

// ---------------------------------------------------------------------------
// The index.

/// One file an index page lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexFile {
    pub filename: String,
    pub url: Url,
    pub sha256: Option<String>,
}

/// The files of a simple-API page at `page`, from its JSON (PEP 691) or HTML (PEP 503).
pub fn parse_page(page: &Url, json: bool, body: &str) -> anyhow::Result<Vec<IndexFile>> {
    let mut out = Vec::new();
    if json {
        #[derive(Deserialize)]
        struct Page {
            files: Vec<File>,
        }
        #[derive(Deserialize)]
        struct File {
            filename: String,
            url: String,
            #[serde(default)]
            hashes: BTreeMap<String, String>,
        }
        let parsed: Page = serde_json::from_str(body).context("the index page is not simple-API JSON")?;
        for file in parsed.files {
            let mut url = page.join(&file.url).with_context(|| format!("the index's URL `{}`", file.url))?;
            url.set_fragment(None);
            let sha256 = file.hashes.get("sha256").map(|h| h.to_ascii_lowercase());
            out.push(IndexFile { filename: file.filename, url, sha256 });
        }
        return Ok(out);
    }
    let lower = body.to_ascii_lowercase();
    let mut at = 0;
    while let Some(start) = lower[at..].find("<a ").map(|i| i + at) {
        let Some(end) = lower[start..].find('>').map(|i| i + start) else { break };
        let tag = &body[start..end];
        let close = lower[end..].find("</a>").map_or(body.len(), |i| i + end);
        let text = unescape(body[end + 1..close].trim());
        at = close.min(body.len());
        let Some(href) = attribute(tag, "href") else { continue };
        let mut url = page.join(&unescape(&href)).with_context(|| format!("the index's link `{href}`"))?;
        let sha256 = url.fragment().and_then(|f| f.strip_prefix("sha256=")).map(|h| h.to_ascii_lowercase());
        url.set_fragment(None);
        let filename = if text.is_empty() {
            url.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default().to_owned()
        } else {
            text
        };
        out.push(IndexFile { filename, url, sha256 });
    }
    Ok(out)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| i + from) {
        from = i + name.len();
        let before = lower[..i].chars().next_back();
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let rest = tag[from..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest.chars().next()?;
        if quote == '"' || quote == '\'' {
            let value = &rest[1..];
            return value.find(quote).map(|end| value[..end].to_owned());
        }
        return Some(rest.split(|c: char| c.is_whitespace() || c == '>').next()?.to_owned());
    }
    None
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

/// What a file is, by its name.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Dist {
    /// Its `python-abi-platform` tags.
    Wheel {
        name: String,
        version: String,
        tags: Vec<String>,
    },
    Sdist {
        name: String,
        version: String,
    },
}

fn dist(filename: &str) -> Option<Dist> {
    if let Some(stem) = filename.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() != 5 && parts.len() != 6 {
            return None;
        }
        let n = parts.len();
        let mut tags = Vec::new();
        for py in parts[n - 3].split('.') {
            for abi in parts[n - 2].split('.') {
                for plat in parts[n - 1].split('.') {
                    tags.push(format!("{py}-{abi}-{plat}"));
                }
            }
        }
        return Some(Dist::Wheel { name: normalize(parts[0]), version: parts[1].to_owned(), tags });
    }
    let stem = filename.strip_suffix(".tar.gz").or_else(|| filename.strip_suffix(".zip"))?;
    let (name, version) = stem.rsplit_once('-')?;
    Some(Dist::Sdist { name: normalize(name), version: version.to_owned() })
}

/// The file to install for `locked`: the wheel whose tag this Python
/// prefers most, else a source distribution when `may_build`. Only files of
/// the package whose hash is in the lock, or is not given and will be checked
/// when downloaded, are considered.
fn choose<'a>(
    locked: &Locked,
    files: &'a [IndexFile],
    tags: &[String],
    may_build: bool,
) -> anyhow::Result<(&'a IndexFile, bool)> {
    let rank: BTreeMap<&str, usize> = tags.iter().enumerate().map(|(i, t)| (t.as_str(), i)).collect();
    let mut best: Option<(usize, &IndexFile)> = None;
    let mut sdist = None;
    let mut any = false;
    for file in files {
        let Some(kind) = dist(&file.filename) else { continue };
        let (name, version) = match &kind {
            Dist::Wheel { name, version, .. } | Dist::Sdist { name, version } => (name, version),
        };
        if *name != locked.name {
            continue;
        }
        let ours = match &file.sha256 {
            Some(hash) => locked.hashes.contains(hash),
            None => version.eq_ignore_ascii_case(&locked.version),
        };
        if !ours {
            continue;
        }
        any = true;
        match kind {
            Dist::Wheel { tags, .. } => {
                if let Some(r) = tags.iter().filter_map(|t| rank.get(t.as_str()).copied()).min() {
                    if best.is_none_or(|(b, _)| r < b) {
                        best = Some((r, file));
                    }
                }
            }
            Dist::Sdist { .. } => sdist = sdist.or(Some(file)),
        }
    }
    if let Some((_, file)) = best {
        return Ok((file, false));
    }
    let wanted = format!("{}=={}", locked.name, locked.version);
    match sdist {
        Some(file) if may_build => Ok((file, true)),
        Some(_) => bail!(
            "{wanted} has no wheel for this Python, only a source distribution, whose build scripts would run: add \
             \"{}\" to [prepare.python] scripts in your user config to allow it",
            locked.name
        ),
        None if any => bail!("{wanted} has no wheel for this Python ({}) among the lock's files", top_tag(tags)),
        None => bail!("the index lists no file of {wanted} with a hash the lock gives"),
    }
}

fn top_tag(tags: &[String]) -> &str {
    tags.first().map_or("no tags", String::as_str)
}

// ---------------------------------------------------------------------------
// The network.

/// Requests only to the allowed origins, redirects included.
pub struct Net {
    client: reqwest::Client,
    allowed: Vec<String>,
    /// The wait before the second attempt, doubled after.
    backoff: Duration,
}

/// How one attempt failed.
enum Failure {
    /// The connection broke, the server erred or the file came out wrong: try again.
    Again(anyhow::Error),
    /// Trying again would not help.
    Final(anyhow::Error),
}

impl Net {
    pub fn new(allowed: Vec<String>, backoff: Duration) -> anyhow::Result<Self> {
        let origins = allowed.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
            }
            let origin = attempt.url().origin().ascii_serialization();
            if origins.contains(&origin) {
                attempt.follow()
            } else {
                attempt.error(refused(&origin))
            }
        });
        let client = reqwest::Client::builder()
            .redirect(policy)
            .user_agent(concat!("molt/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { client, allowed, backoff })
    }

    fn check(&self, url: &Url) -> anyhow::Result<()> {
        let origin = url.origin().ascii_serialization();
        ensure!(self.allowed.contains(&origin), "{}", refused(&origin));
        Ok(())
    }

    /// The files the index lists for `name`.
    pub async fn page(&self, index: &Url, name: &str) -> anyhow::Result<Vec<IndexFile>> {
        let url = index.join(&format!("{name}/"))?;
        self.check(&url)?;
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(self.backoff * 2u32.pow(attempt - 1)).await;
            }
            match self.page_once(&url).await {
                Ok(files) => return Ok(files),
                Err(Failure::Final(e)) => return Err(e),
                Err(Failure::Again(e)) => last = Some(e),
            }
        }
        Err(last.expect("tried").context(format!("{url}: tried {ATTEMPTS} times")))
    }

    async fn page_once(&self, url: &Url) -> Result<Vec<IndexFile>, Failure> {
        let response = self
            .client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/vnd.pypi.simple.v1+json, text/html;q=0.1")
            .send()
            .await
            .map_err(|e| sent(url, e))?;
        let response = status(url, response)?;
        let page = response.url().clone();
        let json = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("json"));
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| Failure::Again(anyhow::anyhow!("{url}: {e}")))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_PAGE {
                return Err(Failure::Final(anyhow::anyhow!("{url}: the index page exceeds 64 MiB")));
            }
        }
        let body = String::from_utf8_lossy(&body);
        parse_page(&page, json, &body).map_err(Failure::Final)
    }

    /// Download `file` into `cache` as `hash`'s entry, trying again when the
    /// transfer breaks; `hashes` are the ones the lock allows.
    async fn download(&self, file: &IndexFile, hashes: &[String], cache: &Cache) -> anyhow::Result<Stored> {
        self.check(&file.url)?;
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(self.backoff * 2u32.pow(attempt - 1)).await;
            }
            match self.download_once(file, hashes, cache).await {
                Ok(stored) => return Ok(stored),
                Err(Failure::Final(e)) => return Err(e),
                Err(Failure::Again(e)) => last = Some(e),
            }
        }
        Err(last.expect("tried").context(format!("{}: tried {ATTEMPTS} times", file.url)))
    }

    async fn download_once(&self, file: &IndexFile, hashes: &[String], cache: &Cache) -> Result<Stored, Failure> {
        let url = &file.url;
        let response = self.client.get(url.clone()).send().await.map_err(|e| sent(url, e))?;
        let mut response = status(url, response)?;
        let room = cache.room(response.content_length().unwrap_or(0)).map_err(Failure::Final)?;
        let mut tmp = cache.temp().map_err(Failure::Final)?;
        let mut digest = Sha256::new();
        let mut bytes = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| Failure::Again(anyhow::anyhow!("{url}: {e}")))? {
            bytes += chunk.len() as u64;
            if bytes > room {
                return Err(Failure::Final(anyhow::anyhow!(
                    "{url} does not fit in the download cache's quota of {} MiB: raise [prepare.python] max_mb",
                    cache.max_bytes >> 20
                )));
            }
            digest.update(&chunk);
            tmp.write_all(&chunk).map_err(|e| Failure::Final(e.into()))?;
        }
        let hash = hex::encode(digest.finalize());
        if !hashes.contains(&hash) || file.sha256.as_ref().is_some_and(|h| *h != hash) {
            return Err(Failure::Again(anyhow::anyhow!(
                "{url} has SHA-256 {hash}, which the lock does not list for this package"
            )));
        }
        let path = cache.store(tmp, &hash, &file.filename).map_err(Failure::Final)?;
        Ok(Stored { path, sha256: hash, bytes, downloaded: true })
    }
}

fn refused(origin: &str) -> String {
    format!(
        "{origin} is not an origin [prepare.python] allows: add it to hosts in your user config if it is a \
         registry or mirror you trust"
    )
}

fn sent(url: &Url, e: reqwest::Error) -> Failure {
    let message = anyhow::Error::from(e).context(url.to_string());
    let chain = format!("{message:#}");
    if chain.contains("not an origin") || chain.contains("redirects") {
        Failure::Final(anyhow::anyhow!("{chain}"))
    } else {
        Failure::Again(anyhow::anyhow!("{chain}"))
    }
}

fn status(url: &Url, response: reqwest::Response) -> Result<reqwest::Response, Failure> {
    let code = response.status();
    if code.is_success() {
        return Ok(response);
    }
    let e = anyhow::anyhow!("{url}: the server answered {code}");
    Err(if code.is_server_error() || code.as_u16() == 429 { Failure::Again(e) } else { Failure::Final(e) })
}

// ---------------------------------------------------------------------------
// The cache.

/// A file in the cache.
#[derive(Clone, Debug)]
struct Stored {
    path: PathBuf,
    sha256: String,
    bytes: u64,
    downloaded: bool,
}

/// Downloaded files by SHA-256, as `files/<sha256>/<filename>`, within a quota.
pub struct Cache {
    root: PathBuf,
    max_bytes: u64,
    /// Entries the current preparation uses, never evicted for room.
    keep: std::sync::Mutex<HashSet<String>>,
}

impl Cache {
    /// The cache in `root`, without what an earlier, interrupted download left.
    pub fn open(root: &Path, max_mb: u64) -> anyhow::Result<Self> {
        for dir in ["files", "tmp"] {
            std::fs::create_dir_all(root.join(dir)).with_context(|| format!("creating {}", root.display()))?;
        }
        for entry in std::fs::read_dir(root.join("tmp"))?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        let root = root.canonicalize()?;
        Ok(Self { root, max_bytes: max_mb << 20, keep: Default::default() })
    }

    /// The directory of entries, which the install reads.
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    fn temp(&self) -> anyhow::Result<tempfile::NamedTempFile> {
        Ok(tempfile::NamedTempFile::new_in(self.root.join("tmp"))?)
    }

    /// The entry of `sha256` named `filename` when it is there and whole.
    fn lookup(&self, sha256: &str, filename: &str) -> Option<Stored> {
        let path = self.files().join(sha256).join(filename);
        let bytes = std::fs::read(&path).ok()?;
        if hex::encode(Sha256::digest(&bytes)) != sha256 {
            let _ = std::fs::remove_dir_all(self.files().join(sha256));
            return None;
        }
        self.keep.lock().unwrap().insert(sha256.to_owned());
        // Its time is when it was last used, which eviction goes by.
        let _ = std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(SystemTime::now()));
        Some(Stored { path, sha256: sha256.to_owned(), bytes: bytes.len() as u64, downloaded: false })
    }

    /// The entries, with their size and when each was last used.
    fn entries(&self) -> Vec<(String, u64, SystemTime)> {
        let mut out = Vec::new();
        for dir in std::fs::read_dir(self.files()).into_iter().flatten().flatten() {
            let mut bytes = 0;
            let mut used = UNIX_EPOCH;
            for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                if let Ok(meta) = file.metadata() {
                    bytes += meta.len();
                    used = used.max(meta.modified().unwrap_or(UNIX_EPOCH));
                }
            }
            out.push((dir.file_name().to_string_lossy().into_owned(), bytes, used));
        }
        out
    }

    /// Room for a file of `need` bytes, made by evicting the entries used
    /// least recently that this preparation does not use: the most a
    /// download may then write.
    fn room(&self, need: u64) -> anyhow::Result<u64> {
        let keep = self.keep.lock().unwrap().clone();
        let mut entries = self.entries();
        let mut used: u64 = entries.iter().map(|(_, b, _)| b).sum();
        entries.sort_by_key(|(_, _, at)| *at);
        for (hash, bytes, _) in entries {
            if used.saturating_add(need) <= self.max_bytes {
                break;
            }
            if keep.contains(&hash) {
                continue;
            }
            if std::fs::remove_dir_all(self.files().join(&hash)).is_ok() {
                used -= bytes;
            }
        }
        ensure!(
            used.saturating_add(need) <= self.max_bytes,
            "the download cache is full: the files this lock needs exceed its quota of {} MiB; raise \
             [prepare.python] max_mb",
            self.max_bytes >> 20
        );
        Ok(self.max_bytes - used)
    }

    /// Make the verified `tmp` the entry of `sha256`, all at once.
    fn store(&self, tmp: tempfile::NamedTempFile, sha256: &str, filename: &str) -> anyhow::Result<PathBuf> {
        ensure!(
            !filename.is_empty() && !filename.contains('/') && !filename.starts_with('.'),
            "the index names a file `{filename}`, which is not a plain file name"
        );
        tmp.as_file().sync_all()?;
        let dir = self.files().join(sha256);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(filename);
        tmp.persist(&path).map_err(|e| e.error).with_context(|| format!("writing {}", path.display()))?;
        self.keep.lock().unwrap().insert(sha256.to_owned());
        Ok(path)
    }
}

// ---------------------------------------------------------------------------
// The environment.

/// What `molt env prepare` writes under the data dir, and the user's config for it.
#[derive(Clone, Debug)]
pub struct Deps {
    /// `<data dir>/deps`.
    pub dir: PathBuf,
    pub python: PythonPrepare,
}

/// What a complete environment holds, written last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub manager: String,
    pub lock: String,
    pub lock_sha256: String,
    pub python: PathBuf,
    pub index: String,
    pub files: Vec<FileRecord>,
    /// The packages built from source, whose build scripts ran.
    pub built: Vec<String>,
    pub at_ms: u64,
}

/// One file of a preparation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub package: String,
    pub version: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    /// Downloaded now, rather than found in the cache.
    pub downloaded: bool,
}

/// One line of `records.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub at_ms: u64,
    pub workspace: PathBuf,
    pub manager: String,
    pub lock: String,
    pub lock_sha256: String,
    pub env: PathBuf,
    /// `prepared`, `unchanged` or `failed`.
    pub outcome: String,
    /// Where it failed: `lock`, `sandbox`, `venv`, `resolve`, `download` or `install`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub built: Vec<String>,
    pub ms: u64,
}

/// The last preparation of a workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Last {
    workspace: PathBuf,
    lock_sha256: String,
    env: PathBuf,
}

/// The outcome of `molt env prepare`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prepared {
    pub env: PathBuf,
    pub lock_sha256: String,
    /// It was prepared before, and nothing was done.
    pub unchanged: bool,
    pub files: Vec<FileRecord>,
    pub built: Vec<String>,
    pub ms: u64,
}

impl Prepared {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let lock = &self.lock_sha256[..12];
        if self.unchanged {
            let _ = writeln!(out, "{LOCK} (lock {lock}) is already prepared in {}", self.env.display());
            return out;
        }
        let _ = writeln!(out, "python: {LOCK} (lock {lock}), {} packages", self.files.len());
        for f in &self.files {
            let how = if f.downloaded { "downloaded" } else { "cached    " };
            let from = Url::parse(&f.url).map(|u| u.origin().ascii_serialization()).unwrap_or_default();
            let _ = writeln!(out, "  {how} {} {}: {} from {from} ({} bytes)", f.package, f.version, f.file, f.bytes);
        }
        if !self.built.is_empty() {
            let _ = writeln!(
                out,
                "  built from source, running their build scripts in the sandbox: {}",
                self.built.join(", ")
            );
        }
        let _ = writeln!(
            out,
            "prepared in {} ({} ms): commands get it read-only, without network",
            self.env.display(),
            self.ms
        );
        out
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn workspace_key(workspace: &Path) -> String {
    hex::encode(Sha256::digest(workspace.as_os_str().as_encoded_bytes()))[..16].to_owned()
}

/// The lock of `workspace`, its bytes and digest, if it has one.
fn read_lock(workspace: &Path) -> anyhow::Result<Option<(Vec<u8>, String)>> {
    let path = workspace.join(LOCK);
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    ensure!(meta.is_file() && meta.len() <= MAX_LOCK, "{} is not a file of at most 1 MiB", path.display());
    let bytes = std::fs::read(&path)?;
    let digest = hex::encode(Sha256::digest(&bytes));
    Ok(Some((bytes, digest)))
}

/// The first `python3` on `path`, resolved.
fn host_python(path: Option<&std::ffi::OsStr>) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let found = std::env::split_paths(path.unwrap_or_default())
        .filter(|d| d.is_absolute())
        .map(|d| d.join("python3"))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .context("python3 is not on PATH: install Python 3, or put it on PATH")?;
    Ok(found.canonicalize()?)
}

impl Deps {
    pub fn new(data_dir: &Path, python: &PythonPrepare) -> Self {
        Self { dir: data_dir.join("deps"), python: python.clone() }
    }

    fn python_dir(&self) -> PathBuf {
        self.dir.join("python")
    }

    /// Where the environment for this lock, interpreter and config lives.
    fn env_dir(&self, lock: &[u8], python: &Path) -> PathBuf {
        let mut h = Sha256::new();
        let mut put = |label: &str, bytes: &[u8]| {
            h.update((label.len() as u64).to_le_bytes());
            h.update(label.as_bytes());
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(bytes);
        };
        put("lock", lock);
        put("python", python.as_os_str().as_encoded_bytes());
        let stamp = std::fs::metadata(python)
            .ok()
            .map(|m| format!("{} {:?}", m.len(), m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok())));
        put("python stamp", stamp.unwrap_or_default().as_bytes());
        put("index", self.python.index.as_bytes());
        let scripts: BTreeSet<String> = self.python.scripts.iter().map(|s| normalize(s)).collect();
        put("scripts", format!("{scripts:?}").as_bytes());
        let key = hex::encode(h.finalize());
        self.python_dir().join("envs").join(&key[..16])
    }

    fn last_path(&self, workspace: &Path) -> PathBuf {
        self.python_dir().join("workspaces").join(format!("{}.json", workspace_key(workspace)))
    }

    fn last(&self, workspace: &Path) -> Option<Last> {
        let last: Last = serde_json::from_slice(&std::fs::read(self.last_path(workspace)).ok()?).ok()?;
        (last.workspace == workspace).then_some(last)
    }

    /// The complete environment for the lock `workspace` has now, if there is one.
    fn current(&self, workspace: &Path, path: Option<&std::ffi::OsStr>) -> Option<(PathBuf, String)> {
        let (lock, digest) = read_lock(workspace).ok()??;
        let python = host_python(path).ok()?;
        let env = self.env_dir(&lock, &python);
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(env.join(MANIFEST)).ok()?).ok()?;
        (manifest.lock_sha256 == digest).then_some((env, digest))
    }

    /// The profile that gives the commands of `workspace` its prepared
    /// environment, read-only, when its lock has one; `path` is the host's PATH.
    pub fn profile(&self, workspace: &Path, path: Option<&std::ffi::OsStr>) -> Option<EnvProfile> {
        let (env, _) = self.current(workspace, path)?;
        let shown = env.display().to_string();
        Some(EnvProfile {
            detect: vec![LOCK.to_owned()],
            read_only: vec![env],
            env: BTreeMap::from([
                ("VIRTUAL_ENV".to_owned(), shown.clone()),
                ("PATH".to_owned(), format!("{shown}/bin:$PATH")),
            ]),
            probes: vec![env_profile::Probe {
                command: "python -c 'import sys; sys.exit(sys.prefix == sys.base_prefix)'".to_owned(),
                fix: "The prepared environment does not run in the sandbox: run `molt env prepare` again".to_owned(),
            }],
            ..EnvProfile::default()
        })
    }

    /// Add [`Deps::profile`] to the user's profiles, as [`PROFILE`].
    pub fn add_profile(&self, profiles: &mut BTreeMap<String, EnvProfile>, workspace: &Path) {
        match self.profile(workspace, std::env::var_os("PATH").as_deref()) {
            Some(profile) => {
                profiles.insert(PROFILE.to_owned(), profile);
            }
            None => {
                profiles.remove(PROFILE);
            }
        }
    }

    /// The readiness check's word on the preparation of `workspace`: ready
    /// when its lock is prepared, blocked when the lock changed since it was,
    /// and nothing when it was never prepared (the dependency file checks
    /// speak then).
    pub fn readiness(&self, workspace: &Path) -> Option<Item> {
        let (_, digest) = read_lock(workspace).ok()??;
        let item = |status, detail: String, fix: Option<String>| Item {
            kind: Kind::Dependency,
            name: "python".to_owned(),
            status,
            detail,
            fix,
            ms: 0,
        };
        if let Some((env, _)) = self.current(workspace, std::env::var_os("PATH").as_deref()) {
            return Some(item(
                Status::Ready,
                format!("{LOCK} (lock {}) is prepared: commands get it from {}", &digest[..12], env.display()),
                None,
            ));
        }
        let last = self.last(workspace)?;
        let why = if last.lock_sha256 == digest {
            "its prepared environment is gone or was made for another Python or config".to_owned()
        } else {
            format!(
                "{LOCK} changed since it was prepared (lock {}, prepared {})",
                &digest[..12],
                &last.lock_sha256[..12]
            )
        };
        Some(item(Status::Blocked, why, Some("run `molt env prepare` to prepare it again".to_owned())))
    }

    fn record(&self, record: &Record) {
        let path = self.dir.join("records.jsonl");
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(_) => return,
        };
        let written = std::fs::create_dir_all(&self.dir).and_then(|()| {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            f.write_all(format!("{line}\n").as_bytes())
        });
        if let Err(e) = written {
            tracing::warn!("could not record the preparation in {}: {e}", path.display());
        }
    }
}

/// What to prepare.
pub struct Options<'a> {
    /// The canonical workspace.
    pub workspace: &'a Path,
    pub deps: &'a Deps,
    /// The user's profiles, over the built-in ones, for the toolchain mounts.
    pub user: &'a BTreeMap<String, EnvProfile>,
    /// The user's `--sandbox-policy`, or the default. Its network and
    /// passed variables are not used.
    pub policy: SandboxPolicy,
    /// The wait before a download is tried again, doubled after.
    pub backoff: Duration,
}

/// A failure at one stage of a preparation.
struct Failed {
    stage: &'static str,
    error: anyhow::Error,
    files: Vec<FileRecord>,
}

fn at(stage: &'static str) -> impl Fn(anyhow::Error) -> Failed {
    move |error| Failed { stage, error, files: Vec::new() }
}

/// Prepare the dependencies of the workspace in `opts`.
pub async fn prepare(opts: Options<'_>) -> anyhow::Result<Prepared> {
    let started = Instant::now();
    let workspace = opts.workspace;
    let deps = opts.deps;
    deps.python.validate().context("in [prepare.python]")?;
    let (lock, digest) = read_lock(workspace)?.with_context(|| {
        format!(
            "{} has no {LOCK}: molt env prepare reads a hash-pinned one (pip-compile --generate-hashes)",
            workspace.display()
        )
    })?;
    let python = host_python(std::env::var_os("PATH").as_deref())?;
    let env = deps.env_dir(&lock, &python);
    std::fs::create_dir_all(deps.python_dir())?;
    let guard = std::fs::File::create(deps.python_dir().join("lock"))?;
    guard.try_lock().map_err(|_| anyhow::anyhow!("another molt env prepare is running with this data dir"))?;
    let record = |outcome: &str, failed: Option<&Failed>, files: Vec<FileRecord>, built: Vec<String>| {
        deps.record(&Record {
            at_ms: now_ms(),
            workspace: workspace.to_path_buf(),
            manager: "pip".to_owned(),
            lock: LOCK.to_owned(),
            lock_sha256: digest.clone(),
            env: env.clone(),
            outcome: outcome.to_owned(),
            stage: failed.map(|f| f.stage.to_owned()),
            error: failed.map(|f| format!("{:#}", f.error)),
            files,
            built,
            ms: started.elapsed().as_millis() as u64,
        })
    };
    let last = Last { workspace: workspace.to_path_buf(), lock_sha256: digest.clone(), env: env.clone() };
    let keep_last = || -> anyhow::Result<()> {
        let path = deps.last_path(workspace);
        std::fs::create_dir_all(path.parent().expect("has a parent"))?;
        std::fs::write(&path, serde_json::to_vec_pretty(&last)?)?;
        Ok(())
    };
    if let Ok(bytes) = std::fs::read(env.join(MANIFEST)) {
        if let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) {
            record("unchanged", None, Vec::new(), Vec::new());
            keep_last()?;
            return Ok(Prepared {
                env,
                lock_sha256: digest,
                unchanged: true,
                files: manifest.files,
                built: manifest.built,
                ms: started.elapsed().as_millis() as u64,
            });
        }
    }
    let result = build(&opts, &lock, &python, &env).await;
    match result {
        Ok((files, built)) => {
            let manifest = Manifest {
                manager: "pip".to_owned(),
                lock: LOCK.to_owned(),
                lock_sha256: digest.clone(),
                python,
                index: deps.python.index.clone(),
                files: files.clone(),
                built: built.clone(),
                at_ms: now_ms(),
            };
            let tmp = tempfile::NamedTempFile::new_in(&env)?;
            std::fs::write(tmp.path(), serde_json::to_vec_pretty(&manifest)?)?;
            tmp.persist(env.join(MANIFEST)).map_err(|e| e.error)?;
            record("prepared", None, files.clone(), built.clone());
            keep_last()?;
            Ok(Prepared {
                env,
                lock_sha256: digest,
                unchanged: false,
                files,
                built,
                ms: started.elapsed().as_millis() as u64,
            })
        }
        Err(failed) => {
            record("failed", Some(&failed), failed.files.clone(), Vec::new());
            let stage = failed.stage;
            let mut error = failed.error.context(format!("preparing {LOCK} failed at the {stage} step"));
            if stage == "install" {
                error = error.context(
                    "the install was not retried, and the environment was left unfinished: fix the cause and run \
                     `molt env prepare` again, which starts it over",
                );
            }
            Err(error)
        }
    }
}

/// Make the environment at `env` from nothing: the virtual environment,
/// the files, then the install.
async fn build(
    opts: &Options<'_>,
    lock: &[u8],
    python: &Path,
    env: &Path,
) -> Result<(Vec<FileRecord>, Vec<String>), Failed> {
    let deps = opts.deps;
    let workspace = opts.workspace;
    let text = std::str::from_utf8(lock).map_err(|_| at("lock")(anyhow::anyhow!("{LOCK} is not UTF-8")))?;
    let locked = parse_lock(text).map_err(at("lock"))?;
    if locked.is_empty() {
        return Err(at("lock")(anyhow::anyhow!("{LOCK} lists no packages, so there is nothing to prepare")));
    }
    let cache = Cache::open(&deps.python_dir().join("cache"), deps.python.max_mb).map_err(at("download"))?;
    if env.exists() {
        // Unfinished: an earlier preparation stopped or failed. Never resumed.
        std::fs::remove_dir_all(env).map_err(|e| at("venv")(e.into()))?;
    }
    std::fs::create_dir_all(env).map_err(|e| at("venv")(e.into()))?;
    let env = env.canonicalize().map_err(|e| at("venv")(e.into()))?;

    let tmp = tempfile::Builder::new().prefix("molt-env-prepare-").tempdir().map_err(|e| at("sandbox")(e.into()))?;
    let scratch = tmp.path().join("scratch");
    let mut policy = opts.policy.clone();
    // Nothing reaches the install but the sandbox's own variables, and it has no network.
    policy.network = false;
    policy.environment.clear();
    let mut user = opts.user.clone();
    user.remove(PROFILE);
    let profiles = env_profile::profiles(&user);
    let var = |name: &str| std::env::var_os(name);
    for applied in env_profile::apply(&mut policy, workspace, &[workspace, &scratch], &profiles, &var) {
        for note in applied.notes {
            tracing::warn!(profile = %applied.name, "{note}");
        }
    }
    let files_dir = cache.files();
    policy.read_only.retain(|p| !files_dir.starts_with(p) && !p.starts_with(&files_dir));
    policy.read_only.push(files_dir.clone());
    policy.writable = vec![env.clone()];
    let sandbox = crate::doctor::Sandbox::start(workspace, &scratch, policy)
        .await
        .map_err(|item| at("sandbox")(anyhow::anyhow!("{}", item.detail)))?;
    let result = async {
        let shown = env.display().to_string();
        let tags_file = env.join("molt-tags.json");
        let venv = format!(
            "{} -m venv {} && {}/bin/python -c 'import json, sys; from pip._vendor.packaging.tags import sys_tags; \
             json.dump([str(t) for t in sys_tags()], open(sys.argv[1], \"w\"))' {}",
            shell_quote(&python.display().to_string()),
            shell_quote(&shown),
            shell_quote(&shown),
            shell_quote(&tags_file.display().to_string()),
        );
        let ran = sandbox.run(&venv, VENV_MS).await.map_err(at("venv"))?;
        if !ran.success() {
            return Err(at("venv")(anyhow::anyhow!(
                "creating the virtual environment failed: {}",
                crate::readiness::last_line(&format!("{}\n{}", ran.stdout, ran.stderr))
            )));
        }
        let tags: Vec<String> = std::fs::read(&tags_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| at("venv")(anyhow::anyhow!("the new environment did not say which wheels it takes")))?;
        let _ = std::fs::remove_file(&tags_file);

        let index = deps.python.index_url().map_err(at("resolve"))?;
        let net = Net::new(deps.python.origins().map_err(at("resolve"))?, opts.backoff).map_err(at("resolve"))?;
        let mut chosen = Vec::new();
        for package in &locked {
            let page = net.page(&index, &package.name).await.map_err(at("resolve"))?;
            let (file, from_source) =
                choose(package, &page, &tags, deps.python.may_build(&package.name)).map_err(at("resolve"))?;
            // Before anything is downloaded, so a lock with one file from elsewhere downloads nothing.
            net.check(&file.url).map_err(at("resolve"))?;
            chosen.push((package, file.clone(), from_source));
        }
        // What this lock uses already is not evicted to make room for the rest.
        cache.keep.lock().unwrap().extend(chosen.iter().filter_map(|(_, f, _)| f.sha256.clone()));
        let mut files = Vec::new();
        let mut install = String::new();
        let mut built = Vec::new();
        for (package, file, from_source) in &chosen {
            let hit = file.sha256.as_deref().and_then(|h| cache.lookup(h, &file.filename));
            let stored = match hit {
                Some(stored) => stored,
                None => net.download(file, &package.hashes, &cache).await.map_err(|error| Failed {
                    stage: "download",
                    error,
                    files: files.clone(),
                })?,
            };
            files.push(FileRecord {
                package: package.name.clone(),
                version: package.version.clone(),
                file: file.filename.clone(),
                url: file.url.to_string(),
                sha256: stored.sha256.clone(),
                bytes: stored.bytes,
                downloaded: stored.downloaded,
            });
            let _ = writeln!(install, "{} --hash=sha256:{}", stored.path.display(), stored.sha256);
            if *from_source {
                built.push(package.name.clone());
            }
        }
        let list = env.join("molt-install.txt");
        std::fs::write(&list, &install).map_err(|e| Failed {
            stage: "install",
            error: e.into(),
            files: files.clone(),
        })?;
        let only_binary = if built.is_empty() { " --only-binary :all:" } else { "" };
        let command = format!(
            "{}/bin/python -m pip install --no-index --no-deps --require-hashes --no-build-isolation --no-cache-dir \
             --disable-pip-version-check --no-input{only_binary} -r {}",
            shell_quote(&shown),
            shell_quote(&list.display().to_string()),
        );
        let ran = sandbox.run(&command, INSTALL_MS).await.map_err(|error| Failed {
            stage: "install",
            error,
            files: files.clone(),
        })?;
        if !ran.success() {
            let said = tail(&format!("{}\n{}", ran.stdout, ran.stderr), 20);
            let how = match (ran.timed_out, ran.exit_code) {
                (true, _) => "timed out".to_owned(),
                (false, Some(code)) => format!("exit code {code}"),
                (false, None) => "was killed".to_owned(),
            };
            return Err(Failed {
                stage: "install",
                error: anyhow::anyhow!("pip install {how}:\n{said}"),
                files: files.clone(),
            });
        }
        Ok((files, built))
    }
    .await;
    sandbox.drop_fork().await;
    result
}

/// The last `n` lines of `text` that say something.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_hash_pinned_lock_is_read_and_anything_that_would_look_elsewhere_is_refused() {
        let text = format!(
            "# compiled\nRequests[socks]==2.31.0 \\\n    --hash=sha256:{A} \\\n    --hash=sha256:{}\n    # via x\n\
             zope.interface==6.0 --hash=sha256:{B}\n",
            B.to_uppercase()
        );
        let locked = parse_lock(&text).unwrap();
        assert_eq!(locked.len(), 2);
        assert_eq!((locked[0].name.as_str(), locked[0].version.as_str()), ("requests", "2.31.0"));
        assert_eq!(locked[0].hashes, [A, B]);
        assert_eq!(locked[1].name, "zope-interface");
        for (text, says) in [
            ("demo>=1.0 --hash=sha256:".to_owned() + A, "not pinned"),
            ("demo==1.0".to_owned(), "has no --hash"),
            ("demo==1.0 --hash=md5:abc".to_owned(), "only sha256"),
            (format!("--index-url https://evil.example/simple\ndemo==1.0 --hash=sha256:{A}"), "only your config"),
            (format!("--extra-index-url=https://x\ndemo==1.0 --hash=sha256:{A}"), "only your config"),
            ("-r other.txt".to_owned(), "not supported"),
            (format!("demo==1.0 ; python_version < \"3.8\" --hash=sha256:{A}"), "markers"),
            (format!("demo==1.0 --hash=sha256:{A}\nDemo==1.1 --hash=sha256:{B}"), "listed twice"),
        ] {
            let err = format!("{:#}", parse_lock(&text).unwrap_err());
            assert!(err.contains(says), "{text}: {err}");
        }
    }

    #[test]
    fn index_pages_are_read_from_json_and_html_with_their_hashes() {
        let page = Url::parse("https://pypi.example/simple/demo/").unwrap();
        let json = format!(
            r#"{{"meta":{{"api-version":"1.0"}},"name":"demo","files":[{{"filename":"demo-1.0-py3-none-any.whl","url":"../../files/demo-1.0-py3-none-any.whl","hashes":{{"sha256":"{A}"}}}}]}}"#
        );
        let files = parse_page(&page, true, &json).unwrap();
        assert_eq!(files[0].url.as_str(), "https://pypi.example/files/demo-1.0-py3-none-any.whl");
        assert_eq!(files[0].sha256.as_deref(), Some(A));
        let html = format!(
            "<html><body><A HREF=\"https://cdn.example/d/demo-1.0.tar.gz#sha256={A}\" data-requires-python=\"&gt;=3\">\
             demo-1.0.tar.gz</a><br/><a class=x href='/p/demo-1.0-py3-none-any.whl?a=1&amp;b=2'></a></body></html>"
        );
        let files = parse_page(&page, false, &html).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].filename.as_str(), files[0].sha256.as_deref()), ("demo-1.0.tar.gz", Some(A)));
        assert_eq!(files[0].url.as_str(), "https://cdn.example/d/demo-1.0.tar.gz");
        assert_eq!(files[1].filename, "demo-1.0-py3-none-any.whl");
        assert_eq!(files[1].url.as_str(), "https://pypi.example/p/demo-1.0-py3-none-any.whl?a=1&b=2");
    }

    #[test]
    fn the_wheel_this_python_prefers_is_chosen_and_a_source_build_needs_the_users_grant() {
        let file = |name: &str, hash: Option<&str>| IndexFile {
            filename: name.to_owned(),
            url: Url::parse(&format!("https://f.example/{name}")).unwrap(),
            sha256: hash.map(str::to_owned),
        };
        let locked = Locked { name: "demo".into(), version: "1.0".into(), hashes: vec![A.into(), B.into()] };
        let tags: Vec<String> =
            ["cp313-cp313-manylinux_2_17_x86_64", "py3-none-any"].iter().map(|s| s.to_string()).collect();
        let files = [
            file("demo-1.0.tar.gz", Some(A)),
            file("demo-1.0-py3-none-any.whl", Some(B)),
            file("demo-1.0-cp313-cp313-manylinux2014_x86_64.manylinux_2_17_x86_64.whl", Some(A)),
            file("demo-1.0-cp313-cp313-win_amd64.whl", Some(B)),
            file("demo-1.0-cp313-cp313-manylinux_2_17_x86_64.whl", Some("c".repeat(64).as_str())),
        ];
        let (chosen, built) = choose(&locked, &files, &tags, false).unwrap();
        assert_eq!(chosen.filename, files[2].filename, "the most preferred tag, among the lock's hashes");
        assert!(!built);
        let (chosen, _) = choose(&locked, &files[..2], &tags, false).unwrap();
        assert_eq!(chosen.filename, "demo-1.0-py3-none-any.whl");
        let err = choose(&locked, &files[..1], &tags, false).unwrap_err().to_string();
        assert!(err.contains("build scripts") && err.contains("scripts"), "{err}");
        let (chosen, built) = choose(&locked, &files[..1], &tags, true).unwrap();
        assert!(built && chosen.filename == "demo-1.0.tar.gz");
        let err = choose(&locked, &files[3..4], &tags, true).unwrap_err().to_string();
        assert!(err.contains("no wheel for this Python"), "{err}");
        let err = choose(&locked, &files[4..], &tags, true).unwrap_err().to_string();
        assert!(err.contains("no file of demo==1.0 with a hash the lock gives"), "{err}");
    }

    #[test]
    fn the_config_allows_only_web_origins_without_credentials() {
        let mut python = PythonPrepare::default();
        python.validate().unwrap();
        assert_eq!(python.origins().unwrap(), ["https://pypi.org", "https://files.pythonhosted.org"]);
        python.index = "https://user:token@mirror.example/simple".into();
        assert!(format!("{:#}", python.validate().unwrap_err()).contains("user or password"));
        python.index = "file:///srv/simple".into();
        assert!(python.validate().is_err());
        python = PythonPrepare { hosts: vec!["https://cdn.example/files".into()], ..PythonPrepare::default() };
        assert!(format!("{:#}", python.validate().unwrap_err()).contains("has a path"));
        python = PythonPrepare { scripts: vec!["../x".into()], ..PythonPrepare::default() };
        assert!(python.validate().is_err());
    }

    /// A one-thread HTTP server: each request gets the next answer for its path.
    struct Server {
        origin: String,
        hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// What the server does for a request.
    #[derive(Clone)]
    enum Answer {
        Body(&'static str, Vec<u8>),
        Redirect(String),
        /// Say this length, send half, and close.
        Cut(Vec<u8>),
        /// Send half, then wait without end.
        Stall(Vec<u8>),
    }

    impl Server {
        fn start(routes: Vec<(&str, Vec<Answer>)>) -> Self {
            use std::io::{BufRead, BufReader};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let routes: BTreeMap<String, std::collections::VecDeque<Answer>> =
                routes.into_iter().map(|(p, a)| (p.to_owned(), a.into())).collect();
            let routes = std::sync::Arc::new(std::sync::Mutex::new(routes));
            let seen = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let (routes, seen) = (routes.clone(), seen.clone());
                    // One thread each, so a stalled answer holds up no other.
                    std::thread::spawn(move || {
                        let mut line = String::new();
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        if reader.read_line(&mut line).is_err() {
                            return;
                        }
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                                break;
                            }
                        }
                        let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
                        seen.lock().unwrap().push(path.clone());
                        let answer = routes.lock().unwrap().get_mut(&path).and_then(|a| {
                            if a.len() > 1 {
                                a.pop_front()
                            } else {
                                a.front().cloned()
                            }
                        });
                        let _ = match answer {
                        None => stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"),
                        Some(Answer::Body(kind, body)) => stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                    body.len()
                                )
                                .as_bytes(),
                            )
                            .and_then(|()| stream.write_all(&body)),
                        Some(Answer::Redirect(to)) => stream.write_all(
                            format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                                .as_bytes(),
                        ),
                        Some(Answer::Cut(body)) => stream
                            .write_all(
                                format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len())
                                    .as_bytes(),
                            )
                            .and_then(|()| stream.write_all(&body[..body.len() / 2])),
                        Some(Answer::Stall(body)) => {
                            let _ = stream.write_all(
                                format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len()).as_bytes(),
                            );
                            let _ = stream.write_all(&body[..body.len() / 2]);
                            let _ = stream.flush();
                            std::thread::sleep(Duration::from_secs(30));
                            Ok(())
                        }
                    };
                    });
                }
            });
            Self { origin, hits }
        }

        fn hits(&self, path: &str) -> usize {
            self.hits.lock().unwrap().iter().filter(|p| *p == path).count()
        }
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn cache_files(cache: &Cache) -> Vec<String> {
        let mut out: Vec<String> = walk(&cache.root);
        out.sort();
        out
    }

    fn walk(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                out.extend(walk(&entry.path()));
            } else {
                out.push(entry.path().display().to_string());
            }
        }
        out
    }

    #[tokio::test]
    async fn a_broken_download_is_tried_again_and_a_cancelled_one_leaves_no_entry() {
        let body = b"wheel bytes, long enough to cut in half".to_vec();
        let server = Server::start(vec![
            (
                "/f/demo-1.0-py3-none-any.whl",
                vec![Answer::Cut(body.clone()), Answer::Body("application/zip", body.clone())],
            ),
            ("/f/always-cut-1.0-py3-none-any.whl", vec![Answer::Cut(body.clone())]),
            ("/f/stall-1.0-py3-none-any.whl", vec![Answer::Stall(body.clone())]),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(&tmp.path().join("cache"), 1).unwrap();
        let net = Net::new(vec![server.origin.clone()], Duration::from_millis(10)).unwrap();
        let file = |name: &str| IndexFile {
            filename: name.to_owned(),
            url: Url::parse(&format!("{}/f/{name}", server.origin)).unwrap(),
            sha256: Some(sha(&body)),
        };
        let hashes = vec![sha(&body)];

        let stored = net.download(&file("demo-1.0-py3-none-any.whl"), &hashes, &cache).await.unwrap();
        assert_eq!(server.hits("/f/demo-1.0-py3-none-any.whl"), 2, "the cut transfer was tried again");
        assert_eq!(std::fs::read(&stored.path).unwrap(), body);
        assert!(cache.lookup(&sha(&body), "demo-1.0-py3-none-any.whl").is_some());

        let err = net.download(&file("always-cut-1.0-py3-none-any.whl"), &hashes, &cache).await.unwrap_err();
        assert!(format!("{err:#}").contains("tried 3 times"), "{err:#}");
        assert_eq!(server.hits("/f/always-cut-1.0-py3-none-any.whl"), 3);
        let only = cache_files(&cache);
        assert_eq!(only.len(), 1, "only the whole file is in the cache: {only:?}");

        let cancelled = tokio::time::timeout(
            Duration::from_millis(500),
            net.download(&file("stall-1.0-py3-none-any.whl"), &hashes, &cache),
        )
        .await;
        assert!(cancelled.is_err(), "the download was cancelled while it waited");
        assert_eq!(cache_files(&cache), only, "a cancelled download leaves no entry and no partial file");

        // A file the lock does not list is not stored either.
        let other = vec![sha(b"something else")];
        let err = net.download(&file("demo-1.0-py3-none-any.whl"), &other, &cache).await.unwrap_err();
        assert!(format!("{err:#}").contains("which the lock does not list"), "{err:#}");
        assert_eq!(cache_files(&cache), only);
        // An entry that rotted is dropped, not used.
        std::fs::write(&stored.path, b"rotten").unwrap();
        assert!(cache.lookup(&sha(&body), "demo-1.0-py3-none-any.whl").is_none());
    }

    #[tokio::test]
    async fn requests_and_redirects_reach_only_the_allowed_origins() {
        let other =
            Server::start(vec![("/f/demo-1.0-py3-none-any.whl", vec![Answer::Body("application/zip", b"x".to_vec())])]);
        let index = Server::start(vec![
            ("/simple/demo/", vec![Answer::Redirect(format!("{}/simple/demo/", other.origin))]),
            ("/f/moved.whl", vec![Answer::Redirect(format!("{}/f/demo-1.0-py3-none-any.whl", other.origin))]),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(&tmp.path().join("cache"), 1).unwrap();
        let net = Net::new(vec![index.origin.clone()], Duration::from_millis(10)).unwrap();
        let root = Url::parse(&format!("{}/simple/", index.origin)).unwrap();
        let err = format!("{:#}", net.page(&root, "demo").await.unwrap_err());
        assert!(err.contains(&other.origin) && err.contains("not an origin [prepare.python] allows"), "{err}");
        assert_eq!(index.hits("/simple/demo/"), 1, "a refused redirect is not tried again");

        let file = |url: String| IndexFile {
            filename: "demo-1.0-py3-none-any.whl".into(),
            url: Url::parse(&url).unwrap(),
            sha256: None,
        };
        let err = format!(
            "{:#}",
            net.download(&file(format!("{}/f/moved.whl", index.origin)), &[sha(b"x")], &cache).await.unwrap_err()
        );
        assert!(err.contains("not an origin"), "{err}");
        let err = format!(
            "{:#}",
            net.download(&file(format!("{}/f/demo-1.0-py3-none-any.whl", other.origin)), &[sha(b"x")], &cache)
                .await
                .unwrap_err()
        );
        assert!(err.contains("not an origin"), "{err}");
        assert_eq!(other.hits("/f/demo-1.0-py3-none-any.whl"), 0, "nothing was sent to the other origin");

        // Allowed, the redirect is followed.
        let net = Net::new(vec![index.origin.clone(), other.origin.clone()], Duration::from_millis(10)).unwrap();
        let stored = net.download(&file(format!("{}/f/moved.whl", index.origin)), &[sha(b"x")], &cache).await.unwrap();
        assert_eq!(std::fs::read(stored.path).unwrap(), b"x");
    }

    #[test]
    fn the_cache_keeps_to_its_quota_by_evicting_what_this_preparation_does_not_use() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(&tmp.path().join("cache"), 1).unwrap();
        let put = |name: &str, bytes: usize| {
            let data = vec![b'x'; bytes];
            let mut f = cache.temp().unwrap();
            f.write_all(&data).unwrap();
            let hash = sha(name.as_bytes());
            cache.store(f, &hash, name).unwrap();
            hash
        };
        let old = put("old.whl", 400 << 10);
        let used = put("used.whl", 400 << 10);
        cache.keep.lock().unwrap().remove(&old);
        let room = cache.room(300 << 10).unwrap();
        assert!(room >= 300 << 10);
        let left: Vec<String> = cache.entries().into_iter().map(|(h, _, _)| h).collect();
        assert_eq!(left, std::slice::from_ref(&used), "the entry this preparation does not use made room");
        let err = cache.room(800 << 10).unwrap_err().to_string();
        assert!(err.contains("quota of 1 MiB"), "{err}");
        assert!(cache.store(cache.temp().unwrap(), &old, "../escape").is_err());
    }
}
