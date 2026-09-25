//! Nothing stored is lost when the schema moves forward.
//!
//! Each fixture in tests/fixtures is a database as some earlier build left
//! it. Opening one with this build must migrate it without touching a stored
//! message, and the next turn must not rewrite them either. Adding a
//! migration means adding a fixture of the schema before it; see TESTING.md.

mod common;

use athena::store::{self, Store};
use common::*;
use rig_core::test_utils::MockTurn;
use rusqlite::Connection;
use rusqlite::types::Value;

fn from_fixture(sql: &str) -> TempDb {
    let tmp = TempDb::new();
    tmp.raw().execute_batch(sql).unwrap();
    tmp
}

const V0_MAIN: &str = include_str!("fixtures/v0_main.sql");
const V0_RUN_OBSERVABILITY: &str = include_str!("fixtures/v0_run_observability.sql");
const V2_RUN_TELEMETRY: &str = include_str!("fixtures/v2_run_telemetry.sql");
const V3_USERS_SESSIONS: &str = include_str!("fixtures/v3_users_sessions.sql");
const V4_SELECTED_SESSIONS: &str = include_str!("fixtures/v4_selected_sessions.sql");

/// Every row of a table, every column, in rowid order, as SQLite holds it.
fn dump(db: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut q = db
        .prepare(&format!("SELECT rowid, * FROM {table} ORDER BY rowid"))
        .unwrap();
    let width = q.column_count();
    q.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Session id, name, owner, for every session.
fn owners(db: &Connection) -> Vec<(String, String, String)> {
    db.prepare(
        "SELECT s.id, s.name, u.transport || ':' || u.external_id
         FROM sessions s JOIN users u ON u.id = s.user_id ORDER BY s.id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn owned_by_cli(ids: &[&str]) -> Vec<(String, String, String)> {
    ids.iter()
        .map(|id| (id.to_string(), id.to_string(), "cli:local".to_string()))
        .collect()
}

#[tokio::test]
async fn a_database_from_main_upgrades_without_losing_a_message() {
    let tmp = from_fixture(V0_MAIN);
    let original = raw_rows(&tmp.raw(), "testsess");
    assert_eq!(original.len(), 8);

    let (service, _) = tmp.service();
    let db = tmp.raw();

    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(raw_rows(&db, "testsess"), original);
    assert_eq!(owners(&db), owned_by_cli(&["testsess"]));
    // Rows written by an older Rig still parse as today's Message.
    let user = cli_user(&service).await;
    assert_eq!(service.history(&user, "testsess").await.unwrap().len(), 8);

    // The next turn appends through Rig's memory. If it ever rewrote the
    // session, a lossy parse of old JSON would show up here.
    let (agent, _) = mock_agent(&service, [MockTurn::text("42")]);
    service
        .send(&agent, &user, "testsess", "what was the result?")
        .await
        .unwrap();

    let after = raw_rows(&db, "testsess");
    assert_eq!(after.len(), 10);
    assert_eq!(&after[..8], &original[..]);
    assert_eq!(runs(&db, "testsess"), [run_row(8, 9, 1, "ok")]);
}

#[tokio::test]
async fn a_database_from_before_migrations_keeps_its_runs() {
    let tmp = from_fixture(V0_RUN_OBSERVABILITY);
    let original = raw_rows(&tmp.raw(), "testsess");

    let (service, _) = tmp.service();
    let db = tmp.raw();

    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(raw_rows(&db, "testsess"), original);
    assert_eq!(runs(&db, "testsess"), [run_row(0, 3, 2, "ok")]);
    let totals = &service.usage(&cli_user(&service).await).await.unwrap()[0];
    assert_eq!((totals.input_tokens, totals.output_tokens), (256, 26));
}

/// The upgrade this build adds: main at 74095ac, schema version 2, three
/// sessions written by the real binary against OpenRouter.
#[tokio::test]
async fn a_database_at_schema_2_gains_owners_without_changing_a_row() {
    let tmp = from_fixture(V2_RUN_TELEMETRY);
    let (messages, run_rows) = {
        let db = tmp.raw();
        assert_eq!(user_version(&db), 2);
        (dump(&db, "messages"), dump(&db, "runs"))
    };
    assert_eq!(messages.len(), 14);
    assert_eq!(run_rows.len(), 3);

    let (service, _) = tmp.service();
    let db = tmp.raw();

    // Every message and every run, every column and rowid, as it was.
    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(dump(&db, "messages"), messages);
    assert_eq!(dump(&db, "runs"), run_rows);
    // Every session has an owner, including one that only ever failed.
    assert_eq!(
        owners(&db),
        owned_by_cli(&["broken", "research", "testsess"])
    );
    let user = cli_user(&service).await;
    let listed: Vec<(String, i64)> = service
        .sessions(&user)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.session.name, s.messages))
        .collect();
    assert_eq!(
        listed,
        [
            ("broken".into(), 0),
            ("research".into(), 2),
            ("testsess".into(), 12)
        ]
    );
    // The reasoning block and tool call still parse as today's Message.
    assert_eq!(service.history(&user, "testsess").await.unwrap().len(), 12);

    // A turn in the migrated session appends after the old rows.
    let (agent, _) = mock_agent(&service, [MockTurn::text("5")]);
    service
        .send(&agent, &user, "testsess", "and the result?")
        .await
        .unwrap();

    let now = dump(&db, "messages");
    assert_eq!(now.len(), 16);
    assert_eq!(&now[..14], &messages[..]);
    assert_eq!(&dump(&db, "runs")[..3], &run_rows[..]);
    assert_eq!(
        runs(&db, "testsess").last().unwrap(),
        &run_row(12, 13, 1, "ok")
    );

    // And the session that had only a failed run starts at seq 0.
    let (agent, _) = mock_agent(&service, [MockTurn::text("hi")]);
    service
        .send(&agent, &user, "broken", "hello")
        .await
        .unwrap();
    assert_eq!(runs(&db, "broken").last().unwrap(), &run_row(0, 1, 1, "ok"));
}

/// The upgrade this build adds: main at 040beeb, schema version 3, users
/// and sessions, two of them Telegram users. Migration 4 only adds
/// `selected_sessions`; every existing row of every table must survive it.
#[tokio::test]
async fn a_database_at_schema_3_gains_session_selection_without_changing_a_row() {
    const TABLES: [&str; 4] = ["messages", "runs", "users", "sessions"];
    let tmp = from_fixture(V3_USERS_SESSIONS);
    let before: Vec<Vec<Vec<Value>>> = {
        let db = tmp.raw();
        assert_eq!(user_version(&db), 3);
        TABLES.iter().map(|t| dump(&db, t)).collect()
    };
    let sizes: Vec<usize> = before.iter().map(Vec::len).collect();
    assert_eq!(sizes, [14, 3, 3, 6]);

    let (service, _) = tmp.service();
    let db = tmp.raw();

    // Every row of every table, every column and rowid, as it was.
    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    let after: Vec<_> = TABLES.iter().map(|t| dump(&db, t)).collect();
    assert_eq!(after, before);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM selected_sessions"), 0);

    // An existing Telegram user selects an existing session and talks in it.
    let store = tmp.open();
    let user = service.user("telegram", "111111").await.unwrap();
    let notes = session(&service, &user, "notes").await;
    assert!(store.select_session(&user, &notes.id).unwrap());
    assert_eq!(store.selected_session(&user).unwrap(), Some(notes.clone()));
    let (agent, _) = mock_agent(&service, [MockTurn::text("noted")]);
    service
        .send(&agent, &user, &notes.id, "hello")
        .await
        .unwrap();

    // The old rows are still exactly as they were; only new ones were added.
    let now: Vec<_> = TABLES.iter().map(|t| dump(&db, t)).collect();
    for ((table, old), new) in TABLES.iter().zip(&before).zip(&now) {
        assert_eq!(&new[..old.len()], &old[..], "{table}");
    }
    assert_eq!(now[0].len(), 16);
    assert_eq!(runs(&db, &notes.id), [run_row(0, 1, 1, "ok")]);
    // The cli user's migrated sessions are untouched and still theirs.
    assert_eq!(
        owners(&db)
            .into_iter()
            .filter(|o| o.2 == "cli:local")
            .collect::<Vec<_>>(),
        owned_by_cli(&["broken", "research", "testsess"])
    );
}

/// The upgrade this build adds: schema version 4, with a Telegram user's
/// selected session. Migration 5 only adds `sandboxes`; every existing row
/// of every table must survive it, and a turn must still work.
#[tokio::test]
async fn a_database_at_schema_4_gains_sandboxes_without_changing_a_row() {
    const TABLES: [&str; 5] = ["messages", "runs", "users", "sessions", "selected_sessions"];
    let tmp = from_fixture(V4_SELECTED_SESSIONS);
    let before: Vec<Vec<Vec<Value>>> = {
        let db = tmp.raw();
        assert_eq!(user_version(&db), 4);
        TABLES.iter().map(|t| dump(&db, t)).collect()
    };
    let sizes: Vec<usize> = before.iter().map(Vec::len).collect();
    assert_eq!(sizes, [14, 3, 3, 6, 1]);

    let (service, _) = tmp.service();
    let db = tmp.raw();

    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    let after: Vec<_> = TABLES.iter().map(|t| dump(&db, t)).collect();
    assert_eq!(after, before);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sandboxes"), 0);

    // The selection survived and still reads back; a turn in it appends.
    let store = tmp.open();
    let user = service.user("telegram", "111111").await.unwrap();
    let notes = store.selected_session(&user).unwrap().unwrap();
    assert_eq!(notes.name, "notes");
    let (agent, _) = mock_agent(&service, [MockTurn::text("noted")]);
    service
        .send(&agent, &user, &notes.id, "hello")
        .await
        .unwrap();

    let now: Vec<_> = TABLES.iter().map(|t| dump(&db, t)).collect();
    for ((table, old), new) in TABLES.iter().zip(&before).zip(&now) {
        assert_eq!(&new[..old.len()], &old[..], "{table}");
    }
    assert_eq!(runs(&db, &notes.id), [run_row(0, 1, 1, "ok")]);
}

#[test]
fn reopening_a_current_database_changes_nothing() {
    let tmp = from_fixture(V3_USERS_SESSIONS);
    drop(tmp.open());
    let first = describe_schema(&tmp.raw());
    let rows = dump(&tmp.raw(), "messages");
    let sessions = owners(&tmp.raw());

    drop(tmp.open());
    let db = tmp.raw();

    assert_eq!(describe_schema(&db), first);
    assert_eq!(dump(&db, "messages"), rows);
    assert_eq!(owners(&db), sessions);
}

#[test]
fn processes_racing_to_create_a_database_all_succeed() {
    let tmp = TempDb::new();
    let path = tmp.path().to_string();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || Store::open(&path).map(|_| ()))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    let db = tmp.raw();
    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM users"), 1);
}

#[test]
fn processes_racing_to_open_one_new_session_all_get_it() {
    let tmp = TempDb::new();
    drop(tmp.open());
    let path = tmp.path().to_string();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                let user = store.user("telegram", "42").unwrap();
                store.open_session(&user, "default").unwrap().id
            })
        })
        .collect();
    let ids: std::collections::BTreeSet<String> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert_eq!(ids.len(), 1, "{ids:?}");
    let db = tmp.raw();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM sessions"), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM users"), 2);
}

/// The schema a fresh database gets: every table's columns, foreign keys and
/// indexes (the automatic ones behind UNIQUE included), and every trigger.
///
/// Any schema change fails this test, so it shows up as a reviewed diff to
/// tests/fixtures/schema.txt rather than slipping through.
#[test]
fn the_schema_matches_the_reviewed_snapshot() {
    let tmp = TempDb::new();
    drop(tmp.open());
    let actual = describe_schema(&tmp.raw());
    let expected = include_str!("fixtures/schema.txt");
    assert_eq!(
        actual, expected,
        "schema changed. If intended, add a migration and a fixture \
         (TESTING.md), then replace tests/fixtures/schema.txt with:\n{actual}"
    );
}

fn strings(db: &Connection, sql: &str, arg: &str) -> Vec<String> {
    db.prepare(sql)
        .unwrap()
        .query_map([arg], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn describe_schema(db: &Connection) -> String {
    let mut out = format!("user_version {}\n", user_version(db));
    let objects: Vec<(String, String, String, String)> = db
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE type IN ('table', 'trigger') AND name NOT LIKE 'sqlite_%'
             ORDER BY type, name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    for (kind, name, table, sql) in objects {
        if kind == "trigger" {
            let sql = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            out += &format!("\ntrigger {name} ON {table}\n  {sql}\n");
            continue;
        }
        out += &format!("\ntable {name}\n");
        let mut q = db
            .prepare(
                "SELECT name, type, \"notnull\", dflt_value, pk
                 FROM pragma_table_info(?1) ORDER BY cid",
            )
            .unwrap();
        let cols = q
            .query_map([&name], |r| {
                Ok(format!(
                    "  {} {}{}{}{}\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    if r.get::<_, bool>(2)? {
                        " NOT NULL"
                    } else {
                        ""
                    },
                    r.get::<_, Option<String>>(3)?
                        .map(|d| format!(" DEFAULT {d}"))
                        .unwrap_or_default(),
                    match r.get::<_, i64>(4)? {
                        0 => String::new(),
                        n => format!(" PK{n}"),
                    },
                ))
            })
            .unwrap();
        for col in cols {
            out += &col.unwrap();
        }
        for fk in strings(
            db,
            "SELECT \"from\" || ' REFERENCES ' || \"table\" || ' (' || \"to\" || ')'
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
            &name,
        ) {
            out += &format!("  foreign key {fk}\n");
        }
        // origin: c = CREATE INDEX, u = UNIQUE constraint, pk = PRIMARY KEY.
        let indexes: Vec<(String, bool, String)> = db
            .prepare(
                "SELECT name, \"unique\", origin FROM pragma_index_list(?1) ORDER BY origin, name",
            )
            .unwrap()
            .query_map([&name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for (index, unique, origin) in indexes {
            let cols = strings(
                db,
                "SELECT name FROM pragma_index_info(?1) ORDER BY seqno",
                &index,
            );
            // Automatic index names carry a per-database counter; the origin
            // and columns are what matters.
            let label = if index.starts_with("sqlite_autoindex") {
                String::new()
            } else {
                format!(" {index}")
            };
            out += &format!(
                "  {}index{label} ({}) from {origin}\n",
                if unique { "unique " } else { "" },
                cols.join(", ")
            );
        }
    }
    out
}
