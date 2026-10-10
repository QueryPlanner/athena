//! The loader that builds a `health_export` database in the sandbox
//! (`src/health/export_loader.py`), run as the sandbox runs it:
//! `python3 -I load.py WORKDIR DEST`, against fixture chunks on disk.
//!
//! Skipped with a message if `python3` is not installed; CI's Ubuntu runners
//! have it.

use athena::health::export;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("athena-loader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `python3` runs here. A test that needs it prints why and returns
/// when it does not.
fn python_available() -> bool {
    match Command::new("python3").arg("--version").output() {
        Ok(out) if out.status.success() => true,
        _ => {
            eprintln!("skipping: python3 is not installed");
            false
        }
    }
}

const META: &str = r#"{"exported_at":"2026-10-11T00:00:00Z","zone":"Asia/Kolkata",
    "types":["heart-rate","steps","weight","sleep","heart-rate-variability",
             "oxygen-saturation"],"from":null,"to":null}"#;

/// One chunk line, as Athena writes it.
fn line(
    t: &str,
    k: &str,
    s: Option<i64>,
    e: Option<i64>,
    d: Option<&str>,
    src: Option<&str>,
    v: Value,
) -> String {
    json!({"t": t, "k": k, "s": s, "e": e, "d": d, "src": src, "v": v}).to_string()
}

/// A workdir holding `meta` and each chunk, as Athena uploads them.
fn workdir(scratch: &Scratch, meta: &str, chunks: &[(&str, Vec<String>)]) -> PathBuf {
    let dir = scratch.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("meta.json"), meta).unwrap();
    for (name, lines) in chunks {
        std::fs::write(dir.join(name), lines.join("\n") + "\n").unwrap();
    }
    dir
}

fn fixture_chunks() -> Vec<(&'static str, Vec<String>)> {
    let first = vec![
        line(
            "heart-rate",
            "hr1",
            Some(1000),
            Some(1000),
            Some("2026-03-10"),
            Some("fitbit"),
            json!({"beatsPerMinute": "72"}),
        ),
        line(
            "steps",
            "st1",
            Some(1000),
            Some(61000),
            Some("2026-03-10"),
            None,
            json!({"count": "12"}),
        ),
        line(
            "weight",
            "w1",
            Some(2000),
            Some(2000),
            Some("2026-03-10"),
            Some("fitbit"),
            json!({"weightGrams": "72500"}),
        ),
        line(
            "sleep",
            "sl1",
            Some(0),
            Some(3_600_000),
            Some("2026-03-10"),
            Some("fitbit"),
            json!({"summary": {"minutesAsleep": "55"}}),
        ),
        line(
            "sleep",
            "sl2",
            Some(0),
            Some(120_000),
            None,
            None,
            json!({}),
        ),
        line(
            "heart-rate-variability",
            "h1",
            Some(5),
            None,
            Some("2026-03-10"),
            None,
            json!({"rootMeanSquareOfSuccessiveDifferencesMilliseconds": "42.5"}),
        ),
        line(
            "oxygen-saturation",
            "o1",
            Some(6),
            None,
            None,
            None,
            json!({"percentage": "97"}),
        ),
        // Non-ASCII text in a value and in a source survives.
        line(
            "heart-rate",
            "hr-uni",
            Some(7),
            Some(7),
            Some("2026-03-10"),
            Some("fitbit ✓ é"),
            json!({"beatsPerMinute": "70", "note": "ünïcode 日本"}),
        ),
    ];
    // The same key again: the later row replaces the earlier one.
    let second = vec![line(
        "heart-rate",
        "hr1",
        Some(1000),
        Some(1000),
        Some("2026-03-10"),
        Some("fitbit"),
        json!({"beatsPerMinute": "80"}),
    )];
    vec![
        ("chunk-00001.ndjson", first),
        ("chunk-00002.ndjson", second),
    ]
}

fn run(args: &[&str]) -> Output {
    Command::new("python3")
        .arg("-I")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/health/export_loader.py"
        ))
        .args(args)
        .output()
        .unwrap()
}

fn db(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

fn last_stdout_line(out: &Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn a_good_load_builds_the_tables_views_and_meta_and_reports_the_counts() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let dir = workdir(&scratch, META, &fixture_chunks());
    let dest = scratch.path().join("out").join("health.sqlite");
    let out = run(&[dir.to_str().unwrap(), dest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report = last_stdout_line(&out);
    assert_eq!(report["rows"]["heart-rate"], 2, "{report}");
    assert_eq!(report["rows"]["steps"], 1);
    assert_eq!(report["rows"]["sleep"], 2);
    assert!(report["bytes"].as_u64().unwrap() > 0);

    let conn = db(&dest);
    // A later row with the same key replaced the earlier one.
    let bpm: f64 = conn
        .query_row(
            "SELECT bpm FROM heart_rate WHERE civil_date = '2026-03-10' AND start_ms = 1000",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bpm, 80.0);
    let steps: (i64, String) = conn
        .query_row("SELECT count, typeof(count) FROM steps", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(steps, (12, "integer".into()));
    let kg: f64 = conn
        .query_row("SELECT kg FROM weight", [], |r| r.get(0))
        .unwrap();
    assert!((kg - 72.5).abs() < 1e-9);
    let minutes: Vec<i64> = conn
        .prepare("SELECT minutes FROM sleep ORDER BY start_ms, minutes")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(minutes, [2, 55], "no summary falls back to the span");
    let rmssd: f64 = conn
        .query_row("SELECT rmssd_ms FROM hrv", [], |r| r.get(0))
        .unwrap();
    assert!((rmssd - 42.5).abs() < 1e-9);
    let percent: f64 = conn
        .query_row("SELECT percent FROM spo2", [], |r| r.get(0))
        .unwrap();
    assert!((percent - 97.0).abs() < 1e-9);
    let (source, note): (String, String) = conn
        .query_row(
            "SELECT source, json_extract(value, '$.note') FROM points WHERE point_key = 'hr-uni'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(source, "fitbit ✓ é");
    assert_eq!(note, "ünïcode 日本");

    let meta: Vec<(String, String)> = conn
        .prepare("SELECT key, value FROM meta ORDER BY key")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let keys: Vec<&str> = meta.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        ["exported_at", "rows", "types", "zone"],
        "from and to are null, so not stored"
    );
    let rows_meta = &meta.iter().find(|(k, _)| k == "rows").unwrap().1;
    let rows_meta: Value = serde_json::from_str(rows_meta).unwrap();
    assert_eq!(rows_meta["steps"], 1);
    let zone = &meta.iter().find(|(k, _)| k == "zone").unwrap().1;
    assert_eq!(zone, "Asia/Kolkata");
}

#[test]
fn the_database_is_mode_0600_and_the_workdir_is_removed() {
    if !python_available() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let dir = workdir(&scratch, META, &fixture_chunks());
    let dest = scratch.path().join("health.sqlite");
    assert!(
        run(&[dir.to_str().unwrap(), dest.to_str().unwrap()])
            .status
            .success()
    );
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(!dir.exists(), "the chunks and meta are removed");
    assert!(!dest.with_extension("tmp").exists());
}

#[test]
fn a_failed_load_leaves_an_existing_destination_untouched_and_removes_the_workdir() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let mut chunks = fixture_chunks();
    chunks[0].1.push("this is not json".into());
    let dir = workdir(&scratch, META, &chunks);
    let dest = scratch.path().join("health.sqlite");
    std::fs::write(&dest, "the previous export").unwrap();
    let out = run(&[dir.to_str().unwrap(), dest.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("chunk-00001.ndjson line 9"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "the previous export"
    );
    assert!(!dir.exists());
}

#[test]
fn a_missing_meta_file_fails_and_removes_the_workdir() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let dir = scratch.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("chunk-00001.ndjson"), "").unwrap();
    let dest = scratch.path().join("health.sqlite");
    let out = run(&[dir.to_str().unwrap(), dest.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("meta.json"));
    assert!(!dest.exists());
    assert!(!dir.exists());
}

#[test]
fn a_line_missing_a_field_is_reported_with_its_chunk_and_line() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let dir = workdir(
        &scratch,
        META,
        &[(
            "chunk-00001.ndjson",
            vec![json!({"t": "steps"}).to_string()],
        )],
    );
    let dest = scratch.path().join("health.sqlite");
    let out = run(&[dir.to_str().unwrap(), dest.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("chunk-00001.ndjson line 1"), "{stderr}");
    assert!(!dest.exists());
}

#[test]
fn blank_lines_in_a_chunk_are_skipped() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let good = line(
        "steps",
        "s1",
        Some(1),
        Some(2),
        Some("2026-03-10"),
        None,
        json!({"count": "3"}),
    );
    let dir = workdir(
        &scratch,
        META,
        &[(
            "chunk-00001.ndjson",
            vec![String::new(), good, "   ".into()],
        )],
    );
    let dest = scratch.path().join("health.sqlite");
    let out = run(&[dir.to_str().unwrap(), dest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(last_stdout_line(&out)["rows"]["steps"], 1);
}

#[test]
fn the_wrong_number_of_arguments_is_a_usage_error() {
    if !python_available() {
        return;
    }
    let out = run(&["only-one"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: load.py WORKDIR DEST"));
}

#[test]
fn the_loader_is_the_one_athena_uploads() {
    assert!(export::LOADER.contains("def build(workdir, dest):"));
    let on_disk = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/health/export_loader.py"
    ))
    .unwrap();
    assert_eq!(export::LOADER, on_disk);
}

#[test]
fn the_schema_the_result_reports_matches_the_built_database() {
    if !python_available() {
        return;
    }
    let scratch = Scratch::new();
    let dir = workdir(&scratch, META, &fixture_chunks());
    let dest = scratch.path().join("health.sqlite");
    assert!(
        run(&[dir.to_str().unwrap(), dest.to_str().unwrap()])
            .status
            .success()
    );
    let conn = db(&dest);
    let columns = |name: &str| -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({name})"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let schema = export::schema();
    for (table, expected) in schema["tables"].as_object().unwrap() {
        let expected: Vec<&str> = expected
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(columns(table), expected, "table {table}");
    }
    for (view, expected) in schema["views"].as_object().unwrap() {
        let expected: Vec<&str> = expected
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(columns(view), expected, "view {view}");
        let kind: String = conn
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = ?1",
                [view],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "view", "{view}");
    }
}
