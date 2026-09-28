//! The optional LLM judge (`--judge`): grades a sample against its case's
//! `rubric`. Advisory only: its verdict is reported in `scores.judge` and
//! `scores.judge_pass` but never changes `pass`. It gates nothing until it
//! has been calibrated against hand labels (plan section 9).
//!
//! Each sample is judged `votes` times (3 by default). The verdict passes if
//! most valid votes pass, and its score is their median.

use super::case::EvalCase;
use super::target::Observation;
use rig_core::completion::{AssistantContent, CompletionModel};
use serde::Deserialize;

const INSTRUCTIONS: &str = "You grade an AI assistant's conversation against a rubric. \
Reply with only a JSON object: {\"score\": <integer 1-5>, \"pass\": <true|false>, \
\"reason\": \"<one sentence>\"}. 5 means the rubric is fully met, 1 not at all.";

/// A judge's decision on one sample.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub score: f64,
    pub pass: bool,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
struct Vote {
    score: u8,
    pass: bool,
    reason: String,
}

pub struct Judge<M> {
    model: M,
    votes: usize,
}

impl<M: CompletionModel + Clone> Judge<M> {
    pub fn new(model: M) -> Self {
        Self { model, votes: 3 }
    }

    /// Judge `obs` against `rubric`. Fails only if no vote could be read.
    pub async fn judge(
        &self,
        rubric: &str,
        case: &EvalCase,
        obs: &Observation,
    ) -> Result<Verdict, String> {
        let prompt = prompt(rubric, case, obs);
        let mut votes: Vec<Vote> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        for _ in 0..self.votes {
            match self.vote(&prompt).await {
                Ok(vote) => votes.push(vote),
                Err(e) => problems.push(e),
            }
        }
        if votes.is_empty() {
            return Err(format!("judge failed: {}", problems.join("; ")));
        }
        let pass = votes.iter().filter(|v| v.pass).count() * 2 > votes.len();
        let mut scores: Vec<u8> = votes.iter().map(|v| v.score).collect();
        scores.sort_unstable();
        let reason = votes
            .iter()
            .find(|v| v.pass == pass)
            .map(|v| v.reason.clone())
            .unwrap_or_default();
        Ok(Verdict {
            score: f64::from(scores[scores.len() / 2]),
            pass,
            reason,
        })
    }

    async fn vote(&self, prompt: &str) -> Result<Vote, String> {
        let response = self
            .model
            .completion_request(prompt)
            .preamble(INSTRUCTIONS.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let text: String = response
            .choice
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        parse_vote(&text)
    }
}

/// The JSON object in a judge's reply, tolerating prose or fences around it.
fn parse_vote(text: &str) -> Result<Vote, String> {
    let unreadable = || format!("unreadable vote: {text:?}");
    let (start, end) = text.find('{').zip(text.rfind('}')).ok_or_else(unreadable)?;
    let vote: Vote = serde_json::from_str(text.get(start..=end).ok_or_else(unreadable)?)
        .map_err(|_| unreadable())?;
    if !(1..=5).contains(&vote.score) {
        return Err(format!("vote score {} is not 1-5", vote.score));
    }
    Ok(vote)
}

fn prompt(rubric: &str, case: &EvalCase, obs: &Observation) -> String {
    let mut text = format!("Rubric:\n{rubric}\n\nConversation:\n");
    for (i, turn) in case.turns.iter().enumerate() {
        text += &format!("User: {turn}\n");
        let reply = obs.replies.get(i).map_or("(no reply)", String::as_str);
        text += &format!("Assistant: {reply}\n");
    }
    let tools: Vec<String> = obs
        .tool_calls
        .iter()
        .map(|c| format!("{}({})", c.name, c.arguments))
        .collect();
    text += &format!("\nTool calls, in order: {}\n", tools.join(", "));
    if let Some(e) = &obs.error {
        text += &format!("The conversation ended with an error: {e}\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::target::ToolCall;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn case() -> EvalCase {
        serde_json::from_value(serde_json::json!({
            "eval_case_id": "c", "kind": "single", "turns": ["add 2 and 2", "thanks"]
        }))
        .unwrap()
    }

    fn obs() -> Observation {
        Observation {
            replies: vec!["4".into()],
            tool_calls: vec![ToolCall {
                name: "add".into(),
                arguments: serde_json::json!({"a": 2}),
            }],
            error: Some("boom".into()),
            ..Observation::default()
        }
    }

    /// A vote in a fence, after a stray tool call the judge must ignore.
    fn vote(score: u8, pass: bool) -> MockTurn {
        let json = format!(r#"{{"score": {score}, "pass": {pass}, "reason": "r{score}"}}"#);
        MockTurn::from_contents([
            AssistantContent::tool_call("t", "add", serde_json::json!({})),
            AssistantContent::text(format!("```json\n{json}\n```")),
        ])
    }

    #[tokio::test]
    async fn the_majority_decides_and_the_score_is_the_median() {
        let model = MockCompletionModel::new([vote(2, false), vote(4, true), vote(5, true)]);
        let judge = Judge::new(model.clone());
        let verdict = judge.judge("be right", &case(), &obs()).await.unwrap();
        assert_eq!(
            verdict,
            Verdict {
                score: 4.0,
                pass: true,
                reason: "r4".into()
            }
        );
        // What the judge was shown: rubric, every turn, tool calls, the error.
        let request = &model.requests()[0];
        // Rig carries the preamble as the history's leading system message.
        let shown = serde_json::to_string(&request).unwrap();
        for part in [
            "JSON object",
            "be right",
            "User: thanks",
            "(no reply)",
            "add({\\\"a\\\":2})",
            "boom",
        ] {
            assert!(shown.contains(part), "{part} not in {shown}");
        }
    }

    #[tokio::test]
    async fn unreadable_votes_are_skipped_and_all_bad_votes_fail_the_judge() {
        let model = MockCompletionModel::new([
            MockTurn::text("I think it's fine"),
            MockTurn::error("rate limited"),
            vote(1, false),
        ]);
        let verdict = Judge::new(model).judge("r", &case(), &obs()).await.unwrap();
        assert_eq!((verdict.score, verdict.pass), (1.0, false));

        let model = MockCompletionModel::new([
            MockTurn::text(r#"{"score": 9, "pass": true, "reason": "x"}"#),
            MockTurn::text("} {"),
            MockTurn::text(r#"{"score": "high"}"#),
        ]);
        let err = Judge::new(model)
            .judge("r", &case(), &obs())
            .await
            .unwrap_err();
        assert!(
            err.starts_with("judge failed: vote score 9 is not 1-5; unreadable vote"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_tie_does_not_pass() {
        let model = MockCompletionModel::new([vote(4, true), MockTurn::error("x"), vote(2, false)]);
        let verdict = Judge::new(model).judge("r", &case(), &obs()).await.unwrap();
        assert_eq!((verdict.pass, verdict.reason.as_str()), (false, "r2"));
    }
}
