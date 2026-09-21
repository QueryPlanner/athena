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

    if session == "usage" {
        println!("session\truns\tcalls\tin\tout\tcached");
        for u in store::usage(&db)? {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                u.session_id,
                u.runs,
                u.model_calls,
                u.input_tokens,
                u.output_tokens,
                u.cached_input_tokens
            );
        }
        return Ok(());
    }

    let model = agent::model();
    let agent = agent::build(&model)?;
    eprintln!(
        "session `{session}` — {} messages restored",
        store::load(&db, &session)?.len()
    );

    match args.next() {
        Some(prompt) => println!(
            "{}",
            runner::turn(&agent, &db, &session, &model, &prompt).await?
        ),
        None => runner::repl(&agent, &db, &session, &model).await?,
    }
    Ok(())
}
