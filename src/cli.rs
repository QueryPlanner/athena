//! The command-line transport: argument dispatch and the REPL.
//!
//! Input and output are parameters rather than stdin/stdout so tests drive
//! the CLI in-process. The agent is built lazily through `make_agent`, so
//! `sessions` and `usage` work without an API key.

use crate::runner::{self, Run};
use crate::store;
use anyhow::Result;
use rusqlite::Connection;
use std::io::{BufRead, Write};

/// `athena [session] [prompt]`, or `athena sessions` / `athena usage`.
///
/// With a prompt, runs one turn and prints the reply. Without one, starts a
/// REPL on `input`. The session defaults to `default`.
pub async fn run<R: Run>(
    args: &[String],
    db: &Connection,
    model: &str,
    make_agent: impl FnOnce() -> Result<R>,
    input: impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let session = args.first().map_or("default", String::as_str);

    if session == "sessions" {
        for (id, n) in store::sessions(db)? {
            writeln!(out, "{id}\t{n} messages")?;
        }
        return Ok(());
    }

    if session == "usage" {
        writeln!(out, "session\truns\tcalls\tin\tout\tcached")?;
        for u in store::usage(db)? {
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}\t{}",
                u.session_id,
                u.runs,
                u.model_calls,
                u.input_tokens,
                u.output_tokens,
                u.cached_input_tokens
            )?;
        }
        return Ok(());
    }

    let agent = make_agent()?;
    eprintln!(
        "session `{session}` — {} messages restored",
        store::load(db, session)?.len()
    );

    match args.get(1) {
        Some(prompt) => writeln!(
            out,
            "{}",
            runner::turn(&agent, db, session, model, prompt).await?
        )?,
        None => repl(&agent, db, session, model, input, out).await?,
    }
    Ok(())
}

/// Read prompts until `exit` or end of input. Blank lines are skipped.
async fn repl<R: Run>(
    agent: &R,
    db: &Connection,
    session: &str,
    model: &str,
    mut input: impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    loop {
        write!(out, "> ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "exit" {
            break;
        }
        writeln!(
            out,
            "{}\n",
            runner::turn(agent, db, session, model, prompt).await?
        )?;
    }
    Ok(())
}
