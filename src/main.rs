//! Wiring only: `.env`, real database, real provider, real stdin/stdout.

use anyhow::Result;
use athena::service::Service;
use athena::{agent, cli, dotenv, http, ops, shutdown, store, telegram};
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
    if ops::requested(&args) {
        return ops::run(&args, &mut std::io::stdout());
    }
    let model = agent::model();
    if telegram::requested(&args)? {
        ops::require_absolute_db()?;
        return telegram::main(&model).await;
    }
    let serving = args.first().is_some_and(|a| a == "serve");
    if serving {
        ops::require_absolute_db()?;
    }
    let service = Service::new(store::Store::open(&store::path())?, &model, cli::warn);
    if serving {
        // Up front: a server without a key would fail every turn.
        let agent = agent::build(&agent::client()?, &model, service.memory());
        let stop = shutdown::listen()?;
        return http::run(&args[1..], Arc::new(service), Arc::new(agent), stop).await;
    }
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
