//! Wiring only: `.env`, real database, real provider, real stdin/stdout.

use anyhow::Result;
use athena::service::Service;
use athena::{agent, cli, dotenv, http, shutdown, store, telegram};
use std::sync::Arc;

fn main() -> Result<()> {
    // Before the runtime exists: setting environment variables is only
    // sound while this is the only thread.
    dotenv::load(".env")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let model = agent::model();
    if telegram::requested(&args)? {
        return telegram::main(&model).await;
    }
    let service = Service::new(store::Store::open(&store::path())?, &model, cli::warn);
    if args.first().is_some_and(|a| a == "serve") {
        // Up front: a server without a key would fail every turn.
        let agent = agent::build(&agent::client()?, &model, service.memory())?;
        let stop = shutdown::listen()?;
        return http::run(&args[1..], Arc::new(service), Arc::new(agent), stop).await;
    }
    let make_agent = || agent::build(&agent::client()?, &model, service.memory());
    cli::run(
        &args,
        &service,
        make_agent,
        std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .await
}
