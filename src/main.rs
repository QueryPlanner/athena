//! Wiring only: `.env`, real database, real provider, real stdin/stdout.

use anyhow::Result;
use athena::service::Service;
use athena::{agent, bench, cli, dotenv, eval, http, ops, shutdown, store, telegram, telemetry};
use std::sync::Arc;

fn main() -> Result<()> {
    // Before the runtime exists: setting environment variables is only
    // sound while this is the only thread.
    dotenv::load(".env")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let telemetry = telemetry::init(telemetry::Role::from_args(&args))?;
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args));
    // After `run`: servers return only once their turns have drained, so
    // the last batch of spans and logs includes them.
    telemetry.shutdown();
    result
}

async fn run(args: Vec<String>) -> Result<()> {
    if ops::requested(&args) {
        return ops::run(&args, &mut std::io::stdout());
    }
    let model = agent::model();
    if telegram::requested(&args)? {
        ops::require_absolute_db()?;
        return telegram::main(&model).await;
    }
    // Before opening the database: neither touches `ATHENA_DB`.
    let mut stdout = std::io::stdout();
    match args.split_first() {
        Some((cmd, rest)) if cmd == "eval" => {
            return eval::main(rest, &model, agent::provider_model, &mut stdout).await;
        }
        Some((cmd, rest)) if cmd == "bench" => return bench::main(rest, &mut stdout).await,
        _ => {}
    }
    let serving = args.first().is_some_and(|a| a == "serve");
    if serving {
        ops::require_absolute_db()?;
    }
    let service = Service::new(store::Store::open(&store::path())?, &model, cli::warn);
    if serving {
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
