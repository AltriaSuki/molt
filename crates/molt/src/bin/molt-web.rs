//! The web service, started by the kernel: `molt-web --allow docs.rs
//! --allow '*.python.org'` serves `web.fetch` for pages on those hosts.

use std::sync::Arc;

use clap::Parser;
use molt_sdk::Service;
use molt_web::{Allow, Web};

#[derive(Parser)]
#[command(name = "molt-web", version, about = "Molt's web service: pages from the hosts you allow")]
struct Cli {
    /// A host pages may come from (repeatable): `docs.rs` for that host,
    /// `*.python.org` for the hosts below python.org.
    #[arg(long = "allow", value_name = "HOST", required = true)]
    allow: Vec<String>,
    /// system, or a public-IP HTTPS endpoint returning application/dns-json.
    #[arg(long, default_value = "system")]
    resolver: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let cli = Cli::parse();
    let allow = Allow::parse(&cli.allow).map_err(anyhow::Error::msg)?;
    let web = Arc::new(Web::with_resolver(allow, &cli.resolver)?);
    let svc = Arc::new(Service::connect_from_env().await?);
    molt_web::serve(svc, web).await;
    Ok(())
}
