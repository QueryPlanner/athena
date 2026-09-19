//! The agent loop. Load history, run a turn, save history.

use crate::store;
use anyhow::Result;
use rig_agent::prelude::Chat;
use rusqlite::Connection;
use std::io::{BufRead, Write};

/// One turn: load, chat, save. This is the whole service.
pub async fn turn<C: Chat>(
    agent: &C,
    db: &Connection,
    session: &str,
    prompt: &str,
) -> Result<String> {
    let mut history = store::load(db, session)?;
    let reply = agent.chat(prompt, &mut history).await?;
    store::save(db, session, &history)?;
    Ok(reply)
}

pub async fn repl<C: Chat>(agent: &C, db: &Connection, session: &str) -> Result<()> {
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" {
            break;
        }
        println!("{}\n", turn(agent, db, session, input).await?);
    }
    Ok(())
}
