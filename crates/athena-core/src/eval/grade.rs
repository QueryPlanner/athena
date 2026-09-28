//! Deterministic graders: the same observation always gets the same grade.

use super::case::{EvalCase, Mode, ToolPattern};
use super::target::{Observation, ToolCall};
use regex::Regex;
use std::collections::BTreeMap;

/// A sample's grade: whether it passed, a 0/1 score per check that applied,
/// and why each failed check failed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Grade {
    pub pass: bool,
    pub scores: BTreeMap<String, f64>,
    pub failures: Vec<String>,
}

impl Grade {
    fn check(&mut self, name: &str, failure: Option<String>) {
        self.scores
            .insert(name.to_string(), if failure.is_none() { 1.0 } else { 0.0 });
        self.failures.extend(failure);
    }
}

/// Grade one sample of `case`.
pub fn grade(case: &EvalCase, obs: &Observation) -> Grade {
    let mut grade = Grade::default();
    let stopped = obs.drift.clone().or_else(|| obs.error.clone());
    grade.check("completed", stopped);

    if let Some(t) = &case.expect.trajectory {
        let failure = (!trajectory_matches(t.mode, &t.tools, &obs.tool_calls)).then(|| {
            format!(
                "trajectory: expected {:?} ({}), got {}",
                t.tools.iter().map(describe).collect::<Vec<_>>(),
                format!("{:?}", t.mode).to_lowercase(),
                calls(&obs.tool_calls)
            )
        });
        grade.check("trajectory", failure);
        let forbidden: Vec<String> = obs
            .tool_calls
            .iter()
            .filter(|c| t.forbid.iter().any(|p| matches(p, c)))
            .map(|c| format!("{}({})", c.name, c.arguments))
            .collect();
        let failure = (!forbidden.is_empty())
            .then(|| format!("forbidden tool calls: {}", forbidden.join(", ")));
        grade.check("forbid", failure);
    }

    if let Some(o) = &case.expect.output {
        let reply = obs.replies.last().map(String::as_str).unwrap_or_default();
        let lower = reply.to_lowercase();
        let mut problems: Vec<String> = Vec::new();
        for s in &o.contains {
            if !lower.contains(&s.to_lowercase()) {
                problems.push(format!("missing {s:?}"));
            }
        }
        for s in &o.not_contains {
            if lower.contains(&s.to_lowercase()) {
                problems.push(format!("contains {s:?}"));
            }
        }
        if let Some(re) = &o.regex
            && !regex(re).is_match(reply)
        {
            problems.push(format!("does not match /{re}/"));
        }
        let failure = (!problems.is_empty())
            .then(|| format!("output: {} in {:?}", problems.join(", "), clip(reply)));
        grade.check("output", failure);
    }

    let limits = [
        (
            "max_model_calls",
            case.thresholds.max_model_calls,
            obs.model_calls,
        ),
        (
            "max_total_tokens",
            case.thresholds.max_total_tokens,
            obs.total_tokens,
        ),
    ];
    for (name, limit, used) in limits {
        if let (Some(limit), Some(used)) = (limit, used) {
            let failure = (used > limit).then(|| format!("{name}: used {used}, limit {limit}"));
            grade.check(name, failure);
        }
    }

    if !obs.finish_reasons.is_empty() {
        let truncated = obs.finish_reasons.iter().any(|r| r == "length");
        let failure =
            truncated.then(|| "finish_reason: a model call hit the output-token limit".to_string());
        grade.check("finish_reason", failure);
    }

    grade.pass = grade.failures.is_empty();
    grade
}

/// Whether `actual` satisfies `expected` under `mode`; see [`Mode`].
fn trajectory_matches(mode: Mode, expected: &[ToolPattern], actual: &[ToolCall]) -> bool {
    match mode {
        Mode::Exact => {
            expected.len() == actual.len()
                && expected.iter().zip(actual).all(|(p, c)| matches(p, c))
        }
        Mode::Ordered => {
            let mut rest = actual.iter();
            expected.iter().all(|p| rest.any(|c| matches(p, c)))
        }
        Mode::Subset => {
            let mut used = vec![false; actual.len()];
            expected.iter().all(|p| {
                let found = (0..actual.len()).find(|&i| !used[i] && matches(p, &actual[i]));
                found.map(|i| used[i] = true).is_some()
            })
        }
    }
}

fn matches(pattern: &ToolPattern, call: &ToolCall) -> bool {
    let tool = pattern.tool();
    (tool == "*" || tool == call.name)
        && pattern
            .args_match()
            .is_none_or(|re| regex(re).is_match(&call.arguments.to_string()))
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("case regexes are checked when the case is loaded")
}

fn describe(p: &ToolPattern) -> String {
    match p.args_match() {
        Some(re) => format!("{} /{re}/", p.tool()),
        None => p.tool().to_string(),
    }
}

fn calls(calls: &[ToolCall]) -> String {
    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
    format!("{names:?}")
}

/// At most 200 characters of `text`, for a failure message.
fn clip(text: &str) -> String {
    match text.char_indices().nth(200) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn case(extra: serde_json::Value) -> EvalCase {
        let mut value = json!({"eval_case_id": "c", "kind": "single", "turns": ["hi"]});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            name: name.into(),
            arguments: args,
        }
    }

    fn obs(replies: &[&str], tool_calls: Vec<ToolCall>) -> Observation {
        Observation {
            replies: replies.iter().map(|r| r.to_string()).collect(),
            tool_calls,
            ..Observation::default()
        }
    }

    fn trajectory(mode: &str, tools: serde_json::Value) -> EvalCase {
        case(json!({"expect": {"trajectory": {"mode": mode, "tools": tools}}}))
    }

    #[test]
    fn a_completed_sample_with_no_expectations_passes() {
        let g = grade(&case(json!({})), &obs(&["x"], vec![]));
        assert!(g.pass);
        assert_eq!(g.scores, BTreeMap::from([("completed".to_string(), 1.0)]));
    }

    #[test]
    fn errors_and_drift_fail_the_sample_with_drift_reported_first() {
        let mut o = obs(&[], vec![]);
        o.error = Some("model failed".into());
        let g = grade(&case(json!({})), &o);
        assert_eq!(
            (g.pass, g.failures.clone()),
            (false, vec!["model failed".into()])
        );
        o.drift = Some("trajectory drift: x".into());
        assert_eq!(
            grade(&case(json!({})), &o).failures,
            ["trajectory drift: x"]
        );
    }

    #[test]
    fn trajectory_modes() {
        let a = || call("add", json!({"a": 21, "b": 21}));
        let r = || call("read", json!({"path": "/x"}));
        let passes = |mode: &str, tools: serde_json::Value, actual: Vec<ToolCall>| {
            grade(&trajectory(mode, tools), &obs(&["x"], actual)).pass
        };
        // exact: same calls, same order, nothing else.
        assert!(passes("exact", json!(["add", "read"]), vec![a(), r()]));
        assert!(!passes("exact", json!(["add"]), vec![a(), r()]));
        assert!(!passes("exact", json!(["read", "add"]), vec![a(), r()]));
        assert!(passes("exact", json!([]), vec![]));
        // ordered: in order, others allowed between.
        assert!(passes(
            "ordered",
            json!(["add", "read"]),
            vec![a(), a(), r()]
        ));
        assert!(!passes("ordered", json!(["read", "add"]), vec![a(), r()]));
        // subset: any order, each actual call used once.
        assert!(passes("subset", json!(["read", "add"]), vec![a(), r()]));
        assert!(!passes("subset", json!(["add", "add"]), vec![a(), r()]));
        // Arguments are matched by regex on compact JSON.
        assert!(passes(
            "exact",
            json!([{"tool": "add", "args_match": "\"a\":21"}]),
            vec![a()]
        ));
        assert!(!passes(
            "exact",
            json!([{"tool": "add", "args_match": "99"}]),
            vec![a()]
        ));
        assert!(passes("exact", json!([{"tool": "*"}]), vec![r()]));

        let g = grade(
            &trajectory("exact", json!([{"tool": "add", "args_match": "9"}])),
            &obs(&["x"], vec![r()]),
        );
        assert_eq!(
            g.failures,
            [r#"trajectory: expected ["add /9/"] (exact), got ["read"]"#]
        );
    }

    #[test]
    fn forbidden_calls_fail_even_when_the_trajectory_matches() {
        let c = case(json!({"expect": {"trajectory": {
            "mode": "subset", "tools": ["add"],
            "forbid": [{"tool": "read_file"}, {"tool": "*", "args_match": "passwd"}]
        }}}));
        assert!(grade(&c, &obs(&["x"], vec![call("add", json!({}))])).pass);
        let g = grade(
            &c,
            &obs(
                &["x"],
                vec![
                    call("add", json!({})),
                    call("shell", json!({"cmd": "cat /etc/passwd"})),
                ],
            ),
        );
        assert!(!g.pass);
        assert_eq!(g.scores["trajectory"], 1.0);
        assert_eq!(g.scores["forbid"], 0.0);
        let why = &g.failures[0];
        assert!(why.starts_with("forbidden tool calls: shell("), "{why}");
    }

    #[test]
    fn output_checks_the_final_reply_case_insensitively() {
        let c = case(json!({"expect": {"output": {
            "contains": ["Forty"], "not_contains": ["sorry"], "regex": "\\d+$"
        }}}));
        assert!(grade(&c, &obs(&["no", "forty-two is 42"], vec![])).pass);
        let g = grade(&c, &obs(&["Sorry, no FORTY"], vec![]));
        assert_eq!(
            g.failures,
            [r#"output: contains "sorry", does not match /\d+$/ in "Sorry, no FORTY""#]
        );
        let g = grade(&c, &obs(&[], vec![]));
        let why = &g.failures[0];
        assert!(why.contains("missing \"Forty\""), "{why}");
    }

    #[test]
    fn budgets_apply_only_when_the_target_reports_usage() {
        let c = case(json!({"thresholds": {"max_model_calls": 2, "max_total_tokens": 100}}));
        let mut o = obs(&["x"], vec![]);
        assert!(!grade(&c, &o).scores.contains_key("max_model_calls"));
        o.model_calls = Some(3);
        o.total_tokens = Some(100);
        let g = grade(&c, &o);
        assert_eq!(g.failures, ["max_model_calls: used 3, limit 2"]);
        assert_eq!(g.scores["max_total_tokens"], 1.0);
    }

    #[test]
    fn a_truncated_model_call_fails_when_finish_reasons_are_known() {
        let mut o = obs(&["x"], vec![]);
        o.finish_reasons = vec!["tool_calls".into(), "stop".into()];
        assert_eq!(grade(&case(json!({})), &o).scores["finish_reason"], 1.0);
        o.finish_reasons.push("length".into());
        assert!(!grade(&case(json!({})), &o).pass);
    }

    #[test]
    fn long_replies_are_clipped_in_failure_messages() {
        let long = "é".repeat(300);
        assert_eq!(clip(&long).chars().count(), 201);
        assert_eq!(clip("short"), "short");
    }
}
