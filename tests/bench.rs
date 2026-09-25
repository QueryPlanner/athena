//! `athena bench` against a real `athena serve` router on loopback, in front
//! of a scripted model. No network beyond loopback.

use athena::bench;
use athena::service::Service;
use athena::store::Store;
use rig_agent::agent::AgentBuilder;
use rig_core::completion::Usage;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use std::sync::Arc;
use std::time::Duration;

async fn server(turns: impl IntoIterator<Item = MockTurn>) -> String {
    let service = Arc::new(Service::new(
        Store::open_in_memory().unwrap(),
        "m",
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

/// More replies than a one-second run can use.
fn plenty() -> impl Iterator<Item = MockTurn> {
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        total_tokens: 12,
        ..Usage::new()
    };
    (0..100_000).map(move |_| MockTurn::text("ok").with_usage(usage))
}

fn config(url: &str) -> bench::Config {
    bench::Config {
        url: url.to_string(),
        concurrency: 2,
        duration: Duration::from_secs(1),
        user: "bench".into(),
        max_p95_ms: 30_000,
        max_error_rate: 0.05,
    }
}

#[tokio::test]
async fn a_healthy_server_passes_with_tokens_counted_and_sessions_rotated() {
    let url = server(plenty()).await;
    let summary = bench::run(&config(&url)).await.unwrap();
    assert!(
        summary.pass,
        "{:?} {:?}",
        summary.failures, summary.first_error
    );
    assert_eq!(summary.errors, 0);
    assert!(summary.turns > 5, "{}", summary.turns);
    // Every request is a turn or a session; sessions rotate every 5 turns.
    let sessions = summary.requests as usize - summary.turns;
    assert!(
        sessions >= summary.turns / 5,
        "{sessions} sessions for {} turns",
        summary.turns
    );
    assert_eq!(summary.tokens.total, 12 * summary.turns as i64);
    assert_eq!(summary.tokens.input, 10 * summary.turns as i64);
    assert_eq!(summary.tokens.output, 2 * summary.turns as i64);
    assert!(summary.latency_ms.p50 <= summary.latency_ms.p95);
    assert!(summary.latency_ms.p95 <= summary.latency_ms.max);
}

#[tokio::test]
async fn main_prints_the_summary_as_json() {
    let url = server(plenty()).await;
    let mut out = Vec::new();
    let args: Vec<String> = ["--url", &url, "--duration-secs", "1", "--concurrency", "1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    bench::main(&args, &mut out).await.unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(summary["pass"], true);
    assert_eq!(summary["url"], url);
    assert_eq!(summary["max_p95_ms"], 30_000);
}

#[tokio::test]
async fn a_failing_model_fails_the_error_rate() {
    // No scripted replies: every turn is a 502.
    let url = server([]).await;
    let summary = bench::run(&config(&url)).await.unwrap();
    assert!(!summary.pass);
    assert_eq!(summary.turns, 0);
    assert!(summary.error_rate > 0.05);
    assert!(summary.failures.iter().any(|f| f.starts_with("error rate")));
    assert!(summary.first_error.unwrap().contains("502"));
}

#[tokio::test]
async fn a_server_that_returns_no_session_id_is_an_error() {
    let router = axum::Router::new().fallback(|| async { axum::Json(serde_json::json!({})) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    let summary = bench::run(&config(&format!("http://{addr}")))
        .await
        .unwrap();
    assert!(!summary.pass);
    assert!(
        summary
            .first_error
            .unwrap()
            .contains("returned no session id")
    );
}
