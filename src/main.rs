mod agent;
mod runner;
mod store;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let db = store::open("agent.db")?;

    let mut args = std::env::args().skip(1);
    let session = args.next().unwrap_or_else(|| "default".into());

    if session == "sessions" {
        for (id, n) in store::sessions(&db)? {
            println!("{id}\t{n} messages");
        }
        return Ok(());
    }

    let agent = agent::build()?;
    eprintln!(
        "session `{session}` — {} messages restored",
        store::load(&db, &session)?.len()
    );

    match args.next() {
        Some(prompt) => println!("{}", runner::turn(&agent, &db, &session, &prompt).await?),
        None => runner::repl(&agent, &db, &session).await?,
    }
    Ok(())
}
