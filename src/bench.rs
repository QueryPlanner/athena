//! `athena bench`: a load check against a running `athena serve`.
//!
//! ```text
//! athena bench --url <base> [--concurrency 3] [--duration-secs 120] [--user bench]
//!              [--max-p95-ms 30000] [--max-error-rate 0.05]
//! ```
//!
//! Each of `concurrency` workers creates a session and sends short prompts,
//! one turn at a time, until the duration is up, starting a new session
//! every few turns so the history stays short. These are real turns: they
//! call the deployed model and cost money. Prints a JSON summary on stdout
//! and exits non-zero if the p95 turn latency or the error rate is over its
//! limit, or if no turn succeeded.

use crate::eval::target::{client, send};
use crate::flags::Flags;
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::io::Write;
use std::time::{Duration, Instant};

const PROMPTS: [&str; 3] = [
    "Reply with one word: ready.",
    "What is 2 + 3? Use the add tool.",
    "Name one primary colour, in one word.",
];

/// Turns per session before a worker starts a new one.
const TURNS_PER_SESSION: usize = 5;

/// How long a worker waits after a failed request, so a server that is down
/// is not hammered.
const BACKOFF: Duration = Duration::from_millis(200);

const FLAGS: [&str; 6] = [
    "url",
    "concurrency",
    "duration-secs",
    "user",
    "max-p95-ms",
    "max-error-rate",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub url: String,
    pub concurrency: usize,
    pub duration: Duration,
    pub user: String,
    pub max_p95_ms: u64,
    pub max_error_rate: f64,
}

impl Config {
    pub fn parse(args: &[String]) -> Result<Self> {
        let flags = Flags::parse(args, &FLAGS, &[])?;
        let Some(url) = flags.get("url").filter(|_| flags.positional.is_empty()) else {
            bail!(
                "usage: athena bench --url <base> [--concurrency 3] [--duration-secs 120] \
                 [--user bench] [--max-p95-ms 30000] [--max-error-rate 0.05]"
            );
        };
        let config = Self {
            url: url.trim_end_matches('/').to_string(),
            concurrency: flags.parsed("concurrency", 3)?,
            duration: Duration::from_secs(flags.parsed("duration-secs", 120)?),
            user: flags.get("user").unwrap_or("bench").to_string(),
            max_p95_ms: flags.parsed("max-p95-ms", 30_000)?,
            max_error_rate: flags.parsed("max-error-rate", 0.05)?,
        };
        if config.concurrency == 0 {
            bail!("--concurrency must be at least 1");
        }
        Ok(config)
    }
}

/// What one worker, or all of them, saw.
#[derive(Debug, Default)]
struct Stats {
    requests: u64,
    errors: u64,
    /// Latency of each successful turn.
    latencies_ms: Vec<u64>,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    first_error: Option<String>,
}

impl Stats {
    fn failed(&mut self, error: anyhow::Error) {
        self.errors += 1;
        self.first_error.get_or_insert_with(|| format!("{error:#}"));
    }

    fn merge(&mut self, other: Stats) {
        self.requests += other.requests;
        self.errors += other.errors;
        self.latencies_ms.extend(other.latencies_ms);
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.first_error = self.first_error.take().or(other.first_error);
    }
}

#[derive(Debug, Serialize)]
pub struct Latency {
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
}

#[derive(Debug, Serialize)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub total: i64,
}

/// The JSON printed on stdout.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub url: String,
    pub concurrency: usize,
    pub duration_secs: u64,
    /// Every HTTP request made, session creations included.
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    /// Successful turns only.
    pub turns: usize,
    pub latency_ms: Latency,
    pub tokens: Tokens,
    pub max_p95_ms: u64,
    pub max_error_rate: f64,
    pub pass: bool,
    pub failures: Vec<String>,
    pub first_error: Option<String>,
}

/// `athena bench ...`: run, print the summary, fail if a threshold failed.
pub async fn main(args: &[String], out: &mut impl Write) -> Result<()> {
    let config = Config::parse(args)?;
    let summary = run(&config).await?;
    // Cannot fail: plain numbers and strings.
    let json = serde_json::to_string_pretty(&summary).expect("the summary serialises");
    writeln!(out, "{json}")?;
    if !summary.pass {
        bail!("load check failed: {}", summary.failures.join("; "));
    }
    Ok(())
}

pub async fn run(config: &Config) -> Result<Summary> {
    let client = client(Duration::from_secs(300))?;
    let deadline = Instant::now() + config.duration;
    let workers: Vec<_> = (0..config.concurrency)
        .map(|w| tokio::spawn(worker(client.clone(), config.clone(), deadline, w)))
        .collect();
    let mut stats = Stats::default();
    for worker in workers {
        stats.merge(worker.await?);
    }
    Ok(summarize(config, stats))
}

async fn worker(
    client: reqwest::Client,
    config: Config,
    deadline: Instant,
    offset: usize,
) -> Stats {
    let mut stats = Stats::default();
    let mut session: Option<(String, usize)> = None;
    let mut n = offset;
    while Instant::now() < deadline {
        let (id, turns) = match session.take() {
            Some((id, turns)) if turns < TURNS_PER_SESSION => (id, turns),
            _ => {
                stats.requests += 1;
                match create_session(&client, &config).await {
                    Ok(id) => (id, 0),
                    Err(e) => {
                        stats.failed(e);
                        tokio::time::sleep(BACKOFF).await;
                        continue;
                    }
                }
            }
        };
        stats.requests += 1;
        let started = Instant::now();
        let url = format!("{}/sessions/{id}/messages", config.url);
        let prompt = PROMPTS[n % PROMPTS.len()];
        n += 1;
        match send(&client, &url, &config.user, Some(json!({"text": prompt}))).await {
            Ok(reply) => {
                stats
                    .latencies_ms
                    .push(started.elapsed().as_millis() as u64);
                let run = &reply.body["run"];
                let count = |field: &str| run[field].as_i64().unwrap_or_default();
                stats.input_tokens += count("input_tokens");
                stats.output_tokens += count("output_tokens");
                stats.total_tokens += count("total_tokens");
                session = Some((id, turns + 1));
            }
            Err(e) => {
                stats.failed(e);
                tokio::time::sleep(BACKOFF).await;
            }
        }
    }
    stats
}

async fn create_session(client: &reqwest::Client, config: &Config) -> Result<String> {
    let url = format!("{}/sessions", config.url);
    let name = format!("bench-{}", uuid::Uuid::new_v4());
    let reply = send(client, &url, &config.user, Some(json!({"name": name}))).await?;
    match reply.body.get("id").and_then(Value::as_str) {
        Some(id) => Ok(id.to_string()),
        None => bail!("{url} returned no session id"),
    }
}

/// The nearest-rank percentile `p` of sorted `values`, 0 when empty.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn summarize(config: &Config, mut stats: Stats) -> Summary {
    stats.latencies_ms.sort_unstable();
    let error_rate = match stats.requests {
        0 => 0.0,
        n => stats.errors as f64 / n as f64,
    };
    let latency = Latency {
        p50: percentile(&stats.latencies_ms, 50.0),
        p95: percentile(&stats.latencies_ms, 95.0),
        max: stats.latencies_ms.last().copied().unwrap_or_default(),
    };
    let mut failures = Vec::new();
    if stats.latencies_ms.is_empty() {
        failures.push("no turn succeeded".to_string());
    }
    if latency.p95 > config.max_p95_ms {
        failures.push(format!(
            "p95 {} ms is over {} ms",
            latency.p95, config.max_p95_ms
        ));
    }
    if error_rate > config.max_error_rate {
        failures.push(format!(
            "error rate {error_rate:.3} is over {}",
            config.max_error_rate
        ));
    }
    Summary {
        url: config.url.clone(),
        concurrency: config.concurrency,
        duration_secs: config.duration.as_secs(),
        requests: stats.requests,
        errors: stats.errors,
        error_rate,
        turns: stats.latencies_ms.len(),
        latency_ms: latency,
        tokens: Tokens {
            input: stats.input_tokens,
            output: stats.output_tokens,
            total: stats.total_tokens,
        },
        max_p95_ms: config.max_p95_ms,
        max_error_rate: config.max_error_rate,
        pass: failures.is_empty(),
        failures,
        first_error: stats.first_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn config() -> Config {
        Config::parse(&args(&["--url", "http://h:1/"])).unwrap()
    }

    #[test]
    fn flags_default_to_the_contract_values() {
        assert_eq!(
            config(),
            Config {
                url: "http://h:1".into(),
                concurrency: 3,
                duration: Duration::from_secs(120),
                user: "bench".into(),
                max_p95_ms: 30_000,
                max_error_rate: 0.05,
            }
        );
        let c = Config::parse(&args(&[
            "--url",
            "u",
            "--concurrency",
            "1",
            "--duration-secs",
            "2",
            "--user",
            "x",
            "--max-p95-ms",
            "9",
            "--max-error-rate",
            "0.5",
        ]))
        .unwrap();
        assert_eq!(
            (c.concurrency, c.duration.as_secs(), c.user.as_str()),
            (1, 2, "x")
        );
        assert_eq!((c.max_p95_ms, c.max_error_rate), (9, 0.5));
        for bad in [
            &[][..],
            &["--url", "u", "extra"][..],
            &["--url", "u", "--concurrency", "0"][..],
        ] {
            assert!(Config::parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50.0), 50);
        assert_eq!(percentile(&values, 95.0), 95);
        assert_eq!(percentile(&[7], 95.0), 7);
        assert_eq!(percentile(&[1, 2], 0.0), 1);
        assert_eq!(percentile(&[], 95.0), 0);
    }

    #[test]
    fn thresholds_fail_on_slow_turns_errors_or_nothing_done() {
        let stats = |latencies: Vec<u64>, requests, errors| Stats {
            requests,
            errors,
            latencies_ms: latencies,
            ..Stats::default()
        };
        let ok = summarize(&config(), stats(vec![30, 10, 20], 4, 0));
        assert!(ok.pass, "{:?}", ok.failures);
        assert_eq!(
            (ok.latency_ms.p50, ok.latency_ms.max, ok.turns),
            (20, 30, 3)
        );

        let slow = summarize(&config(), stats(vec![40_000], 1, 0));
        assert_eq!(slow.failures, ["p95 40000 ms is over 30000 ms"]);
        let flaky = summarize(&config(), stats(vec![1], 10, 1));
        assert_eq!(flaky.failures, ["error rate 0.100 is over 0.05"]);
        let idle = summarize(&config(), stats(vec![], 0, 0));
        assert_eq!(
            (idle.error_rate, idle.failures.clone()),
            (0.0, vec!["no turn succeeded".to_string()])
        );
    }

    #[test]
    fn merging_keeps_the_first_error_seen() {
        let mut a = Stats::default();
        let mut b = Stats::default();
        b.failed(anyhow::anyhow!("second"));
        b.failed(anyhow::anyhow!("third"));
        a.merge(b);
        assert_eq!((a.errors, a.first_error.as_deref()), (2, Some("second")));
    }
}
