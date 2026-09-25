//! `athena eval`: run eval cases against the agent and grade them.
//!
//! ```text
//! athena eval run --target <replay|URL> [--cases evals/cases] [--k N]
//!                 [--out results.jsonl] [--user eval] [--judge]
//! athena eval record [--cases evals/cases] [--case ID] [--user eval]
//! athena eval compare <base.jsonl> <candidate.jsonl>
//! ```
//!
//! # Targets
//!
//! - `replay` runs each case through the production agent (preamble, tools,
//!   agent loop, SQLite memory) in front of a [`cassette::ReplayModel`] that
//!   serves the case's recorded model responses. No network, no cost, the
//!   same result every time; a cassette that no longer matches the agent
//!   fails the case as `trajectory drift`. Every case gates.
//! - A URL (`http://host:port`) drives a running `athena serve` through its
//!   HTTP API: a new session per sample, one message per turn, then the
//!   session's messages. Cases with `"gate": false` only report.
//!
//! Either way the tool trajectory is rebuilt from the session's stored
//! messages, exactly as production leaves them.
//!
//! `record` runs cases against the real model (`AGENT_MODEL` on OpenRouter,
//! so it needs `OPENROUTER_API_KEY` and costs money) and writes each case's
//! cassette, but only when the recording passes the case's graders.
//!
//! # Case schema (`evals/cases/*.json`)
//!
//! Field names follow Google agents-cli's `EvalCase` where they overlap.
//!
//! ```json
//! {
//!   "eval_case_id": "add_tool",
//!   "kind": "trajectory",
//!   "tags": ["tools", "core"],
//!   "turns": ["Use the add tool to add 21 and 21."],
//!   "expect": {
//!     "trajectory": {
//!       "mode": "ordered",
//!       "tools": ["add", {"tool": "add", "args_match": "21"}],
//!       "forbid": [{"tool": "read_file"}, {"tool": "*", "args_match": "passwd"}]
//!     },
//!     "output": {"contains": ["42"], "not_contains": ["sorry"], "regex": "\\b42\\b"},
//!     "rubric": "States the sum plainly."
//!   },
//!   "thresholds": {"pass_rate": 1.0, "max_model_calls": 4, "max_total_tokens": 5000, "gate": true},
//!   "cassette": "../cassettes/add_tool.json"
//! }
//! ```
//!
//! | Field | Meaning |
//! |---|---|
//! | `eval_case_id` | Unique id; results and cassettes are keyed by it. |
//! | `kind` | `single`, `multi`, `trajectory`, `safety` or `regression`. `safety` is scored pass^k: every sample must pass. The rest are scored by pass rate. |
//! | `tags` | Free-form; `compare` reports pass rates per tag. |
//! | `turns` | User messages, sent in order to one new session. |
//! | `expect.trajectory.mode` | How the tool calls must match `tools`: `ordered` (default: in order, others allowed between), `subset` (any order) or `exact` (these and nothing else). |
//! | `expect.trajectory.tools` | Tool names, or `{"tool", "args_match"}` where `args_match` is a regex searched in the call's arguments as compact JSON. `"*"` matches any tool. |
//! | `expect.trajectory.forbid` | Patterns no call may match. |
//! | `expect.output` | Checks on the **final** reply: `contains` and `not_contains` (case-insensitive substrings), `regex`. |
//! | `expect.rubric` | Read only by `--judge`. Never gates. |
//! | `thresholds.pass_rate` | Share of the `--k` samples that must pass (default 1). |
//! | `thresholds.max_model_calls`, `max_total_tokens` | Budgets per sample, across all turns. |
//! | `thresholds.gate` | `false` makes a URL run report the case without failing. Replay ignores it. |
//! | `cassette` | Relative to the case file. Default `../cassettes/<eval_case_id>.json`. |
//!
//! Every sample must also finish without an error, and where the target
//! reports finish reasons (replay does), no model call may stop at the
//! output-token limit (`finish_reason` `length`).
//!
//! # Results
//!
//! `--out` writes one JSON line per sample ([`report::Row`]): `eval_run_id`,
//! `git_sha` (`ATHENA_VERSION`, else `GITHUB_SHA`, else `dev`), `env`
//! (`ATHENA_ENV`, else `dev`), `target`, `case_id`, `kind`, `tags`, `sample`,
//! `pass`, `scores` (0/1 per check, plus `judge` and `judge_pass` with
//! `--judge`), `reason`, `judge_reason`, `run_id`, `session_id`, `trace_id`
//! (from a `traceparent` or `x-trace-id` response header), `tokens`,
//! `model_calls`, `latency_ms`, `model`.
//!
//! The command prints a summary and exits non-zero if a gating case missed
//! its threshold. `compare` exits non-zero on any drop in a safety case, or a
//! drop of more than 5 points in any other case or tag.

pub mod case;
pub mod cassette;
pub mod grade;
pub mod judge;
pub mod report;
pub mod target;

use crate::flags::Flags;
use anyhow::{Context, Result, bail};
use judge::Judge;
use report::Row;
use rig_core::completion::CompletionModel;
use std::io::Write;
use std::path::Path;

pub const USAGE: &str = "usage:
  athena eval run --target <replay|URL> [--cases evals/cases] [--k N] [--out FILE] [--user eval] [--judge]
  athena eval record [--cases evals/cases] [--case ID] [--user eval]
  athena eval compare BASE.jsonl CANDIDATE.jsonl";

const DEFAULT_CASES: &str = "evals/cases";
const DEFAULT_USER: &str = "eval";

/// `athena eval ...`. `make_model` builds a real model by name, for `record`
/// and `--judge`; nothing else calls it.
pub async fn main<M, F>(
    args: &[String],
    agent_model: &str,
    make_model: F,
    out: &mut impl Write,
) -> Result<()>
where
    M: CompletionModel + Clone + 'static,
    F: Fn(&str) -> Result<M>,
{
    match args.split_first() {
        Some((cmd, rest)) if cmd == "run" => run(rest, agent_model, make_model, out).await,
        Some((cmd, rest)) if cmd == "record" => record(rest, agent_model, make_model, out).await,
        Some((cmd, rest)) if cmd == "compare" => compare(rest, out),
        _ => bail!("{USAGE}"),
    }
}

/// The judge's model: `ATHENA_JUDGE_MODEL`, else the agent's own. A judge
/// from another model family is less likely to share the agent's blind spots.
fn judge_model(configured: Option<String>, agent_model: &str) -> String {
    configured.unwrap_or_else(|| agent_model.to_string())
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

async fn run<M, F>(
    args: &[String],
    agent_model: &str,
    make_model: F,
    out: &mut impl Write,
) -> Result<()>
where
    M: CompletionModel + Clone + 'static,
    F: Fn(&str) -> Result<M>,
{
    let flags = Flags::parse(args, &["target", "cases", "k", "out", "user"], &["judge"])?;
    if !flags.positional.is_empty() {
        bail!("{USAGE}");
    }
    let target = flags
        .get("target")
        .context("--target is required: `replay`, or the URL of a running athena serve")?;
    let k: usize = flags.parsed("k", 1)?;
    if k == 0 {
        bail!("--k must be at least 1");
    }
    let user = flags.get("user").unwrap_or(DEFAULT_USER);
    let cases = case::load(Path::new(flags.get("cases").unwrap_or(DEFAULT_CASES)))?;
    let judge = match flags.switch("judge") {
        true => Some(Judge::new(make_model(&judge_model(
            std::env::var("ATHENA_JUDGE_MODEL").ok(),
            agent_model,
        ))?)),
        false => None,
    };
    let http = match target {
        "replay" => None,
        url => Some(target::Http::new(url, user)?),
    };

    let eval_run_id = uuid::Uuid::new_v4().to_string();
    let git_sha = report::git_sha(
        std::env::var("ATHENA_VERSION").ok(),
        std::env::var("GITHUB_SHA").ok(),
    );
    let env = env_or("ATHENA_ENV", "dev");
    let mut rows: Vec<Row> = Vec::new();
    let mut summaries = Vec::new();
    for case in &cases {
        let mut case_rows = Vec::new();
        for sample in 0..k {
            let obs = match &http {
                None => target::replay(case, user).await?,
                Some(api) => api.run(case).await,
            };
            let grade = grade::grade(case, &obs);
            let mut scores = grade.scores;
            let mut judge_reason = None;
            if let (Some(judge), Some(rubric)) = (&judge, &case.expect.rubric) {
                match judge.judge(rubric, case, &obs).await {
                    Ok(v) => {
                        scores.insert("judge".into(), v.score);
                        scores.insert("judge_pass".into(), f64::from(u8::from(v.pass)));
                        judge_reason = Some(v.reason);
                    }
                    Err(e) => judge_reason = Some(e),
                }
            }
            case_rows.push(Row {
                eval_run_id: eval_run_id.clone(),
                git_sha: git_sha.clone(),
                env: env.clone(),
                target: target.to_string(),
                case_id: case.eval_case_id.clone(),
                kind: case.kind,
                tags: case.tags.clone(),
                sample,
                pass: grade.pass,
                scores,
                reason: match grade.failures.is_empty() {
                    true => "ok".into(),
                    false => grade.failures.join("; "),
                },
                judge_reason,
                run_id: obs.run_id,
                session_id: obs.session_id,
                trace_id: obs.trace_id,
                tokens: obs.total_tokens,
                model_calls: obs.model_calls,
                latency_ms: obs.latency_ms,
                model: obs.model,
            });
        }
        summaries.push(report::summarize(case, &case_rows, http.is_none()));
        rows.extend(case_rows);
    }
    if let Some(path) = flags.get("out") {
        report::write_rows(Path::new(path), &rows)?;
    }
    let failed = report::print_summary(&summaries, out)?;
    if failed > 0 {
        bail!("{failed} gating eval cases missed their threshold");
    }
    Ok(())
}

async fn record<M, F>(
    args: &[String],
    agent_model: &str,
    make_model: F,
    out: &mut impl Write,
) -> Result<()>
where
    M: CompletionModel + Clone + 'static,
    F: Fn(&str) -> Result<M>,
{
    let flags = Flags::parse(args, &["cases", "case", "user"], &[])?;
    if !flags.positional.is_empty() {
        bail!("{USAGE}");
    }
    let user = flags.get("user").unwrap_or(DEFAULT_USER);
    let mut cases = case::load(Path::new(flags.get("cases").unwrap_or(DEFAULT_CASES)))?;
    if let Some(id) = flags.get("case") {
        cases.retain(|c| c.eval_case_id == id);
        if cases.is_empty() {
            bail!("no eval case `{id}`");
        }
    }
    let model = make_model(agent_model)?;
    let mut failures = 0;
    for case in &cases {
        let (cassette, obs) = target::record(case, model.clone(), agent_model, user).await?;
        let grade = grade::grade(case, &obs);
        if grade.pass {
            let path = case.cassette_path();
            cassette.write(&path)?;
            let calls = cassette.interactions.len();
            let (id, path) = (&case.eval_case_id, path.display());
            writeln!(out, "recorded {id}: {calls} model calls -> {path}")?;
        } else {
            failures += 1;
            let (id, why) = (&case.eval_case_id, grade.failures.join("; "));
            writeln!(out, "NOT recorded {id}: {why}")?;
        }
    }
    if failures > 0 {
        bail!("{failures} cases failed while recording; their cassettes were left unchanged");
    }
    Ok(())
}

fn compare(args: &[String], out: &mut impl Write) -> Result<()> {
    let [base, candidate] = args else {
        bail!("{USAGE}");
    };
    let base = report::read_rows(Path::new(base))?;
    let candidate = report::read_rows(Path::new(candidate))?;
    let regressions = report::compare(&base, &candidate, out)?;
    if !regressions.is_empty() {
        bail!("regressions: {}", regressions.join(", "));
    }
    writeln!(out, "no regressions")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_judge_uses_its_own_model_when_configured() {
        assert_eq!(judge_model(Some("j/x".into()), "a/y"), "j/x");
        assert_eq!(judge_model(None, "a/y"), "a/y");
    }

    #[test]
    fn env_or_falls_back_when_unset() {
        assert_eq!(env_or("ATHENA_TEST_SURELY_UNSET_VAR", "d"), "d");
        assert!(!env_or("PATH", "d").is_empty());
    }
}
