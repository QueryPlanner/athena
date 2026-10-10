//! `health_export` through the real agent loop, against the fake sandbox
//! server (`sandbox/fake_server.rs`) and a real database: what is refused,
//! what the sandbox is sent, and what the model is told.

mod common;
// This binary uses only part of the fake; the others use the rest.
#[path = "sandbox/fake_server.rs"]
#[allow(dead_code)]
mod fake_server;

use athena::agent;
use athena::custom::Custom;
use athena::health::{data, export};
use athena::mcp::Mcp;
use athena::runner::Request;
use athena::sandbox::Sandboxes;
use athena::service::{Service, User};
use athena::store::{PointRow, Store};
use common::*;
use fake_server::{FakeSandbox, config, printed};
use jiff::Timestamp;
use rig_agent::agent::AgentBuilder;
use rig_core::completion::CompletionRequest;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};
use std::sync::Arc;

const HR: &str = "heart-rate";
const STEPS: &str = "steps";
const DATA_DIR: &str = "/tmp/athena-data";
const DB_PATH: &str = "/tmp/athena-data/health.sqlite";

struct World {
    tmp: TempDb,
    service: Service,
    user: User,
    session: String,
    fake: FakeSandbox,
    sandboxes: Arc<Sandboxes>,
}

async fn world() -> World {
    let fake = FakeSandbox::start().await;
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let sandboxes = Arc::new(Sandboxes::new(config(&fake.url), tmp.open()));
    let user = service.user("telegram", "7").await.unwrap();
    let session = session(&service, &user, "s").await.id;
    World {
        tmp,
        service,
        user,
        session,
        fake,
        sandboxes,
    }
}

impl World {
    fn store(&self) -> Store {
        self.tmp.open()
    }

    fn put(&self, data_type: &str, rows: &[PointRow]) {
        self.store()
            .health_points_put(self.user.id(), data_type, rows, Timestamp::UNIX_EPOCH)
            .unwrap();
    }

    /// One conversation: `turns` in front of the production agent, with
    /// `sandboxes` (or none), and the scripted model returned.
    async fn converse(
        &self,
        sandboxes: Option<Arc<Sandboxes>>,
        turns: Vec<MockTurn>,
        request: Request,
    ) -> MockCompletionModel {
        let model = MockCompletionModel::new(turns);
        let agent = agent::configure_persistent(
            AgentBuilder::new(model.clone()).memory(self.service.memory()),
            sandboxes,
            &Custom::default(),
            &Mcp::none(),
            self.store(),
            None,
        );
        self.service
            .send(&agent, &self.user, &self.session, request)
            .await
            .unwrap();
        model
    }

    /// One turn in which the model calls `export` with `args`, sent as the
    /// user's typed message or as a scheduled turn; returns what it answered.
    async fn export(&self, args: Value, request: Request) -> String {
        let model = self
            .converse(
                Some(self.sandboxes.clone()),
                vec![
                    MockTurn::tool_call("call_1", data::EXPORT, args),
                    MockTurn::text("done"),
                ],
                request,
            )
            .await;
        tool_result(&model.requests()[1])
    }

    /// The commands the sandbox was sent, in order.
    fn commands(&self) -> Vec<Value> {
        self.fake
            .requests_to("POST", "/command")
            .into_iter()
            .map(|r| r.body)
            .collect()
    }

    fn command_text(&self, index: usize) -> String {
        self.commands()[index]["command"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The export directory the first command created.
    fn export_dir(&self) -> String {
        dir_in(&self.command_text(0))
    }

    /// Queue the sandbox answers for an export that succeeds: `mkdir`, then
    /// the loader with `report`.
    fn queue_success(&self, report: &Value) {
        self.fake.reply_next(printed("created\n"));
        self.fake.reply_next(printed(&report.to_string()));
    }
}

/// What a tool result says, as the model was sent it.
fn tool_result(request: &CompletionRequest) -> String {
    let last = serde_json::to_value(request.chat_history.last().unwrap()).unwrap();
    let content = &last["content"][0];
    assert_eq!(content["type"], "toolresult", "{last}");
    let item = &content["content"][0];
    match item["type"].as_str() {
        Some("json") => item["value"].to_string(),
        _ => item["text"].as_str().unwrap().to_string(),
    }
}

const SECRET: &str = "SECRET-VALUE-4411";

/// The export directory named in `text`: the prefix and 32 hex digits.
fn dir_in(text: &str) -> String {
    const PREFIX: &str = "/tmp/athena-data/.export-";
    let start = text.find(PREFIX).unwrap();
    text[start..start + PREFIX.len() + 32].to_string()
}

fn heart(bpm: &str) -> String {
    json!({"heartRate": {"beatsPerMinute": bpm}}).to_string()
}

fn row(key: &str, start: i64, civil: &str, value: &str) -> PointRow {
    PointRow {
        key: key.into(),
        start_ms: Some(start),
        end_ms: Some(start + 60_000),
        civil_date: Some(civil.into()),
        value: value.into(),
        source: Some("fitbit".into()),
    }
}

fn t0() -> i64 {
    "2026-03-10T08:00:00Z"
        .parse::<Timestamp>()
        .unwrap()
        .as_millisecond()
}

/// A short error event, as execd streams one.
fn error_event(name: &str, value: &str) -> String {
    format!("{{\"type\":\"error\",\"error\":{{\"ename\":\"{name}\",\"evalue\":\"{value}\"}}}}\n\n")
}

// ----------------------------------------------------------- refusals

#[tokio::test]
async fn a_scheduled_export_is_refused_before_anything_reaches_the_sandbox() {
    let w = world().await;
    w.put(HR, &[row("a", t0(), "2026-03-10", &heart("70"))]);
    let answer = w
        .export(
            json!({}),
            Request {
                text: "go".into(),
                scheduled: true,
                ..Request::default()
            },
        )
        .await;
    assert!(answer.contains("not in a scheduled task"), "{answer}");
    assert!(w.fake.requests().is_empty(), "the sandbox was sent nothing");
}

#[tokio::test]
async fn without_a_sandbox_the_tool_says_so_and_is_still_offered() {
    let w = world().await;
    w.put(HR, &[row("a", t0(), "2026-03-10", &heart("70"))]);
    let model = w
        .converse(
            None,
            vec![
                MockTurn::tool_call("call_1", data::EXPORT, json!({})),
                MockTurn::text("done"),
            ],
            Request::from("export it"),
        )
        .await;
    let offered = model.requests()[0]
        .tools
        .iter()
        .any(|t| t.name == data::EXPORT);
    assert!(offered, "the tool is registered without a sandbox");
    let answer = tool_result(&model.requests()[1]);
    assert!(answer.contains("not set up on this server"), "{answer}");
}

#[tokio::test]
async fn an_export_with_no_points_says_there_is_nothing_and_sends_nothing() {
    let w = world().await;
    let answer = w
        .export(json!({"types": [HR]}), Request::from("export"))
        .await;
    assert!(answer.contains("nothing to export"), "{answer}");
    assert!(w.fake.requests().is_empty());
}

#[tokio::test]
async fn an_export_over_the_size_cap_is_refused_before_any_command() {
    let w = world().await;
    // 135 points of a million bytes: 135 000 000 bytes, over the 128 MiB cap.
    let big = "a".repeat(1_000_000);
    let rows: Vec<PointRow> = (0..135)
        .map(|i| row(&format!("k{i}"), t0() + i * 60_000, "2026-03-10", &big))
        .collect();
    w.put(HR, &rows);
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(
        answer.contains("over the 128 MiB an export may hold"),
        "{answer}"
    );
    assert!(answer.contains("narrow `types`"), "{answer}");
    assert!(w.fake.requests().is_empty(), "no command and no upload");
}

// ------------------------------------------------------------ success

#[tokio::test]
async fn an_export_builds_the_database_and_reports_its_shape_without_any_value() {
    let w = world().await;
    w.put(
        HR,
        &[
            row("h1", t0(), "2026-03-10", &heart(SECRET)),
            row("h2", t0() + 60_000, "2026-03-10", &heart("71")),
            row("h3", t0() + 120_000, "2026-03-10", &heart("72")),
        ],
    );
    w.put(
        STEPS,
        &[row(
            "s1",
            t0(),
            "2026-03-10",
            "{\"steps\":{\"count\":\"5\"}}",
        )],
    );
    let report = json!({"rows": {"heart-rate": 3, "steps": 1}, "bytes": 4096});
    w.queue_success(&report);

    let answer = w
        .export(json!({}), Request::from("analyse my heart rate"))
        .await;
    assert!(!answer.contains(SECRET), "no stored value is in the result");
    let result: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(result["path"], DB_PATH);
    assert_eq!(result["rows"], report["rows"]);
    assert_eq!(result["total_rows"], 4);
    assert_eq!(result["file_bytes"], 4096);
    assert_eq!(result["timezone"], "Asia/Kolkata");
    assert!(result["from"].is_null() && result["to"].is_null());
    assert!(result["how_to_query"].as_str().unwrap().contains("python3"));
    assert!(
        result["expires"]
            .as_str()
            .unwrap()
            .contains("30 idle minutes")
    );
    assert!(
        result["privacy"]
            .as_str()
            .unwrap()
            .contains("internet access")
    );
    assert!(result["tables"]["points"].is_array());
    assert!(result["views"]["heart_rate"].is_array());
    assert!(result["columns"].is_string());
    assert!(result["meta"].is_string());
}

#[tokio::test]
async fn the_sandbox_is_sent_the_directory_setup_the_files_and_the_loader_in_order() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart("70"))]);
    w.put(
        STEPS,
        &[row(
            "s1",
            t0(),
            "2026-03-10",
            "{\"steps\":{\"count\":\"5\"}}",
        )],
    );
    w.queue_success(&json!({"rows": {"steps": 1, "heart-rate": 1}, "bytes": 1}));
    w.export(json!({"from": "2026-03-10"}), Request::from("export"))
        .await;

    let dir = w.export_dir();
    let name = dir.trim_start_matches(&format!("{DATA_DIR}/.export-"));
    assert_eq!(name.len(), 32);
    assert!(name.chars().all(|c| c.is_ascii_hexdigit()), "{dir}");

    let commands = w.commands();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(
        commands[0]["command"],
        format!("mkdir -p '{dir}' && chmod 700 '{DATA_DIR}' '{dir}'")
    );
    assert_eq!(commands[0]["timeout"], 30_000);
    assert_eq!(
        commands[1]["command"],
        format!("'python3' '{dir}/load.py' '{dir}' '{DB_PATH}'")
    );
    assert_eq!(commands[1]["timeout"], 900_000);

    assert_eq!(
        w.fake.file(&format!("{dir}/load.py")).unwrap(),
        export::LOADER.as_bytes()
    );
    let meta: Value =
        serde_json::from_slice(&w.fake.file(&format!("{dir}/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["zone"], "Asia/Kolkata");
    assert_eq!(meta["types"], json!([STEPS, HR]));
    assert_eq!(meta["from"], "2026-03-10");
    assert!(meta["to"].is_null());
    assert!(meta["exported_at"].as_str().unwrap().contains('T'));

    let chunk =
        String::from_utf8(w.fake.file(&format!("{dir}/chunk-00001.ndjson")).unwrap()).unwrap();
    let lines: Vec<Value> = chunk
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        for key in ["t", "k", "s", "e", "d", "src", "v"] {
            assert!(line.get(key).is_some(), "{key} in {line}");
        }
    }
    // Rows come out in the order they were stored, and each value is the
    // type's own field.
    assert_eq!(lines[0]["t"], HR);
    assert_eq!(lines[0]["v"], json!({"beatsPerMinute": "70"}));
    assert_eq!(lines[1]["t"], STEPS);
    assert_eq!(lines[1]["v"], json!({"count": "5"}));
}

#[tokio::test]
async fn each_export_gets_a_new_directory() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart("70"))]);
    w.queue_success(&json!({"rows": {}, "bytes": 1}));
    w.queue_success(&json!({"rows": {}, "bytes": 1}));
    w.export(json!({}), Request::from("first")).await;
    w.export(json!({}), Request::from("second")).await;
    let dirs: Vec<String> = [0, 2].iter().map(|&i| dir_in(&w.command_text(i))).collect();
    assert_ne!(dirs[0], dirs[1]);
}

#[tokio::test]
async fn a_last_chunk_that_empties_the_buffer_leaves_no_second_chunk() {
    let w = world().await;
    // Batches hold three of these points, so the second batch passes four
    // mebibytes and is uploaded; the batch after it is empty and nothing is
    // left to upload at the end.
    let note = "n".repeat(1_000_000);
    let rows: Vec<PointRow> = (0..6)
        .map(|i| {
            let value = json!({"heartRate": {"beatsPerMinute": "1", "note": &note}});
            row(
                &format!("k{i:02}"),
                t0() + i * 60_000,
                "2026-03-10",
                &value.to_string(),
            )
        })
        .collect();
    w.put(HR, &rows);
    w.queue_success(&json!({"rows": {"heart-rate": 6}, "bytes": 1}));
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(answer.contains(DB_PATH), "{answer}");

    let dir = w.export_dir();
    assert!(w.fake.file(&format!("{dir}/chunk-00001.ndjson")).is_some());
    assert!(w.fake.file(&format!("{dir}/chunk-00002.ndjson")).is_none());
}

#[tokio::test]
async fn a_large_export_is_uploaded_in_chunks_of_about_four_mebibytes() {
    let w = world().await;
    let note = "n".repeat(1_000_000);
    let rows: Vec<PointRow> = (0..10)
        .map(|i| {
            let value = json!({"heartRate": {"beatsPerMinute": "1", "note": &note}});
            row(
                &format!("k{i:02}"),
                t0() + i * 60_000,
                "2026-03-10",
                &value.to_string(),
            )
        })
        .collect();
    w.put(HR, &rows);
    w.queue_success(&json!({"rows": {"heart-rate": 10}, "bytes": 1}));
    w.export(json!({}), Request::from("export")).await;

    let dir = w.export_dir();
    let first = w.fake.file(&format!("{dir}/chunk-00001.ndjson")).unwrap();
    let second = w.fake.file(&format!("{dir}/chunk-00002.ndjson")).unwrap();
    assert!(first.len() >= 4 * 1024 * 1024, "{}", first.len());
    assert!(w.fake.file(&format!("{dir}/chunk-00003.ndjson")).is_none());
    let mut keys = Vec::new();
    for body in [first, second] {
        for line in String::from_utf8(body).unwrap().lines() {
            let point: Value = serde_json::from_str(line).unwrap();
            keys.push(point["k"].as_str().unwrap().to_string());
        }
    }
    let expected: Vec<String> = (0..10).map(|i| format!("k{i:02}")).collect();
    assert_eq!(
        keys, expected,
        "every point once, in the order they were stored"
    );
}

#[tokio::test]
async fn the_whole_conversation_sizes_then_reads_points_then_exports() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart("70"))]);
    w.queue_success(&json!({"rows": {"heart-rate": 1}, "bytes": 9}));
    let model = w
        .converse(
            Some(w.sandboxes.clone()),
            vec![
                MockTurn::tool_call("c1", data::SIZE, json!({})),
                MockTurn::tool_call("c2", data::POINTS, json!({"type": HR})),
                MockTurn::tool_call("c3", data::EXPORT, json!({"types": [HR]})),
                MockTurn::text("done"),
            ],
            Request::from("sleep analysis please"),
        )
        .await;
    let size: Value = serde_json::from_str(&tool_result(&model.requests()[1])).unwrap();
    assert_eq!(size["total_rows"], 1);
    let points = tool_result(&model.requests()[2]);
    assert!(points.contains("70"), "{points}");
    let export: Value = serde_json::from_str(&tool_result(&model.requests()[3])).unwrap();
    assert_eq!(export["path"], DB_PATH);
    assert_eq!(export["rows"]["heart-rate"], 1);
}

// ------------------------------------------------------------ failures

/// The one-command failure cases: the sandbox's answer for each command,
/// the message the model is told, and that the directory is removed after.
async fn assert_failed_and_cleaned(w: &World, answer: &str) {
    let dir = w.export_dir();
    let commands = w.commands();
    let last = commands.last().unwrap()["command"].as_str().unwrap();
    assert_eq!(last, format!("'rm' '-rf' '{dir}'"), "{commands:?}");
    assert!(!answer.contains(SECRET), "{answer}");
}

#[tokio::test]
async fn a_failed_mkdir_is_reported_and_its_directory_removed() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart(SECRET))]);
    w.fake
        .reply_next(error_event("OSError", "read-only file system"));
    w.fake.reply_next(printed("removed\n"));
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(
        answer.contains("creating /tmp/athena-data/.export-"),
        "{answer}"
    );
    assert!(answer.contains("read-only file system"), "{answer}");
    assert_failed_and_cleaned(&w, &answer).await;
    assert_eq!(w.commands().len(), 2, "nothing was uploaded or loaded");
}

#[tokio::test]
async fn a_failed_upload_is_reported_and_its_directory_removed() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart(SECRET))]);
    w.fake.reply_next(printed("created\n"));
    w.fake.reply_next(printed("removed\n"));
    w.fake.fail_next("/files/upload", 500, "the disk is full");
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(
        answer.contains("the disk is full") || answer.contains("500"),
        "{answer}"
    );
    assert_failed_and_cleaned(&w, &answer).await;
}

#[tokio::test]
async fn a_loader_error_is_reported_and_its_directory_removed() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart(SECRET))]);
    w.fake.reply_next(printed("created\n"));
    w.fake
        .reply_next(error_event("ValueError", "chunk-00001.ndjson line 3"));
    w.fake.reply_next(printed("removed\n"));
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(
        answer.contains("building the database in the sandbox failed"),
        "{answer}"
    );
    assert_failed_and_cleaned(&w, &answer).await;
}

#[tokio::test]
async fn a_loader_that_prints_nothing_is_reported_and_its_directory_removed() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart(SECRET))]);
    w.fake.reply_next(printed("created\n"));
    w.fake.reply_next(printed(""));
    w.fake.reply_next(printed("removed\n"));
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(answer.contains("gave no result"), "{answer}");
    assert_failed_and_cleaned(&w, &answer).await;
}

#[tokio::test]
async fn a_loader_that_prints_something_other_than_json_is_reported() {
    let w = world().await;
    w.put(HR, &[row("h1", t0(), "2026-03-10", &heart(SECRET))]);
    w.fake.reply_next(printed("created\n"));
    w.fake.reply_next(printed("done, probably\n"));
    w.fake.reply_next(printed("removed\n"));
    let answer = w.export(json!({}), Request::from("export")).await;
    assert!(answer.contains("gave no result"), "{answer}");
    assert_failed_and_cleaned(&w, &answer).await;
}
