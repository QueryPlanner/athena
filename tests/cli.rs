//! The CLI, driven two ways: in-process through `cli::run` with the mock
//! model, and as the real `athena` binary for everything that needs no
//! network.

mod common;

use athena::cli;
use athena::service::Service;
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

async fn try_cli(
    list: &[&str],
    service: &Service,
    agent: Agent,
    input: &str,
) -> anyhow::Result<String> {
    let mut out = Vec::new();
    cli::run(
        &args(list),
        service,
        || Ok(agent),
        input.as_bytes(),
        &mut out,
    )
    .await?;
    Ok(String::from_utf8(out).unwrap())
}

async fn cli(list: &[&str], service: &Service, agent: Agent, input: &str) -> String {
    try_cli(list, service, agent, input).await.unwrap()
}

/// A command that must work without an agent.
async fn offline(list: &[&str], service: &Service) -> anyhow::Result<String> {
    let mut out = Vec::new();
    cli::run(&args(list), service, no_agent, &b""[..], &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

/// A service whose cli:local user has run `add 21 and 21` in session `s`.
async fn seeded(tmp: &TempDb) -> Service {
    let (service, _) = tmp.service();
    let (agent, _) = mock_agent(&service, add_turns());
    cli(&["s", "add 21 and 21"], &service, agent, "").await;
    service
}

fn cli_session(tmp: &TempDb, name: &str) -> String {
    session_id(&tmp.raw(), "cli", "local", name)
}

#[tokio::test]
async fn sessions_and_usage_read_the_database_without_building_an_agent() {
    let tmp = TempDb::new();
    let service = seeded(&tmp).await;

    assert_eq!(
        offline(&["sessions"], &service).await.unwrap(),
        "s\t4 messages\n"
    );
    assert_eq!(
        offline(&["usage"], &service).await.unwrap(),
        "session\truns\tcalls\tin\tout\tcached\ns\t1\t2\t256\t26\t0\n"
    );
}

#[tokio::test]
async fn a_prompt_argument_runs_one_turn_and_prints_the_reply() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let (agent, _) = mock_agent(&service, add_turns());

    let out = cli(&["s", "add 21 and 21"], &service, agent, "").await;

    assert_eq!(out, "42\n");
    assert_eq!(raw_rows(&tmp.raw(), &cli_session(&tmp, "s")).len(), 4);
}

#[tokio::test]
async fn with_no_arguments_the_repl_runs_in_the_default_session() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let (agent, _) = mock_agent(&service, [MockTurn::text("hello")]);

    let out = cli(&[], &service, agent, "hi\nexit\n").await;

    assert_eq!(out, "> hello\n\n> ");
    assert_eq!(raw_rows(&tmp.raw(), &cli_session(&tmp, "default")).len(), 2);
}

#[tokio::test]
async fn the_repl_skips_blank_lines_and_stops_at_end_of_input() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    // One scripted reply: a blank line reaching the model would exhaust it.
    let (agent, model) = mock_agent(&service, [MockTurn::text("hello")]);

    let out = cli(&["s"], &service, agent, "\n   \nhi\n").await;

    assert_eq!(out, "> > > hello\n\n> ");
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn the_repl_stops_at_exit_and_ignores_what_follows() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let (agent, model) = mock_agent(&service, []);

    cli(&["s"], &service, agent, "exit\nhi\n").await;

    assert_eq!(model.request_count(), 0);
    assert!(raw_rows(&tmp.raw(), &cli_session(&tmp, "s")).is_empty());
}

#[tokio::test]
async fn a_failed_agent_build_is_reported_before_anything_is_written() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();

    let err = offline(&["s", "hi"], &service).await.unwrap_err();

    assert!(err.to_string().contains("must not build"), "{err}");
    let db = tmp.raw();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM runs"), 0);
}

#[tokio::test]
async fn sessions_new_creates_an_empty_session_once() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();

    let out = offline(&["sessions", "new", "research"], &service)
        .await
        .unwrap();
    let err = offline(&["sessions", "new", "research"], &service)
        .await
        .unwrap_err();

    let id = cli_session(&tmp, "research");
    assert_eq!(out, format!("research\t{id}\n"));
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM sessions"), 1);
    assert_eq!(
        offline(&["sessions"], &service).await.unwrap(),
        "research\t0 messages\n"
    );

    // Chatting in it by name uses that session rather than making another.
    let (agent, _) = mock_agent(&service, [MockTurn::text("hello")]);
    cli(&["research", "hi"], &service, agent, "").await;
    assert_eq!(raw_rows(&tmp.raw(), &id).len(), 2);
}

#[tokio::test]
async fn another_user_sees_none_of_the_cli_users_sessions() {
    let tmp = TempDb::new();
    let service = seeded(&tmp).await;
    let tg = ["--user", "telegram:42"];

    assert_eq!(
        offline(&[&tg[..], &["sessions"]].concat(), &service)
            .await
            .unwrap(),
        ""
    );
    assert_eq!(
        offline(&[&tg[..], &["usage"]].concat(), &service)
            .await
            .unwrap(),
        "session\truns\tcalls\tin\tout\tcached\n"
    );

    // The same session name is a different, empty conversation for them.
    let (agent, model) = mock_agent(&service, [MockTurn::text("hello")]);
    let out = cli(&[&tg[..], &["s", "hi"]].concat(), &service, agent, "").await;

    assert_eq!(out, "hello\n");
    assert_eq!(model.requests()[0].chat_history.len(), 2);
    let db = tmp.raw();
    let theirs = session_id(&db, "telegram", "42", "s");
    assert_ne!(theirs, cli_session(&tmp, "s"));
    assert_eq!(raw_rows(&db, &theirs).len(), 2);
    assert_eq!(raw_rows(&db, &cli_session(&tmp, "s")).len(), 4);
    assert_eq!(
        offline(&["sessions"], &service).await.unwrap(),
        "s\t4 messages\n"
    );
}

#[tokio::test]
async fn bad_arguments_are_refused_before_anything_is_written() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();

    for list in [
        &["--user", "nobody"][..],
        &["sessions", "new"][..],
        &["sessions", "new", "  "][..],
    ] {
        assert!(offline(list, &service).await.is_err(), "{list:?}");
    }
    let (agent, model) = mock_agent(&service, []);
    let err = try_cli(&["s", ""], &service, agent, "").await.unwrap_err();

    assert!(
        err.to_string().contains("message must not be empty"),
        "{err}"
    );
    assert_eq!(model.request_count(), 0);
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM messages"), 0);
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
    let service = seeded(&tmp).await;

    for (list, input, ok_writes, what) in [
        (&["sessions"][..], "", 0, "the session list"),
        (&["sessions", "new", "fresh"][..], "", 0, "the new session"),
        (&["usage"][..], "", 0, "the usage header"),
        (&["usage"][..], "", 1, "a usage row"),
        (&["s", "hi"][..], "", 0, "a one-shot reply"),
        (&["s"][..], "hi\n", 0, "the REPL prompt"),
        (&["s"][..], "hi\n", 1, "a REPL reply"),
    ] {
        let (agent, _) = mock_agent(&service, [MockTurn::text("hello")]);
        let mut out = ClosedAfter { ok_writes };
        let result = cli::run(
            &args(list),
            &service,
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

fn stdout(out: std::process::Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn the_binary_reads_the_database_named_by_athena_db() {
    let tmp = TempDb::new();
    drop(seeded(&tmp).await);

    assert_eq!(stdout(athena(&tmp, &["sessions"])), "s\t4 messages\n");
}

#[test]
fn the_binary_creates_a_migrated_database_on_first_use() {
    let tmp = TempDb::new();

    assert_eq!(
        stdout(athena(&tmp, &["usage"])),
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
    let db = tmp.raw();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM messages"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM runs"), 0);
}

#[test]
fn the_binary_creates_and_lists_sessions_per_user() {
    let tmp = TempDb::new();

    let created = stdout(athena(&tmp, &["sessions", "new", "notes"]));
    let duplicate = athena(&tmp, &["sessions", "new", "notes"]);
    let theirs = stdout(athena(
        &tmp,
        &["--user", "telegram:42", "sessions", "new", "notes"],
    ));

    assert!(!duplicate.status.success(), "{duplicate:?}");
    assert!(
        String::from_utf8(duplicate.stderr)
            .unwrap()
            .contains("already exists")
    );
    let db = tmp.raw();
    let mine = session_id(&db, "cli", "local", "notes");
    let other = session_id(&db, "telegram", "42", "notes");
    assert_eq!(created, format!("notes\t{mine}\n"));
    assert_eq!(theirs, format!("notes\t{other}\n"));
    assert_ne!(mine, other);
    assert_eq!(stdout(athena(&tmp, &["sessions"])), "notes\t0 messages\n");
    assert_eq!(
        stdout(athena(&tmp, &["--user", "telegram:43", "sessions"])),
        ""
    );
}
