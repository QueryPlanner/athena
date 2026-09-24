//! The CLI, driven two ways: in-process through `cli::run` with the mock
//! model, and as the real `athena` binary for everything that needs no
//! network.

mod common;

use athena::{cli, runner};
use common::*;
use rig_agent::agent::Agent;
use rig_core::test_utils::MockTurn;
use std::process::Command;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// An agent factory that fails the test if the CLI ever calls it.
fn no_agent() -> anyhow::Result<Agent> {
    anyhow::bail!("this command must not build an agent")
}

async fn cli(list: &[&str], db: &rusqlite::Connection, agent: Agent, input: &str) -> String {
    let mut out = Vec::new();
    cli::run(
        &args(list),
        db,
        "m",
        || Ok(agent),
        input.as_bytes(),
        &mut out,
    )
    .await
    .unwrap();
    String::from_utf8(out).unwrap()
}

async fn seeded(tmp: &TempDb) -> rusqlite::Connection {
    let db = tmp.open();
    let (agent, _) = mock_agent(add_turns());
    runner::turn(&agent, &db, "s", "m", "add 21 and 21")
        .await
        .unwrap();
    db
}

#[tokio::test]
async fn sessions_and_usage_read_the_database_without_building_an_agent() {
    let tmp = TempDb::new();
    let db = seeded(&tmp).await;

    for (command, expected) in [
        ("sessions", "s\t4 messages\n"),
        (
            "usage",
            "session\truns\tcalls\tin\tout\tcached\ns\t1\t2\t256\t26\t0\n",
        ),
    ] {
        let mut out = Vec::new();
        cli::run(&args(&[command]), &db, "m", no_agent, &b""[..], &mut out)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }
}

#[tokio::test]
async fn a_prompt_argument_runs_one_turn_and_prints_the_reply() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, _) = mock_agent(add_turns());

    let out = cli(&["s", "add 21 and 21"], &db, agent, "").await;

    assert_eq!(out, "42\n");
    assert_eq!(raw_rows(&db, "s").len(), 4);
}

#[tokio::test]
async fn with_no_arguments_the_repl_runs_in_the_default_session() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, _) = mock_agent([MockTurn::text("hello")]);

    let out = cli(&[], &db, agent, "hi\nexit\n").await;

    assert_eq!(out, "> hello\n\n> ");
    assert_eq!(raw_rows(&db, "default").len(), 2);
}

#[tokio::test]
async fn the_repl_skips_blank_lines_and_stops_at_end_of_input() {
    let tmp = TempDb::new();
    let db = tmp.open();
    // One scripted reply: a blank line reaching the model would exhaust it.
    let (agent, model) = mock_agent([MockTurn::text("hello")]);

    let out = cli(&["s"], &db, agent, "\n   \nhi\n").await;

    assert_eq!(out, "> > > hello\n\n> ");
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn the_repl_stops_at_exit_and_ignores_what_follows() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, model) = mock_agent([]);

    cli(&["s"], &db, agent, "exit\nhi\n").await;

    assert_eq!(model.request_count(), 0);
    assert!(raw_rows(&db, "s").is_empty());
}

#[tokio::test]
async fn a_failed_agent_build_is_reported_before_anything_is_written() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let mut out = Vec::new();

    let err = cli::run(&args(&["s", "hi"]), &db, "m", no_agent, &b""[..], &mut out)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("must not build"), "{err}");
    assert!(raw_rows(&db, "s").is_empty());
    assert!(runs(&db, "s").is_empty());
}

/// Output that accepts `ok_writes` writes and then fails, like a pipe whose
/// reader has gone away (`athena usage | head -1`).
struct ClosedAfter {
    ok_writes: usize,
}

impl std::io::Write for ClosedAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.ok_writes == 0 {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        self.ok_writes -= 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Each case: arguments, REPL input, and how many writes succeed before the
/// output closes, chosen so the failure lands on a different write site.
#[tokio::test]
async fn a_closed_output_is_an_error_not_silently_dropped() {
    let tmp = TempDb::new();
    let db = seeded(&tmp).await;

    for (list, input, ok_writes, what) in [
        (&["sessions"][..], "", 0, "the session list"),
        (&["usage"][..], "", 0, "the usage header"),
        (&["usage"][..], "", 1, "a usage row"),
        (&["s", "hi"][..], "", 0, "a one-shot reply"),
        (&["s"][..], "hi\n", 0, "the REPL prompt"),
        (&["s"][..], "hi\n", 1, "a REPL reply"),
    ] {
        let (agent, _) = mock_agent([MockTurn::text("hello")]);
        let mut out = ClosedAfter { ok_writes };
        let result = cli::run(
            &args(list),
            &db,
            "m",
            || Ok(agent),
            input.as_bytes(),
            &mut out,
        )
        .await;
        let err = result.expect_err(what);
        assert!(err.to_string().contains("pipe"), "{what}: {err}");
    }
}

// ---- the real binary ----

/// `athena` pointed at a temp database, with no key and no model override
/// inherited from the developer's shell. `env_remove` rather than
/// `env_clear`, which would also drop the coverage profiler's variables.
fn athena(tmp: &TempDb, list: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_athena"))
        .args(list)
        .env("ATHENA_DB", tmp.path())
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("AGENT_MODEL")
        .output()
        .unwrap()
}

#[tokio::test]
async fn the_binary_reads_the_database_named_by_athena_db() {
    let tmp = TempDb::new();
    drop(seeded(&tmp).await);

    let out = athena(&tmp, &["sessions"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "s\t4 messages\n");
}

#[test]
fn the_binary_creates_a_migrated_database_on_first_use() {
    let tmp = TempDb::new();

    let out = athena(&tmp, &["usage"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "session\truns\tcalls\tin\tout\tcached\n"
    );
    assert_eq!(
        user_version(&tmp.raw()),
        athena::store::SCHEMA_VERSION as i64
    );
}

#[test]
fn the_binary_without_an_api_key_fails_clearly_and_writes_nothing() {
    let tmp = TempDb::new();

    let out = athena(&tmp, &["s", "hi"]);

    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("OPENROUTER_API_KEY"), "{stderr}");
    let db = tmp.open();
    assert!(raw_rows(&db, "s").is_empty());
    assert!(runs(&db, "s").is_empty());
}
