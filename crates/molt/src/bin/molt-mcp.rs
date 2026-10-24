//! The MCP bridge, one process per server of the user's `[mcp.NAME]`
//! configuration (see `docs/mcp.md`):
//!
//! - `molt-mcp list ...` prints the declarations of the server's tools for
//!   its manifest, as JSON, from the cache or from the server;
//! - `molt-mcp serve ...`, started by the supervisor as service `mcp-NAME`,
//!   serves `mcp-NAME.call`.
//!
//! molt writes the arguments from the trusted configuration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use molt_mcp::{Launch, McpServer};
use molt_sdk::Service;
use molt_tools::SandboxPolicy;

#[derive(Parser)]
#[command(name = "molt-mcp", version, about = "Molt's MCP bridge: one MCP server's tools as a Molt service")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the declarations of the server's tools, as JSON.
    List(LaunchArgs),
    /// Serve the server's tools on the bus.
    Serve(LaunchArgs),
}

#[derive(Args)]
struct LaunchArgs {
    /// The server's name in `[mcp.NAME]`.
    #[arg(long)]
    name: String,
    /// The server's configuration, as JSON.
    #[arg(long)]
    server: String,
    /// The cache file of its tool list.
    #[arg(long)]
    cache: PathBuf,
    /// The sandbox policy, as JSON; the shell's commands' default when left out.
    #[arg(long, conflicts_with = "no_sandbox")]
    policy: Option<String>,
    /// Run the server with the host user's files and network, as the shell's commands are.
    #[arg(long)]
    no_sandbox: bool,
}

impl LaunchArgs {
    fn launch(self) -> anyhow::Result<Launch> {
        let server: McpServer = serde_json::from_str(&self.server).context("--server")?;
        server.validate(&self.name).map_err(anyhow::Error::msg)?;
        let sandbox = match (self.no_sandbox, self.policy) {
            (true, _) => None,
            (false, Some(json)) => Some(serde_json::from_str::<SandboxPolicy>(&json).context("--policy")?),
            (false, None) => Some(SandboxPolicy::default()),
        };
        Ok(Launch { name: self.name, server, cache: self.cache, sandbox })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    match Cli::parse().cmd {
        Cmd::List(args) => {
            let listing = args.launch()?.list().await?;
            let out = serde_json::json!({
                "tools": listing.tools,
                "cached": listing.cached,
                "left_out": listing.left_out,
            });
            println!("{out}");
        }
        Cmd::Serve(args) => {
            let launch = args.launch()?;
            let svc = Arc::new(Service::connect_from_env().await?);
            launch.serve(svc).await;
        }
    }
    Ok(())
}
