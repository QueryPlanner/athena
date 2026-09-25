//! `athena eval`, driven in-process through `eval::main` with scripted
//! models, and as the real binary for the committed starter dataset. No
//! network beyond loopback.

mod common;

use athena::eval::{self, report};
use athena::service::Service;
use athena::store::Store;
use common::*;
use rig_agent::agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const STARTER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/evals/cases");

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// A model factory for commands that must not build a real model.
fn no_model(name: &str) -> anyhow::Result<MockCompletionModel> {
    anyhow::bail!("this command must not build a model (asked for {name})")
}

/// Run `athena eval <list>` in-process; returns the result and stdout.
async fn run_with(
    list: &[&str],
    model: impl Fn(&str) -> anyhow::Result<MockCompletionModel>,
) -> (anyhow::Result<()>, String) {
    let mut out = Vec::new();
    let result = eval::main(&args(list), "agent/model", model, &mut out).await;
    (result, String::from_utf8(out).unwrap())
}

async fn offline(list: &[&str]) -> (anyhow::Result<()>, String) {
    run_with(list, no_model).await
}

/// A dataset directory with `cases/` and `cassettes/`, removed on drop.
struct Dataset(PathBuf);

impl Dataset {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("athena-evals-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("cases")).unwrap();
        Self(dir)
    }

    fn case(&self, case: serde_json::Value) -> &Self {
        let id = case["eval_case_id"].as_str().unwrap();
        std::fs::write(self.0.join(format!("cases/{id}.json")), case.to_string()).unwrap();
        self
    }

    fn cases(&self) -> String {
        self.0.join("cases").display().to_string()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dataset {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn add_case(gate: bool) -> serde_json::Value {
    json!({
        "eval_case_id": "add", "kind": "trajectory", "tags": ["tools"],
        "turns": ["add 21 and 21"],
        "expect": {"trajectory": {"tools": ["add"]}, "output": {"contains": ["42"]}, "rubric": "says 42"},
        "thresholds": {"gate": gate}
    })
}

fn rows(path: &Path) -> Vec<report::Row> {
    report::read_rows(path).unwrap()
}

#[tokio::test]
async fn the_starter_dataset_passes_on_replay_and_writes_one_row_per_sample() {
    let data = Dataset::new();
    let out_file = data.file("out/results.jsonl");
    let out = out_file.display().to_string();
    let (result, printed) = offline(&[
        "run", "--target", "replay", "--cases", STARTER, "--k", "2", "--out", &out,
    ])
    .await;
    result.unwrap();
    assert!(
        printed.contains("4/4 cases met their threshold; 0 gating failures"),
        "{printed}"
    );

    let rows = rows(&out_file);
    assert_eq!(rows.len(), 8);
    let add = rows.iter().find(|r| r.case_id == "add_tool").unwrap();
    assert!(add.pass);
    assert_eq!(add.target, "replay");
    assert_eq!(add.model.as_deref(), Some("scripted/starter"));
    assert_eq!(add.model_calls, Some(2));
    assert!(add.run_id.is_some() && add.session_id.is_some() && add.tokens.is_some());
    assert_eq!(add.scores["trajectory"], 1.0);
    let safety = rows.iter().filter(|r| r.case_id == "safety_no_secret_read");
    assert_eq!(safety.map(|r| r.sample).collect::<Vec<_>>(), [0, 1]);
}

#[tokio::test]
async fn record_writes_a_cassette_that_replay_then_passes() {
    let data = Dataset::new();
    data.case(add_case(true));
    let model = MockCompletionModel::new(add_turns());
    let (result, printed) = run_with(&["record", "--cases", &data.cases()], |name| {
        assert_eq!(name, "agent/model");
        Ok(model.clone())
    })
    .await;
    result.unwrap();
    assert!(
        printed.starts_with("recorded add: 2 model calls -> "),
        "{printed}"
    );
    assert!(data.file("cassettes/add.json").exists());

    let (result, printed) = offline(&["run", "--target", "replay", "--cases", &data.cases()]).await;
    result.unwrap();
    assert!(printed.contains("1/1 cases"), "{printed}");
}

#[tokio::test]
async fn a_recording_that_fails_its_graders_is_not_written() {
    let data = Dataset::new();
    data.case(add_case(true));
    data.case(json!({"eval_case_id": "other", "kind": "single", "turns": ["x"]}));
    // Answers without calling the tool.
    let model = MockCompletionModel::new([MockTurn::text("42")]);
    let (result, printed) = run_with(
        &[
            "record",
            "--cases",
            &data.cases(),
            "--case",
            "add",
            "--user",
            "u",
        ],
        |_| Ok(model.clone()),
    )
    .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("1 cases failed while recording"), "{err}");
    assert!(
        printed.starts_with("NOT recorded add: trajectory"),
        "{printed}"
    );
    assert!(!data.file("cassettes/add.json").exists());

    let (result, _) = offline(&["record", "--cases", &data.cases(), "--case", "nope"]).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no eval case `nope`")
    );
}

#[tokio::test]
async fn a_stale_cassette_fails_the_gate_as_trajectory_drift() {
    let data = Dataset::new();
    data.case(add_case(false));
    let model = MockCompletionModel::new(add_turns());
    run_with(&["record", "--cases", &data.cases()], |_| Ok(model.clone()))
        .await
        .0
        .unwrap();
    // The case changes after it was recorded. `gate: false` does not help:
    // replay always gates.
    let mut changed = add_case(false);
    changed["turns"] = json!(["add 20 and 22"]);
    data.case(changed);

    let (result, printed) = offline(&["run", "--target", "replay", "--cases", &data.cases()]).await;
    let err = result.unwrap_err().to_string();
    assert_eq!(err, "1 gating eval cases missed their threshold");
    assert!(
        printed.contains("trajectory drift at model call 0"),
        "{printed}"
    );
    assert!(printed.contains("  FAIL\n"), "{printed}");
}

#[tokio::test]
async fn the_judge_is_advisory_and_reported_in_scores() {
    let data = Dataset::new();
    data.case(add_case(true));
    let recorder = MockCompletionModel::new(add_turns());
    run_with(&["record", "--cases", &data.cases()], |_| {
        Ok(recorder.clone())
    })
    .await
    .0
    .unwrap();

    let vote = |pass: bool| {
        MockTurn::text(format!(
            r#"{{"score": 2, "pass": {pass}, "reason": "judged {pass}"}}"#
        ))
    };
    // Sample 0: the judge fails it; sample 1: every vote is unreadable.
    let judge = MockCompletionModel::new([
        vote(false),
        vote(false),
        vote(true),
        MockTurn::error("down"),
        MockTurn::error("down"),
        MockTurn::error("down"),
    ]);
    let out_file = data.file("r.jsonl");
    let out = out_file.display().to_string();
    let (result, _) = run_with(
        &[
            "run",
            "--target",
            "replay",
            "--cases",
            &data.cases(),
            "--k",
            "2",
            "--judge",
            "--out",
            &out,
        ],
        |_| Ok(judge.clone()),
    )
    .await;
    result.unwrap();
    let rows = rows(&out_file);
    assert!(rows[0].pass, "a failing judge does not fail the sample");
    assert_eq!(rows[0].scores["judge"], 2.0);
    assert_eq!(rows[0].scores["judge_pass"], 0.0);
    assert_eq!(rows[0].judge_reason.as_deref(), Some("judged false"));
    assert!(!rows[1].scores.contains_key("judge"));
    assert!(
        rows[1]
            .judge_reason
            .as_deref()
            .unwrap()
            .starts_with("judge failed")
    );
}

/// A real `athena serve` on loopback, in front of a scripted model.
async fn server(turns: Vec<MockTurn>) -> String {
    let service = Arc::new(Service::new(
        Store::open_in_memory().unwrap(),
        "served/model",
        athena::cli::warn,
    ));
    let agent = athena::agent::configure(
        AgentBuilder::new(MockCompletionModel::new(turns)).memory(service.memory()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(athena::http::serve(
        listener,
        service,
        Arc::new(agent),
        std::future::pending(),
    ));
    format!("http://{addr}")
}

#[tokio::test]
async fn a_url_target_gates_only_on_gating_cases() {
    let data = Dataset::new();
    data.case(add_case(true));
    data.case(json!({
        "eval_case_id": "chat", "kind": "single", "turns": ["hi"],
        "expect": {"output": {"contains": ["never said"]}}, "thresholds": {"gate": false}
    }));
    let mut turns = add_turns();
    turns.push(MockTurn::text("hello"));
    let url = server(turns).await;
    let out_file = data.file("live.jsonl");
    let out = out_file.display().to_string();
    let (result, printed) = offline(&[
        "run",
        "--target",
        &url,
        "--cases",
        &data.cases(),
        "--user",
        "tester",
        "--out",
        &out,
    ])
    .await;
    result.unwrap();
    assert!(printed.contains("FAIL (advisory)"), "{printed}");
    let rows = rows(&out_file);
    let add = rows.iter().find(|r| r.case_id == "add").unwrap();
    assert!(add.pass, "{}", add.reason);
    assert_eq!(add.target, url);
    assert_eq!(add.model.as_deref(), Some("served/model"));

    // Now the model is out of turns: the gating case fails.
    let (result, printed) = offline(&["run", "--target", &url, "--cases", &data.cases()]).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("1 gating eval cases"),
        "{printed}"
    );
    assert!(printed.contains("502"), "{printed}");
}

#[tokio::test]
async fn compare_passes_on_equal_results_and_fails_on_a_safety_regression() {
    let data = Dataset::new();
    let base = data.file("base.jsonl");
    let base_s = base.display().to_string();
    offline(&[
        "run", "--target", "replay", "--cases", STARTER, "--out", &base_s,
    ])
    .await
    .0
    .unwrap();
    let (result, printed) = offline(&["compare", &base_s, &base_s]).await;
    result.unwrap();
    assert!(printed.ends_with("no regressions\n"), "{printed}");

    let mut worse = rows(&base);
    for row in worse
        .iter_mut()
        .filter(|r| r.case_id == "safety_no_secret_read")
    {
        row.pass = false;
    }
    let cand = data.file("cand.jsonl");
    report::write_rows(&cand, &worse).unwrap();
    let (result, printed) = offline(&["compare", &base_s, &cand.display().to_string()]).await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("case safety_no_secret_read"), "{err}");
    assert!(err.contains("tag p0"), "{err}");
    assert!(printed.contains("REGRESSION"), "{printed}");
}

#[tokio::test]
async fn malformed_commands_are_refused_with_the_usage() {
    let data = Dataset::new();
    data.case(add_case(true));
    let cases = data.cases();
    for (list, why) in [
        (&[][..], "usage:"),
        (&["nope"][..], "usage:"),
        (&["run", "extra", "--target", "replay"][..], "usage:"),
        (&["run"][..], "--target is required"),
        (
            &["run", "--target", "replay", "--k", "0"][..],
            "--k must be at least 1",
        ),
        (
            &["run", "--target", "replay", "--k", "x"][..],
            "--k got `x`",
        ),
        (
            &[
                "run",
                "--target",
                "replay",
                "--cases",
                "/nonexistent/athena",
            ][..],
            "reading",
        ),
        (
            &["run", "--target", "replay", "--cases", &cases, "--judge"][..],
            "must not build a model",
        ),
        (&["record", "extra"][..], "usage:"),
        (&["record", "--cases", &cases][..], "must not build a model"),
        (&["compare", "one"][..], "usage:"),
        (
            &["compare", "/nonexistent/a", "/nonexistent/b"][..],
            "reading",
        ),
    ] {
        let (result, _) = offline(list).await;
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains(why), "{list:?}: {err}");
    }
}

// ---- the real binary ----

fn athena(list: &[&str]) -> std::process::Output {
    let dir = WorkDir::new();
    Command::new(env!("CARGO_BIN_EXE_athena"))
        .args(list)
        .current_dir(dir.path())
        .env_remove("ATHENA_DB")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("ATHENA_JUDGE_MODEL")
        .output()
        .unwrap()
}

#[test]
fn the_binary_replays_the_starter_dataset_without_a_key_or_a_database() {
    let out = athena(&["eval", "run", "--target", "replay", "--cases", STARTER]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("0 gating failures"), "{stdout}");
}

#[test]
fn the_binary_needs_a_key_only_to_judge() {
    let out = athena(&[
        "eval", "run", "--target", "replay", "--cases", STARTER, "--judge",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("OPENROUTER_API_KEY"), "{stderr}");
}

#[test]
fn the_binary_bench_reports_json_and_fails_when_nothing_answers() {
    let out = athena(&[
        "bench",
        "--url",
        "http://127.0.0.1:1",
        "--duration-secs",
        "1",
        "--concurrency",
        "1",
    ]);
    assert!(!out.status.success());
    let summary: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["turns"], 0);
    assert_eq!(summary["pass"], false);
    assert!(summary["errors"].as_u64().unwrap() >= 1);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("load check failed: no turn succeeded"),
        "{stderr}"
    );
}
