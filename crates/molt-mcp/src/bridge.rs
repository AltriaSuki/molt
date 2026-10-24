//! Listing a server's tools for the manifest, and serving their calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use molt_proto::tool::{ToolCall, ToolDecl, ToolOutput};
use molt_proto::{ErrorCode, RemoteError};
use molt_sdk::Service;
use molt_tools::SandboxPolicy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::cache::{CacheFile, ListedTool};
use crate::client::Client;
use crate::McpServer;

/// How long a server gets to start and to list its tools.
const STARTUP: Duration = Duration::from_secs(30);
/// How often a server is started in one run before the bridge gives up on it.
const MAX_STARTS: u32 = 4;
/// The most result text sent back on the bus; the planner cuts it further.
const MAX_REPLY: usize = 256 * 1024;
/// Where the system's programs are; anything elsewhere is mounted for the server.
const SYSTEM: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64"];

/// One server, as the bridge is told to run it.
#[derive(Clone, Debug)]
pub struct Launch {
    /// Its name in `[mcp.NAME]`.
    pub name: String,
    pub server: McpServer,
    /// The cache file of its tool list.
    pub cache: PathBuf,
    /// The sandbox it runs in, or none when the user chose `--no-sandbox`.
    pub sandbox: Option<SandboxPolicy>,
}

/// The declarations for a server's manifest.
#[derive(Debug, Default)]
pub struct Listing {
    pub tools: Vec<ToolDecl>,
    /// They came from the cache; the server was not started.
    pub cached: bool,
    /// The tools left out, each with why.
    pub left_out: Vec<String>,
}

impl Launch {
    /// The hash of everything that decides what the server lists.
    fn key(&self) -> String {
        let doc = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "name": self.name,
            "server": self.server,
            "sandbox": self.sandbox,
        });
        hex::encode(Sha256::digest(doc.to_string().as_bytes()))
    }

    fn program(&self) -> Result<PathBuf, String> {
        let command = Path::new(&self.server.command);
        let found = if command.components().count() > 1 {
            command.to_path_buf()
        } else {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&path)
                .map(|dir| dir.join(command))
                .find(|p| p.is_file())
                .ok_or_else(|| format!("there is no {} on PATH", self.server.command))?
        };
        if !found.is_absolute() {
            return Err(format!("command {} must be an absolute path or a name on PATH", found.display()));
        }
        found.canonicalize().map_err(|e| format!("command {}: {e}", found.display()))
    }

    /// The server's command, in its sandbox unless the user chose none.
    fn command(&self) -> Result<(Command, Option<molt_tools::sandbox::Guard>), String> {
        let program = self.program()?;
        let Some(base) = &self.sandbox else {
            let mut command = Command::new(&program);
            command.args(&self.server.args).env_clear();
            let names = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TZ"].map(str::to_owned);
            for name in names.into_iter().chain(self.server.variables()) {
                if let Some(value) = std::env::var_os(&name) {
                    command.env(name, value);
                }
            }
            return Ok((command, None));
        };
        let mut policy = base.clone();
        policy.network = self.server.network;
        policy.environment = self.server.variables();
        policy.cargo_cache = None;
        policy.set_env.clear();
        for mount in &self.server.mounts {
            let mount = mount.canonicalize().map_err(|e| format!("mount {}: {e}", mount.display()))?;
            if !policy.read_only.contains(&mount) {
                policy.read_only.push(mount);
            }
        }
        let covered =
            |p: &Path| SYSTEM.iter().any(|s| p.starts_with(s)) || policy.read_only.iter().any(|r| p.starts_with(r));
        if !covered(&program) {
            policy.read_only.push(program.clone());
        }
        let (mut command, guard) = policy.server_command(&program).map_err(|e| format!("sandbox: {e:#}"))?;
        command.args(&self.server.args);
        Ok((command, Some(guard)))
    }

    async fn start(&self) -> Result<(Arc<Client>, Vec<ListedTool>), String> {
        let (command, guard) = self.command()?;
        let client = Client::start(command, guard, STARTUP).await?;
        let tools = client.list_tools(STARTUP).await?;
        Ok((client, tools))
    }

    /// The declarations of the server's tools: from the cache when it was
    /// written for this configuration, else from the server, started for it.
    /// Records them as what this run's planner is offered.
    pub async fn list(&self) -> anyhow::Result<Listing> {
        if self.server.calls == 0 {
            return Ok(Listing::default());
        }
        let key = self.key();
        let (listed, cached) = match CacheFile::read(&self.cache).filter(|c| c.key == key) {
            Some(cache) => (cache.listed, true),
            None => {
                let (client, tools) =
                    self.start().await.map_err(|e| anyhow::anyhow!("the {} MCP server: {e}", self.name))?;
                drop(client);
                (tools, false)
            }
        };
        CacheFile::new(key, listed.clone()).write(&self.cache)?;
        let mut listing = Listing { cached, ..Listing::default() };
        let mut names = std::collections::BTreeSet::new();
        for tool in &listed {
            match self.server.declare(&self.name, tool) {
                Ok(Some(decl)) if names.insert(decl.name.clone()) => listing.tools.push(decl),
                Ok(Some(decl)) => {
                    listing.left_out.push(format!("{}: another tool is also named {}", tool.name, decl.name))
                }
                Ok(None) => {}
                Err(why) => listing.left_out.push(format!("{}: {why}", tool.name)),
            }
        }
        for name in self.server.tools.keys().filter(|n| !listed.iter().any(|t| &t.name == *n)) {
            listing.left_out.push(format!("{name}: configured, but the server does not list it"));
        }
        Ok(listing)
    }

    /// The bridge of a run: it holds calls to the tools the cache says
    /// this run's planner was offered. The server starts at the first call.
    pub fn bridge(self) -> Arc<Bridge> {
        Arc::new(Bridge::new(self))
    }

    /// Serve `mcp-NAME.call` on `svc` until its link closes, starting the
    /// server at once.
    pub async fn serve(self, svc: Arc<Service>) {
        let bridge = self.bridge();
        let starting = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = starting.running().await {
                tracing::warn!(server = %starting.launch.name, "the MCP server did not start: {e}");
            }
        });
        svc.serve_cancellable(16, move |req, cancel| {
            let bridge = bridge.clone();
            async move {
                let call: ToolCall = serde_json::from_value(req.payload).map_err(|e| RemoteError {
                    code: ErrorCode::Invalid,
                    message: format!("bad {} request: {e}", crate::target(&bridge.launch.name)),
                })?;
                let output = bridge.call(call, cancel).await?;
                serde_json::to_value(output)
                    .map_err(|e| RemoteError { code: ErrorCode::Failed, message: e.to_string() })
            }
        })
        .await;
    }
}

/// A tool as this run's planner was told of it.
struct Offered {
    listed: ListedTool,
    decl: ToolDecl,
}

#[derive(Default)]
struct State {
    client: Option<Arc<Client>>,
    starts: u32,
    /// Tools the server now describes otherwise than the run was told, with how.
    stale: BTreeMap<String, String>,
}

/// One server's bridge: calls go through it to the server.
pub struct Bridge {
    launch: Launch,
    /// By the name the model calls them.
    offered: BTreeMap<String, Offered>,
    state: tokio::sync::Mutex<State>,
}

impl Bridge {
    fn new(launch: Launch) -> Self {
        let cache = CacheFile::read(&launch.cache).unwrap_or_default();
        let mut offered = BTreeMap::new();
        for listed in cache.offered {
            if let Ok(Some(decl)) = launch.server.declare(&launch.name, &listed) {
                offered.insert(decl.name.clone(), Offered { listed, decl });
            }
        }
        Self { launch, offered, state: tokio::sync::Mutex::default() }
    }

    /// The running server, started (again) when it is not.
    async fn running(self: &Arc<Self>) -> Result<Arc<Client>, String> {
        let mut state = self.state.lock().await;
        if let Some(client) = state.client.as_ref().filter(|c| !c.is_gone()) {
            return Ok(client.clone());
        }
        if state.starts >= MAX_STARTS {
            return Err(format!("it failed {MAX_STARTS} times, and is not started again in this run"));
        }
        state.starts += 1;
        if state.starts > 1 {
            tracing::warn!(server = %self.launch.name, "starting the MCP server again");
        }
        let (client, tools) = self.launch.start().await?;
        self.compare(&mut state, &tools);
        state.client = Some(client.clone());
        let (bridge, watched) = (self.clone(), Arc::downgrade(&client));
        let (changed, gone) = (client.changed.clone(), client.gone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = changed.notified() => {}
                    () = gone.cancelled() => return,
                }
                let Some(client) = watched.upgrade().filter(|c| !c.is_gone()) else { return };
                match client.list_tools(STARTUP).await {
                    Ok(tools) => bridge.compare(&mut *bridge.state.lock().await, &tools),
                    Err(e) => tracing::warn!(server = %bridge.launch.name, "relisting its tools failed: {e}"),
                }
            }
        });
        Ok(client)
    }

    /// Note which offered tools the server now describes otherwise, and
    /// have the cache hold what it says now, for the next run.
    fn compare(&self, state: &mut State, tools: &[ListedTool]) {
        state.stale.clear();
        for (name, offered) in &self.offered {
            let why = match tools.iter().find(|t| t.name == offered.listed.name) {
                None => "the server no longer lists it",
                Some(now) if now.description != offered.listed.description => "the server changed its description",
                Some(now) if now.input_schema != offered.listed.input_schema => "the server changed its input schema",
                Some(_) => continue,
            };
            tracing::warn!(server = %self.launch.name, tool = %name, "{why} since this run began");
            state.stale.insert(name.clone(), why.to_owned());
        }
        let mut cache =
            CacheFile::read(&self.launch.cache).unwrap_or_else(|| CacheFile::new(self.launch.key(), Vec::new()));
        if cache.listed != tools {
            cache.listed = tools.to_vec();
            if let Err(e) = cache.write(&self.launch.cache) {
                tracing::warn!(server = %self.launch.name, "could not update the cached tool list: {e:#}");
            }
        }
    }

    /// Make `call` to the server, if it is one the run was offered as it
    /// still is, with an input that fits the tool's schema.
    pub async fn call(self: &Arc<Self>, call: ToolCall, cancel: CancellationToken) -> Result<ToolOutput, RemoteError> {
        let server = &self.launch.name;
        let error = |code, message: String| RemoteError { code, message };
        let offered = self.offered.get(&call.tool).ok_or_else(|| {
            error(ErrorCode::Invalid, format!("the {server} MCP server has no tool {:?} in this run", call.tool))
        })?;
        let client = self
            .running()
            .await
            .map_err(|e| error(ErrorCode::Unavailable, format!("the {server} MCP server is not running: {e}")))?;
        let output = |content: String, is_error| ToolOutput { content, is_error };
        if let Some(why) = self.state.lock().await.stale.get(&call.tool) {
            return Ok(output(
                format!(
                    "{} was not called: {why} since this run began, so what you were told of it may no longer be \
                     true. The next run is offered it as the server describes it now.",
                    call.tool
                ),
                true,
            ));
        }
        let input = if call.input.is_null() { json!({}) } else { call.input };
        if let Err(why) = crate::schema::check(&offered.listed.input_schema, &input) {
            return Ok(output(format!("{} was not called: its input does not fit its schema: {why}", call.tool), true));
        }
        let timeout = Duration::from_millis(offered.decl.timeout_ms().saturating_sub(500).max(1_000));
        let params = json!({ "name": offered.listed.name, "arguments": input });
        match client.request("tools/call", params, timeout, Some(&cancel)).await {
            Ok(result) => {
                let mut text = crate::result_text(&result);
                if text.len() > MAX_REPLY {
                    let mut cut = MAX_REPLY;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    text.push_str("\n[the rest is not shown]");
                }
                Ok(output(text, result.get("isError").and_then(Value::as_bool) == Some(true)))
            }
            Err(_) if cancel.is_cancelled() => Err(error(ErrorCode::Cancelled, "the call was cancelled".into())),
            Err(e) => Err(error(ErrorCode::Failed, format!("the {server} MCP server failed the call: {e}"))),
        }
    }
}
