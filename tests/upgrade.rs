//! Nothing stored is lost when the schema moves forward.
//!
//! Each fixture in tests/fixtures is a database as some earlier build left
//! it. Opening one with this build must migrate it without touching a stored
//! message, and the next turn must not rewrite them either. Adding a
//! migration means adding a fixture of the schema before it; see TESTING.md.

mod common;

use athena::{runner, store};
use common::*;
use rig_core::test_utils::MockTurn;
use rusqlite::Connection;

fn from_fixture(sql: &str) -> TempDb {
    let tmp = TempDb::new();
    tmp.raw().execute_batch(sql).unwrap();
    tmp
}

const V0_MAIN: &str = include_str!("fixtures/v0_main.sql");
const V0_RUN_OBSERVABILITY: &str = include_str!("fixtures/v0_run_observability.sql");

#[tokio::test]
async fn a_database_from_main_upgrades_without_losing_a_message() {
    let tmp = from_fixture(V0_MAIN);
    let original = raw_rows(&tmp.raw(), "testsess");
    assert_eq!(original.len(), 8);

    let db = tmp.open();

    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(raw_rows(&db, "testsess"), original);
    // Rows written by an older Rig still parse as today's Message.
    assert_eq!(store::load(&db, "testsess").unwrap().len(), 8);

    // save() deletes and rewrites the whole session each turn. If Rig ever
    // parsed old JSON lossily, this is where the old rows would change.
    let (agent, _) = mock_agent([MockTurn::text("42")]);
    runner::turn(&agent, &db, "testsess", "m", "what was the result?")
        .await
        .unwrap();

    let after = raw_rows(&db, "testsess");
    assert_eq!(after.len(), 10);
    assert_eq!(&after[..8], &original[..]);
    assert_eq!(runs(&db, "testsess"), [run_row(8, 9, 1, "ok")]);
}

#[test]
fn a_database_from_before_migrations_keeps_its_runs() {
    let tmp = from_fixture(V0_RUN_OBSERVABILITY);
    let original = raw_rows(&tmp.raw(), "testsess");

    let db = tmp.open();

    assert_eq!(user_version(&db), store::SCHEMA_VERSION as i64);
    assert_eq!(raw_rows(&db, "testsess"), original);
    assert_eq!(runs(&db, "testsess"), [run_row(0, 3, 2, "ok")]);
    let totals = &store::usage(&db).unwrap()[0];
    assert_eq!((totals.input_tokens, totals.output_tokens), (256, 26));
}

#[test]
fn reopening_a_current_database_changes_nothing() {
    let tmp = from_fixture(V0_MAIN);
    let first = describe_schema(&tmp.open());
    let rows = raw_rows(&tmp.raw(), "testsess");

    let db = tmp.open();

    assert_eq!(describe_schema(&db), first);
    assert_eq!(raw_rows(&db, "testsess"), rows);
}

#[test]
fn processes_racing_to_create_a_database_all_succeed() {
    let tmp = TempDb::new();
    let path = tmp.path().to_string();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || store::open(&path).map(|_| ()))
        })
        .collect();
    for h in handles {
        h.join().unwrap().unwrap();
    }
    assert_eq!(user_version(&tmp.raw()), store::SCHEMA_VERSION as i64);
}

/// The schema a fresh database gets, column by column and index by index.
///
/// Any schema change fails this test, so it shows up as a reviewed diff to
/// tests/fixtures/schema.txt rather than slipping through.
#[test]
fn the_schema_matches_the_reviewed_snapshot() {
    let tmp = TempDb::new();
    let actual = describe_schema(&tmp.open());
    let expected = include_str!("fixtures/schema.txt");
    assert_eq!(
        actual, expected,
        "schema changed. If intended, add a migration and a fixture \
         (TESTING.md), then replace tests/fixtures/schema.txt with:\n{actual}"
    );
}

fn describe_schema(db: &Connection) -> String {
    let mut out = format!("user_version {}\n", user_version(db));
    let objects: Vec<(String, String, String)> = db
        .prepare(
            "SELECT type, name, tbl_name FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type DESC, name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    for (kind, name, table) in objects {
        if kind == "table" {
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
        } else {
            let cols: Vec<String> = db
                .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")
                .unwrap()
                .query_map([&name], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            out += &format!("\n{kind} {name} ON {table} ({})\n", cols.join(", "));
        }
    }
    out
}
