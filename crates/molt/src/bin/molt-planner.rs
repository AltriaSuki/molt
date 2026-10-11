//! The planner service (`planner.*`), started by the kernel. Its settings
//! come from the environment; see `molt_planner::Config::from_env`.
//! `--state DIR` keeps run checkpoints in `DIR`.

use std::sync::Arc;

use anyhow::Context;

use molt_sdk::Service;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let mut cfg = molt_planner::Config::from_env()?;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state" => cfg.state_dir = Some(args.next().context("--state needs a directory")?.into()),
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let svc = Arc::new(Service::connect_from_env().await?);
    molt_planner::serve(svc, cfg).await;
    Ok(())
}
