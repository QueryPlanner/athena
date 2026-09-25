//! Wiring only: real database, real provider, real stdin/stdout.

use anyhow::Result;
use athena::service::Service;
use athena::{agent, cli, store, telegram};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = agent::model();
    if telegram::requested(&args)? {
        return telegram::main(&model).await;
    }
    let service = Service::new(store::Store::open(&store::path())?, &model, cli::warn);
    let make_agent = || Ok(agent::build(&agent::client()?, &model, service.memory()));
    cli::run(
        &args,
        &service,
        make_agent,
        std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .await
}
