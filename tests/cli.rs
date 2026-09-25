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

/// `athena` run in `dir`, with no key, model or database inherited from the
/// developer's shell. `env_remove` rather than `env_clear`, which would also
/// drop the coverage profiler's variables.
fn athena_in(dir: &WorkDir, db: Option<&TempDb>, list: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_athena"));
    cmd.args(list)
        .current_dir(dir.path())
        .env_remove("ATHENA_DB")
        .env_remove("ATHENA_VERSION")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("AGENT_MODEL");
    if let Some(db) = db {
        cmd.env("ATHENA_DB", db.path());
    }
    cmd.output().unwrap()
}

/// `athena` pointed at a temp database, in an empty directory.
fn athena(tmp: &TempDb, list: &[&str]) -> std::process::Output {
    athena_in(&WorkDir::new(), Some(tmp), list)
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

#[test]
fn the_binary_reads_settings_from_a_dotenv_file_in_its_directory() {
    let dir = WorkDir::new();
    let tmp = TempDb::new();
    dir.env_file(&format!("# comment\nATHENA_DB={}\n", tmp.path()));

    stdout(athena_in(&dir, None, &["usage"]));

    // The database named only in .env was created and migrated.
    assert_eq!(
        user_version(&tmp.raw()),
        athena::store::SCHEMA_VERSION as i64
    );
}

#[test]
fn a_variable_set_in_the_shell_wins_over_dotenv() {
    let dir = WorkDir::new();
    let from_file = TempDb::new();
    let from_shell = TempDb::new();
    dir.env_file(&format!("ATHENA_DB={}\n", from_file.path()));

    stdout(athena_in(&dir, Some(&from_shell), &["usage"]));

    assert_eq!(
        user_version(&from_shell.raw()),
        athena::store::SCHEMA_VERSION as i64
    );
    assert!(!std::path::Path::new(from_file.path()).exists());
}

#[test]
fn a_malformed_dotenv_stops_the_binary_before_it_touches_anything() {
    let dir = WorkDir::new();
    let tmp = TempDb::new();
    dir.env_file("this line is not KEY=value\n");

    let out = athena_in(&dir, Some(&tmp), &["usage"]);

    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("reading .env"), "{stderr}");
    assert!(!std::path::Path::new(tmp.path()).exists());
}

#[test]
fn the_binary_prints_its_version_without_touching_the_database() {
    let tmp = TempDb::new();
    let dir = WorkDir::new();

    let dev = stdout(athena_in(&dir, Some(&tmp), &["--version"]));
    dir.env_file("ATHENA_VERSION=v1.2.3+abc\n");
    let released = stdout(athena_in(&dir, Some(&tmp), &["--version"]));

    assert_eq!(dev, format!("athena {}-dev\n", env!("CARGO_PKG_VERSION")));
    assert_eq!(released, "athena v1.2.3+abc\n");
    assert!(!std::path::Path::new(tmp.path()).exists());
}

fn wal_bytes(tmp: &TempDb) -> u64 {
    std::fs::metadata(format!("{}-wal", tmp.path()))
        .map(|m| m.len())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_binary_backs_up_a_database_in_use_including_its_wal() {
    let tmp = TempDb::new();
    // Still open, as a running server would hold it.
    let service = seeded(&tmp).await;
    assert!(wal_bytes(&tmp) > 0, "the turn should still be in the WAL");
    let dir = WorkDir::new();
    let dest = dir.path().join("backups/staging/1.db");

    let out = stdout(athena_in(
        &dir,
        Some(&tmp),
        &["backup", dest.to_str().unwrap()],
    ));

    assert_eq!(
        out,
        format!("backed up {} to {}\n", tmp.path(), dest.display())
    );
    let copy = rusqlite::Connection::open(&dest).unwrap();
    let id = cli_session(&tmp, "s");
    assert_eq!(user_version(&copy), athena::store::SCHEMA_VERSION as i64);
    assert_eq!(raw_rows(&copy, &id), raw_rows(&tmp.raw(), &id));
    assert_eq!(raw_rows(&copy, &id).len(), 4);
    assert!(!dir.path().join("backups/staging/1.db.partial").exists());
    drop(service);
}

#[tokio::test]
async fn the_binary_backs_up_a_stopped_database() {
    let tmp = TempDb::new();
    // Closed, as after `systemctl stop`: the WAL is checkpointed and gone.
    drop(seeded(&tmp).await);
    assert!(!std::path::Path::new(&format!("{}-wal", tmp.path())).exists());
    let dir = WorkDir::new();
    let dest = dir.path().join("stopped.db");

    stdout(athena_in(
        &dir,
        Some(&tmp),
        &["backup", dest.to_str().unwrap()],
    ));

    let id = cli_session(&tmp, "s");
    let copy = rusqlite::Connection::open(&dest).unwrap();
    assert_eq!(raw_rows(&copy, &id).len(), 4);
}

#[test]
fn the_binary_backs_up_an_older_schema_without_migrating_it() {
    let tmp = TempDb::new();
    tmp.raw()
        .execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY);
             INSERT INTO sessions VALUES ('old');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    let dir = WorkDir::new();
    let dest = dir.path().join("old.db");

    stdout(athena_in(
        &dir,
        Some(&tmp),
        &["backup", dest.to_str().unwrap()],
    ));

    let tables = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'";
    for db in [tmp.raw(), rusqlite::Connection::open(&dest).unwrap()] {
        assert_eq!(user_version(&db), 1);
        assert_eq!(count(&db, tables), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);
    }
}

#[test]
fn a_failed_backup_says_why_and_leaves_no_new_file() {
    let dir = WorkDir::new();
    let missing = dir.path().join("missing.db");
    let not_a_file = dir.path().join("not-a-dir");
    std::fs::write(&not_a_file, "a file").unwrap();
    let taken = dir.path().join("taken.db");
    std::fs::write(&taken, "keep").unwrap();
    let not_a_db = TempDb::new();
    std::fs::write(not_a_db.path(), "this is not a database, just text").unwrap();
    let source = TempDb::new();
    source.raw().execute_batch("CREATE TABLE t (x)").unwrap();

    for (db, dest, why) in [
        // No ATHENA_DB: the default, agent.db in the working directory.
        (
            None,
            missing.clone(),
            "opening the database agent.db read-only",
        ),
        (Some(&not_a_db), missing.clone(), "backing up"),
        (Some(&source), not_a_file.join("x.db"), "creating"),
        (Some(&source), taken.clone(), "already exists"),
    ] {
        let out = athena_in(&dir, db, &["backup", dest.to_str().unwrap()]);
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(!out.status.success(), "{why}");
        assert!(stderr.contains(why), "{why}: {stderr}");
    }
    assert!(!missing.exists());
    assert!(!dir.path().join("agent.db").exists());
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "keep");
}
