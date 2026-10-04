//! The planner service (`planner.*`), started by the kernel. Its settings
//! come from the environment; see `molt_planner::Config::from_env`.

use std::sync::Arc;

use molt_sdk::Service;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let cfg = molt_planner::Config::from_env()?;
    let svc = Arc::new(Service::connect_from_env().await?);
    molt_planner::serve(svc, cfg).await;
    Ok(())
}
