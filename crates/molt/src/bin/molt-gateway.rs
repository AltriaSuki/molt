//! The model gateway service (`model.*`), started by the kernel. Its
//! settings come from the environment; see `molt_gateway::Config::from_env`.

use std::sync::Arc;

use molt_gateway::{Config, Gateway};
use molt_sdk::Service;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    molt::init_tracing("info");
    let svc = Arc::new(Service::connect_from_env().await?);
    let gateway = Arc::new(Gateway::new(Config::from_env()?)?);
    molt_gateway::serve(svc, gateway).await;
    Ok(())
}
