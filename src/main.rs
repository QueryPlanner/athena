//! Wiring only: real database, real provider, real stdin/stdout.

use anyhow::Result;
use athena::{agent, cli, store};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db = store::open(&store::path())?;
    let model = agent::model();
    let make_agent = || Ok(agent::build(&agent::client()?, &model));
    cli::run(
        &args,
        &db,
        &model,
        make_agent,
        std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .await
}
