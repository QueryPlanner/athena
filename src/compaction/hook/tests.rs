//! The hook, driven the way production drives it: through the service, with
//! the production agent in front of a scripted model, a real store, and a
//! second scripted model writing the summaries.
//!
//! The window is 10 000 tokens, so compaction starts above 8 000, keeps at
//! least 2 000 tokens word for word, and expects a summary to cost 1 000.
//! "BIG" below is a message of about 1 000 tokens. What the provider
//! "reports" for each call is scripted, which is how a test says how full
//! the context is.

use super::*;
use crate::agent;
use crate::compaction::tests::pairing_problems;
use crate::compaction::{ModelSummarizer, Settings, Summarize, SummaryFuture};
use crate::media;
use crate::runner::Request;
use crate::service::{Error, Service, Session, Turn, TurnEvent, User};
use rig_agent as rig;
use rig_agent::agent::{Agent, AgentBuilder};
use rig_agent::rig_tool;
use rig_core::completion::{AssistantContent, Usage};
use rig_core::memory::{ConversationMemory, MemoryError};
use rig_core::message::UserContent;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};
use rig_core::wasm_compat::WasmBoxedFuture;
use std::time::Duration;

const WINDOW: u64 = 10_000;
const BIG: usize = 1_000;

type Warnings = Arc<Mutex<Vec<String>>>;

/// `label` followed by filler, about `tokens` tokens long.
fn words(label: &str, tokens: usize) -> String {
    format!("{label} {}", "w".repeat(tokens * 4))
}

fn reports(input_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens: 10,
        total_tokens: input_tokens + 10,
        ..Usage::new()
    }
}

/// A reply of `tokens` tokens, after a request the provider counted at `reported`.
fn reply(label: &str, tokens: usize, reported: u64) -> MockTurn {
    MockTurn::text(words(label, tokens)).with_usage(reports(reported))
}

fn add_call(id: &str, reported: u64) -> MockTurn {
    MockTurn::tool_call(id, "add", serde_json::json!({"a": 1, "b": 2}))
        .with_usage(reports(reported))
}

fn summary(text: &str) -> MockTurn {
    MockTurn::text(text).with_usage(Usage {
        input_tokens: 4_100,
        output_tokens: 60,
        total_tokens: 4_160,
        ..Usage::new()
    })
}

/// What each message of a request is, in a word: the first word of what was
/// said, or `call`, `result`, `system`, `summary`.
fn labels(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|message| match message {
            Message::System { .. } => "system".to_string(),
            Message::User { content }
                if content
                    .iter()
                    .any(|c| matches!(c, UserContent::ToolResult(_))) =>
            {
                "result".to_string()
            }
            Message::Assistant { content, .. }
                if content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_))) =>
            {
                "call".to_string()
            }
            other => {
                let text = other.rag_text().or_else(|| match other {
                    Message::Assistant { content, .. } => content.iter().find_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    }),
                    _ => None,
                });
                let text = text.unwrap_or_default();
                if text.starts_with("[Notes on the earlier conversation") {
                    "summary".to_string()
                } else {
                    text.split(' ').next().unwrap_or_default().to_string()
                }
            }
        })
        .collect()
}

/// A service with compaction on, a scripted summarizer, and one session.
struct Rig {
    service: Arc<Service>,
    store: Store,
    user: User,
    session: Session,
    summarizer: MockCompletionModel,
    warnings: Warnings,
}

fn settings(context_tokens: Option<u64>) -> Settings {
    Settings {
        compact_at: 0.8,
        model: "test/summarizer".into(),
        context_tokens,
    }
}

async fn rig_with(summarizer: impl Summarize + 'static, model: MockCompletionModel) -> Rig {
    let settings = settings(Some(WINDOW));
    rig_around(Compactor::new(settings, "test/model", summarizer), model).await
}

async fn rig_around(compactor: Compactor, model: MockCompletionModel) -> Rig {
    let store = Store::open_in_memory().unwrap();
    let warnings = Warnings::default();
    let sink = warnings.clone();
    let service = Service::new(store.clone(), "test/model", move |w| {
        sink.lock().unwrap().push(w.to_string())
    })
    .with_compactor(Some(compactor));
    let user = service.user("cli", "local").await.unwrap();
    let session = service.open_session(&user, "s").await.unwrap();
    Rig {
        service: Arc::new(service),
        store,
        user,
        session,
        summarizer: model,
        warnings,
    }
}

async fn rig(summaries: impl IntoIterator<Item = MockTurn>) -> Rig {
    let model = MockCompletionModel::new(summaries);
    rig_with(ModelSummarizer(model.clone()), model).await
}

impl Rig {
    /// The production agent in front of a scripted model.
    fn agent(&self, turns: impl IntoIterator<Item = MockTurn>) -> (Agent, MockCompletionModel) {
        let model = MockCompletionModel::new(turns);
        let builder = AgentBuilder::new(model.clone()).memory(self.service.memory());
        (agent::configure(builder), model)
    }

    async fn say(&self, agent: &Agent, text: &str) -> Result<Turn, Error> {
        self.service
            .send(agent, &self.user, &self.session.id, text)
            .await
    }

    async fn say_streaming(&self, agent: Agent, text: &str) -> Result<Turn, Error> {
        let mut stream = self
            .service
            .send_stream(Arc::new(agent), &self.user, &self.session.id, text)
            .await
            .unwrap();
        let mut done = None;
        while let Some(event) = stream.next().await {
            if let TurnEvent::Done(outcome) = event {
                done = Some(outcome);
            }
        }
        done.expect("a streamed turn ends with Done")
    }

    /// `(through_seq, summary, model, input, output)` of every checkpoint.
    async fn checkpoints(&self) -> Vec<(i64, String, String, i64, i64)> {
        self.store
            .call(|s| {
                let db = s.db_for_tests();
                let mut q = db
                    .prepare(
                        "SELECT through_seq, summary, model, input_tokens, output_tokens
                         FROM compactions ORDER BY through_seq",
                    )
                    .unwrap();
                q.query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            })
            .await
    }

    /// `(first_seq, last_seq, status, model, input_tokens)` of every run, as saved.
    async fn runs(&self) -> Vec<(i64, i64, String, String, i64)> {
        self.store
            .call(|s| {
                let db = s.db_for_tests();
                let mut q = db
                    .prepare(
                        "SELECT first_seq, last_seq, status, model, input_tokens
                         FROM runs ORDER BY rowid",
                    )
                    .unwrap();
                q.query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            })
            .await
    }

    /// What the next turn's memory load returns, as the model would see it.
    async fn visible(&self) -> Vec<String> {
        let id = self.session.id.clone();
        let memory = self.service.memory();
        labels(&memory.load(&id).await.unwrap())
    }

    async fn history_len(&self) -> usize {
        self.service
            .history(&self.user, &self.session.id)
            .await
            .unwrap()
            .len()
    }
}

/// Three turns of BIG messages. The last call of the third says the context
/// is 8 500 tokens, which is over the line.
fn three_big_turns() -> Vec<MockTurn> {
    vec![
        reply("A1", BIG, 1_100),
        reply("A2", BIG, 3_100),
        reply("A3", BIG, 8_500),
    ]
}

async fn talk_three_turns(rig: &Rig, agent: &Agent) {
    for (n, _) in (1..=3).zip(0..) {
        rig.say(agent, &words(&format!("U{n}"), BIG)).await.unwrap();
    }
}

/// Every request sent to the model must be one a provider accepts: no tool
/// result without its call, no call without its result.
fn assert_well_formed(model: &MockCompletionModel) {
    for (n, request) in model.requests().iter().enumerate() {
        let problems = pairing_problems(&request.chat_history);
        assert!(problems.is_empty(), "request {n}: {problems:?}");
    }
}

// ------------------------------------------------------------- before a turn

#[tokio::test(flavor = "multi_thread")]
async fn once_the_last_call_reported_over_the_line_the_next_turn_compacts_before_its_first_call() {
    let rig = rig([summary("SUMMARY the code word is LYNX-2211")]).await;
    let (agent, model) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 3_000)]].concat());
    talk_three_turns(&rig, &agent).await;
    // Nothing was near the line until now.
    assert_eq!(rig.summarizer.request_count(), 0);
    assert!(rig.checkpoints().await.is_empty());

    rig.say(&agent, "What was the code word?").await.unwrap();

    // One summary call, made before the model call it makes room for.
    assert_eq!(rig.summarizer.request_count(), 1);
    let sent = model.requests().pop().unwrap().chat_history;
    // The oldest four messages became the summary; the rest, a fifth of the
    // window and more, and the prompt, are as they were said.
    assert_eq!(labels(&sent), ["system", "summary", "U3", "A3", "What"]);
    let shown = sent[1].rag_text().unwrap();
    assert!(
        shown.ends_with("SUMMARY the code word is LYNX-2211"),
        "{shown}"
    );
    assert_well_formed(&model);
    // What the summarizer was shown is the oldest four messages.
    let asked = rig.summarizer.requests()[0]
        .chat_history
        .last()
        .unwrap()
        .rag_text()
        .unwrap();
    for said in ["User: U1 ", "Assistant: A1 ", "User: U2 ", "Assistant: A2 "] {
        assert!(asked.contains(said), "{said}");
    }
    assert!(!asked.contains("U3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_checkpoint_stands_in_for_the_rows_it_covers_and_nothing_is_lost() {
    let rig = rig([summary("SUMMARY lynx")]).await;
    let (agent, model) = rig.agent(
        [
            three_big_turns(),
            vec![reply("A4", BIG, 3_000), reply("A5", 10, 3_100)],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;

    let turn = rig.say(&agent, "What was the code word?").await.unwrap();

    // Rows 0 to 3 are turns 1 and 2. The checkpoint is saved after the turn's
    // own rows (6 and 7), and the turn's run is the turn's, not the summary's.
    assert_eq!(
        rig.checkpoints().await,
        [(
            3,
            "SUMMARY lynx".into(),
            "test/summarizer".into(),
            4_100,
            60
        )]
    );
    assert_eq!((turn.run.first_seq, turn.run.last_seq), (6, 7));
    assert_eq!(rig.history_len().await, 8, "history still has every row");
    // The summary call is a run of its own, with its own model and cost, and
    // it saved no messages.
    let runs = rig.runs().await;
    assert_eq!(runs.len(), 5);
    assert_eq!(
        runs[4],
        (6, 5, "ok".into(), "test/summarizer".into(), 4_100)
    );
    assert_eq!(runs[3], (6, 7, "ok".into(), "test/model".into(), 3_000));

    // The next turn loads the summary and the rows after it, nothing else.
    assert_eq!(rig.visible().await, ["summary", "U3", "A3", "What", "A4"]);
    rig.say(&agent, "And now?").await.unwrap();
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(
        labels(&sent),
        ["system", "summary", "U3", "A3", "What", "A4", "And"]
    );
    assert_eq!(rig.summarizer.request_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_under_the_line_is_never_touched() {
    let rig = rig([]).await;
    let (agent, model) = rig.agent([
        reply("A1", BIG, 1_100),
        reply("A2", BIG, 3_100),
        reply("A3", BIG, 5_500),
        reply("A4", BIG, 7_000),
    ]);
    for n in 1..=4 {
        rig.say(&agent, &words(&format!("U{n}"), BIG))
            .await
            .unwrap();
    }

    assert_eq!(rig.summarizer.request_count(), 0);
    assert!(rig.checkpoints().await.is_empty());
    // The last turn saw the whole history, as it always did.
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(
        labels(&sent),
        ["system", "U1", "A1", "U2", "A2", "U3", "A3", "U4"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_reports_no_usage_is_estimated_from_the_text() {
    let rig = rig([summary("SUMMARY unreported")]).await;
    // Nothing is reported, so the estimate is the text plus a fixed 4 000 for
    // the system prompt and tools: turn 3's request crosses 8 000.
    let (agent, model) = rig.agent([
        MockTurn::text(words("A1", BIG)),
        MockTurn::text(words("A2", BIG)),
        add_call("c1", 0),
        MockTurn::text("A3 done"),
    ]);
    rig.say(&agent, &words("U1", BIG)).await.unwrap();
    rig.say(&agent, &words("U2", BIG)).await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 0);

    rig.say(&agent, &words("U3", BIG)).await.unwrap();

    // Compacted once, before the first call, and the second call of the turn
    // (still nothing reported) kept using the summary instead of compacting again.
    assert_eq!(rig.summarizer.request_count(), 1);
    let requests = model.requests();
    let (first, second) = (&requests[2].chat_history, &requests[3].chat_history);
    assert_eq!(labels(first), ["system", "summary", "A2", "U3"]);
    assert_eq!(
        labels(second),
        ["system", "summary", "A2", "U3", "call", "result"]
    );
    assert_well_formed(&model);
    assert_eq!(rig.checkpoints().await.len(), 1);
}

// -------------------------------------------------------------- during a turn

#[rig_tool(description = "Return a block of text")]
fn blob(tokens: u64) -> Result<String, rig::tool::ToolExecutionError> {
    Ok("z".repeat(tokens as usize * 4))
}

fn blob_call(id: &str, tokens: u64, reported: u64) -> MockTurn {
    MockTurn::tool_call(id, "blob", serde_json::json!({"tokens": tokens}))
        .with_usage(reports(reported))
}

fn blob_agent(
    rig: &Rig,
    turns: impl IntoIterator<Item = MockTurn>,
) -> (Agent, MockCompletionModel) {
    let model = MockCompletionModel::new(turns);
    let agent = AgentBuilder::new(model.clone())
        .memory(rig.service.memory())
        .tool(Blob)
        .default_max_turns(usize::MAX)
        .build();
    (agent, model)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_loop_inside_one_turn_compacts_and_keeps_the_summary_for_its_later_calls() {
    let rig = rig([summary("SUMMARY of the first two turns")]).await;
    // Two earlier turns, then a turn of four model calls. The second reports
    // an 8 700 token request, so the third is compacted.
    let (agent, model) = rig.agent([
        reply("A1", BIG, 1_100),
        reply("A2", BIG, 3_100),
        add_call("c1", 4_200),
        add_call("c2", 8_700),
        add_call("c3", 4_000),
        reply("A3", 10, 4_200),
    ]);
    rig.say(&agent, &words("U1", BIG)).await.unwrap();
    rig.say(&agent, &words("U2", BIG)).await.unwrap();

    rig.say(&agent, "go").await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 1);
    let requests = model.requests();
    // The first two requests are the earlier turns'. The turn's four calls
    // are the rest; the third is the first over the line.
    let shape = |n: usize| labels(&requests[n].chat_history);
    assert_eq!(shape(2), ["system", "U1", "A1", "U2", "A2", "go"]);
    assert_eq!(
        shape(3),
        ["system", "U1", "A1", "U2", "A2", "go", "call", "result"]
    );
    assert_eq!(
        shape(4),
        [
            "system", "summary", "U2", "A2", "go", "call", "result", "call", "result"
        ],
        "compacted before this call"
    );
    // The patch lasts one call; the hook applies the summary again.
    assert_eq!(
        shape(5),
        [
            "system", "summary", "U2", "A2", "go", "call", "result", "call", "result", "call",
            "result"
        ]
    );
    assert_well_formed(&model);
    // The checkpoint points at the last message the summary covers: U1 and
    // A1 are rows 0 and 1.
    assert_eq!(
        rig.checkpoints()
            .await
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_inside_the_turn_itself_points_at_the_turns_own_rows() {
    let rig = rig([summary("SUMMARY of the first calls")]).await;
    let (agent, model) = blob_agent(
        &rig,
        [
            blob_call("c1", 1_500, 500),
            blob_call("c2", 1_500, 2_100),
            blob_call("c3", 1_500, 3_700),
            blob_call("c4", 1_500, 5_300),
            blob_call("c5", 1_500, 6_900),
            // The sixth request is over the line: 6 900 plus the new result.
            blob_call("c6", 1_500, 3_000),
            reply("done", 5, 4_500),
        ],
    );

    rig.say(&agent, "go").await.unwrap();

    let sent = model.requests().remove(5).chat_history;
    // Everything up to the third result became the summary; the last two
    // calls, with the first of their results the prompt, stay as they were.
    // (This agent has no preamble, so there is no system message.)
    assert_eq!(
        labels(&sent),
        ["summary", "call", "result", "call", "result"]
    );
    assert_well_formed(&model);
    // The turn's rows are: go, then a call and its result six times, then
    // the answer. The summary covers the first seven (0 to 6).
    assert_eq!(
        rig.checkpoints()
            .await
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [6]
    );
    assert_eq!(rig.history_len().await, 14);
    // A later turn starts from the summary and the seven rows after it.
    assert_eq!(rig.visible().await.len(), 8);
    assert_eq!(rig.visible().await[0], "summary");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_outgrows_the_window_twice_summarizes_the_first_summary_and_saves_only_the_last()
 {
    let rig = rig([summary("SUMMARY one"), summary("SUMMARY two")]).await;
    // The sixth request is over the line and is compacted; the tenth is over
    // it again, with the first summary already in use.
    let (agent, model) = blob_agent(
        &rig,
        [
            blob_call("c1", 1_500, 500),
            blob_call("c2", 1_500, 2_100),
            blob_call("c3", 1_500, 3_700),
            blob_call("c4", 1_500, 5_300),
            blob_call("c5", 1_500, 6_900),
            blob_call("c6", 1_500, 3_000),
            blob_call("c7", 1_500, 4_600),
            blob_call("c8", 1_500, 6_200),
            blob_call("c9", 1_500, 8_700),
            blob_call("c10", 1_500, 3_500),
            reply("done", 5, 4_000),
        ],
    );

    rig.say(&agent, "go").await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 2);
    let requests = model.requests();
    let shape = |n: usize| labels(&requests[n].chat_history);
    assert_eq!(shape(5)[..2], ["summary", "call"]);
    // The tenth request shows the second summary and the last two exchanges.
    assert_eq!(shape(9), ["summary", "call", "result", "call", "result"]);
    let second = &rig.summarizer.requests()[1];
    let asked = second.chat_history.last().unwrap().rag_text().unwrap();
    assert!(asked.contains("SUMMARY one"), "{asked}");
    assert!(asked.contains("[called blob"), "{asked}");
    let sent = requests[9].chat_history[0].rag_text().unwrap();
    assert!(sent.ends_with("SUMMARY two"), "{sent}");
    assert_well_formed(&model);
    // Only the newer summary is saved: it covers the older one's rows too.
    let saved = rig.checkpoints().await;
    assert_eq!(saved.len(), 1);
    assert_eq!((saved[0].0, saved[0].1.as_str()), (14, "SUMMARY two"));
    assert_eq!(rig.runs().await.iter().filter(|r| r.1 < r.0).count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_streaming_path_compacts_too() {
    let rig = rig([summary("SUMMARY streamed")]).await;
    let turns = |texts: &[(&str, usize, u64)]| -> Vec<Vec<MockStreamEvent>> {
        texts
            .iter()
            .map(|(label, tokens, reported)| {
                vec![
                    MockStreamEvent::text(words(label, *tokens)),
                    MockStreamEvent::final_response(reports(*reported)),
                ]
            })
            .collect()
    };
    let model = MockCompletionModel::from_stream_turns(turns(&[
        ("A1", BIG, 1_100),
        ("A2", BIG, 3_100),
        ("A3", BIG, 8_500),
        ("A4", BIG, 3_000),
    ]));
    let agent = |memory| agent::configure(AgentBuilder::new(model.clone()).memory(memory));
    for n in 1..=3 {
        let text = words(&format!("U{n}"), BIG);
        rig.say_streaming(agent(rig.service.memory()), &text)
            .await
            .unwrap();
    }
    assert_eq!(rig.summarizer.request_count(), 0);

    rig.say_streaming(agent(rig.service.memory()), "What was the code word?")
        .await
        .unwrap();

    assert_eq!(rig.summarizer.request_count(), 1);
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(labels(&sent), ["system", "summary", "U3", "A3", "What"]);
    assert_eq!(
        rig.checkpoints()
            .await
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [3]
    );
    assert_eq!(rig.history_len().await, 8);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_streamed_tool_loop_keeps_the_summary_on_its_later_calls() {
    let rig = rig([summary("SUMMARY mid stream")]).await;
    let call = |id: &str, reported: u64| {
        vec![
            MockStreamEvent::tool_call(id, "add", serde_json::json!({"a": 1, "b": 2})),
            MockStreamEvent::final_response(reports(reported)),
        ]
    };
    let text = |label: &str, reported: u64| {
        vec![
            MockStreamEvent::text(words(label, BIG)),
            MockStreamEvent::final_response(reports(reported)),
        ]
    };
    let model = MockCompletionModel::from_stream_turns([
        text("A1", 1_100),
        text("A2", 3_100),
        call("c1", 8_700),
        call("c2", 4_000),
        text("A3", 4_200),
    ]);
    let agent = || agent::configure(AgentBuilder::new(model.clone()).memory(rig.service.memory()));
    rig.say_streaming(agent(), &words("U1", BIG)).await.unwrap();
    rig.say_streaming(agent(), &words("U2", BIG)).await.unwrap();

    rig.say_streaming(agent(), "go").await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 1);
    let requests = model.requests();
    // The turn's calls are the third, fourth and fifth. The reported size of
    // the first is what the second's compaction is based on.
    assert_eq!(
        labels(&requests[2].chat_history),
        ["system", "U1", "A1", "U2", "A2", "go"]
    );
    assert_eq!(
        labels(&requests[3].chat_history),
        ["system", "summary", "U2", "A2", "go", "call", "result"]
    );
    assert_eq!(
        labels(&requests[4].chat_history),
        [
            "system", "summary", "U2", "A2", "go", "call", "result", "call", "result"
        ]
    );
    assert_well_formed(&model);
}

// ------------------------------------------------------------- when it fails

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_fails_leaves_no_checkpoint_but_the_summary_is_still_recorded() {
    let rig = rig([summary("SUMMARY unused"), summary("SUMMARY again")]).await;
    let (agent, _) = rig.agent(
        [
            three_big_turns(),
            vec![MockTurn::error("upstream down"), reply("A4", 10, 3_000)],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;

    let err = rig
        .say(&agent, "What was the code word?")
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Model(_)), "{err:?}");
    // The summary was paid for, so it is a run; nothing points at the
    // session's rows, which the turn never wrote.
    assert!(rig.checkpoints().await.is_empty());
    assert_eq!(rig.history_len().await, 6);
    let runs = rig.runs().await;
    assert_eq!(runs[3].2, "error");
    assert_eq!(
        runs[4],
        (6, 5, "ok".into(), "test/summarizer".into(), 4_100)
    );

    // Sending again compacts again, and this time it is saved.
    rig.say(&agent, "What was the code word?").await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 2);
    assert_eq!(rig.checkpoints().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_whose_rows_could_not_be_saved_leaves_no_checkpoint() {
    let rig = rig([summary("SUMMARY lost")]).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 3_000)]].concat());
    talk_three_turns(&rig, &agent).await;
    rig.store
        .call(|s| {
            s.db_for_tests()
                .execute_batch(
                    "CREATE TRIGGER fail BEFORE INSERT ON messages
                     BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                )
                .unwrap()
        })
        .await;

    let err = rig
        .say(&agent, "What was the code word?")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("disk full"), "{err}");
    assert_eq!(rig.summarizer.request_count(), 1);
    assert!(rig.checkpoints().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_that_fails_is_skipped_and_the_turn_carries_on_with_everything() {
    let rig = rig([MockTurn::error("rate limited")]).await;
    let (agent, model) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 8_900)]].concat());
    talk_three_turns(&rig, &agent).await;

    let turn = rig.say(&agent, "What was the code word?").await.unwrap();

    assert_eq!(turn.reply.split(' ').next(), Some("A4"));
    // The whole history went to the model, as it would have without compaction.
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(
        labels(&sent),
        ["system", "U1", "A1", "U2", "A2", "U3", "A3", "What"]
    );
    assert!(rig.checkpoints().await.is_empty());
    // The attempt is on record, with why it failed.
    let runs = rig.runs().await;
    assert_eq!(runs[4], (6, 5, "error".into(), "test/summarizer".into(), 0));
    let error: Option<String> = rig
        .store
        .call(|s| {
            s.db_for_tests()
                .query_row("SELECT error FROM runs WHERE status = 'error'", [], |r| {
                    r.get(0)
                })
                .unwrap()
        })
        .await;
    assert!(error.unwrap().contains("rate limited"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_summary_is_a_failed_one() {
    let rig = rig([summary("  \n")]).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 8_900)]].concat());
    talk_three_turns(&rig, &agent).await;

    rig.say(&agent, "What was the code word?").await.unwrap();

    assert!(rig.checkpoints().await.is_empty());
    let runs = rig.runs().await;
    assert_eq!(runs[4].2, "error");
}

/// A summarizer that never answers.
struct Hangs;

impl Summarize for Hangs {
    fn summarize<'a>(&'a self, _: &'a str, _: u64) -> SummaryFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_that_takes_too_long_is_given_up_on() {
    // Rebuilt with a short timeout, since the rig's compactor has the real one.
    let store = Store::open_in_memory().unwrap();
    let settings = Settings {
        compact_at: 0.8,
        model: "test/summarizer".into(),
        context_tokens: Some(WINDOW),
    };
    let compactor =
        Compactor::new(settings, "test/model", Hangs).summary_timeout(Duration::from_millis(50));
    let service = Service::new(store.clone(), "test/model", |_| {}).with_compactor(Some(compactor));
    let user = service.user("cli", "local").await.unwrap();
    let session = service.open_session(&user, "s").await.unwrap();
    let model =
        MockCompletionModel::new([three_big_turns(), vec![reply("A4", BIG, 8_900)]].concat());
    let agent = agent::configure(AgentBuilder::new(model.clone()).memory(service.memory()));
    for n in 1..=3 {
        service
            .send(&agent, &user, &session.id, words(&format!("U{n}"), BIG))
            .await
            .unwrap();
    }

    let turn = service
        .send(&agent, &user, &session.id, "What was the code word?")
        .await
        .unwrap();

    assert_eq!(turn.reply.split(' ').next(), Some("A4"));
    let (status, error): (String, String) = store
        .call(|s| {
            s.db_for_tests()
                .query_row(
                    "SELECT status, error FROM runs WHERE model = 'test/summarizer'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        })
        .await;
    assert_eq!(status, "error");
    assert!(error.contains("took longer than 0.05 seconds"), "{error}");
    assert_eq!(
        labels(&model.requests().pop().unwrap().chat_history).len(),
        8
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn after_a_failed_summary_it_is_tried_again_only_once_the_context_has_grown() {
    let rig = rig([
        MockTurn::error("rate limited"),
        summary("SUMMARY second try"),
    ])
    .await;
    // Two earlier turns, then a loop. Call 3 is the first over the line, and
    // the summary fails. Call 4 is barely bigger, so it does not try again;
    // call 5 is more than a twentieth of the window bigger, so it does.
    let (agent, model) = rig.agent([
        reply("A1", BIG, 1_100),
        reply("A2", BIG, 3_100),
        add_call("c1", 8_700),
        add_call("c2", 8_800),
        add_call("c3", 9_500),
        add_call("c4", 4_000),
        reply("done", 5, 4_100),
    ]);
    rig.say(&agent, &words("U1", BIG)).await.unwrap();
    rig.say(&agent, &words("U2", BIG)).await.unwrap();

    rig.say(&agent, "go").await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 2);
    let requests = model.requests();
    let first_word = |n: usize| labels(&requests[n].chat_history)[1].clone();
    // Calls 3 to 5 of the session (the turn's first three) are unpatched;
    // the call after the second summary is.
    assert_eq!(first_word(2), "U1");
    assert_eq!(first_word(3), "U1");
    assert_eq!(first_word(4), "U1");
    assert_eq!(first_word(5), "summary");
    assert_eq!(rig.checkpoints().await.len(), 1);
}

// ------------------------------------------------------------------ the cut

#[tokio::test(flavor = "multi_thread")]
async fn a_second_compaction_summarizes_the_first_summary_and_what_came_after() {
    let rig = rig([summary("SUMMARY one"), summary("SUMMARY two")]).await;
    let (agent, model) = rig.agent(
        [
            three_big_turns(),
            vec![reply("A4", BIG, 8_900), reply("A5", BIG, 3_000)],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;
    rig.say(&agent, "U4 short").await.unwrap();
    assert_eq!(rig.checkpoints().await.len(), 1);

    rig.say(&agent, "U5 short").await.unwrap();

    // The second summary was written from the first plus what followed it.
    assert_eq!(rig.summarizer.request_count(), 2);
    let asked = rig.summarizer.requests()[1]
        .chat_history
        .last()
        .unwrap()
        .rag_text()
        .unwrap();
    assert!(asked.contains("SUMMARY one"), "{asked}");
    assert!(asked.contains("User: U3 "), "{asked}");
    assert!(!asked.contains("A3"), "{asked}");
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(labels(&sent), ["system", "summary", "A3", "U4", "A4", "U5"]);
    // The newer checkpoint covers more rows; both are kept.
    let through: Vec<i64> = rig.checkpoints().await.iter().map(|c| c.0).collect();
    assert_eq!(through, [3, 4]);
    assert_eq!(rig.visible().await[..2], ["summary", "A3"]);
    assert_well_formed(&model);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_picture_counts_as_a_picture_and_not_as_its_bytes() {
    let png = {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend(vec![1u8; 200_000]);
        bytes
    };
    let photo = || Request {
        text: "what is this?".into(),
        files: vec![media::File {
            name: "a.png".into(),
            mime: Some("image/png".into()),
            size: png.len() as u64,
            image: media::shown(&png, Some("image/png")),
            saved: Err("no sandbox".into()),
        }],
        outbox: None,
        scheduled: false,
    };
    // The same history twice: 6 900 tokens at the end of the last turn. The
    // reply and a plain prompt make 7 000: under the line. A photo adds
    // 1 500, which is over.
    let script = || {
        [
            reply("A1", BIG, 1_100),
            reply("A2", 100, 6_900),
            reply("A3", 10, 3_000),
        ]
    };
    let with_photo = rig([summary("SUMMARY photo")]).await;
    let (agent, model) = with_photo.agent(script());
    with_photo.say(&agent, &words("U1", BIG)).await.unwrap();
    with_photo.say(&agent, &words("U2", BIG)).await.unwrap();
    assert_eq!(with_photo.summarizer.request_count(), 0);

    let photo_turn = with_photo
        .service
        .send(&agent, &with_photo.user, &with_photo.session.id, photo())
        .await
        .unwrap();

    assert_eq!(photo_turn.reply.split(' ').next(), Some("A3"));
    assert_eq!(with_photo.summarizer.request_count(), 1);
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(labels(&sent), ["system", "summary", "U2", "A2", "what"]);

    let without = rig([]).await;
    let (agent, _) = without.agent(script());
    without.say(&agent, &words("U1", BIG)).await.unwrap();
    without.say(&agent, &words("U2", BIG)).await.unwrap();
    without.say(&agent, "what is this?").await.unwrap();
    assert_eq!(without.summarizer.request_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_context_that_cannot_be_helped_by_cutting_is_left_alone() {
    let rig = rig([]).await;
    // One huge prompt, then a tool loop: nothing before it to summarize, and
    // summarizing it would leave nothing under the line.
    let (agent, model) = rig.agent([add_call("c1", 0), add_call("c2", 0), reply("ok", 5, 0)]);

    rig.say(&agent, &words("HUGE", 9_000)).await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 0);
    assert_eq!(model.request_count(), 3);
    assert!(rig.checkpoints().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_that_would_still_leave_the_request_over_the_line_is_not_made() {
    let rig = rig([]).await;
    // Everything older than the prompt is small enough to summarize, but the
    // prompt itself is over the line, and it is never cut.
    let (agent, _) = rig.agent([reply("A1", BIG, 1_100), reply("A2", 10, 9_000)]);
    rig.say(&agent, &words("U1", BIG)).await.unwrap();

    rig.say(&agent, &words("U2", 7_500)).await.unwrap();

    assert_eq!(rig.summarizer.request_count(), 0);
}

/// Memory that hides the first message from the agent: its history no longer
/// matches the rows the store loaded.
struct DropsTheFirst(crate::store::SqliteMemory);

impl ConversationMemory for DropsTheFirst {
    fn load<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        Box::pin(async move {
            let mut history = self.0.load(id).await?;
            history.remove(0);
            Ok(history)
        })
    }

    fn append<'a>(
        &'a self,
        id: &'a str,
        messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.0.append(id, messages)
    }

    fn clear<'a>(&'a self, id: &'a str) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        self.0.clear(id)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_that_does_not_match_the_stores_rows_is_never_compacted() {
    let rig = rig([summary("SUMMARY wrong")]).await;
    let (agent, _) = rig.agent(three_big_turns());
    talk_three_turns(&rig, &agent).await;
    // The same session through a memory that alters what it loads: the cut
    // could not be turned into a row.
    let model = MockCompletionModel::new([reply("A4", BIG, 3_000)]);
    let altered = agent::configure(
        AgentBuilder::new(model.clone()).memory(DropsTheFirst(rig.service.memory())),
    );

    let turn = rig.say(&altered, "What was the code word?").await.unwrap();

    assert_eq!(turn.reply.split(' ').next(), Some("A4"));
    assert_eq!(rig.summarizer.request_count(), 0);
    assert!(rig.checkpoints().await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_run_to_go_by_the_size_is_estimated_and_compaction_still_works() {
    let rig = rig([summary("SUMMARY estimated")]).await;
    // Rows from before there were runs, and then no runs table at all.
    let id = rig.session.id.clone();
    rig.store
        .call(move |s| {
            let rows: Vec<Message> = (1..=6)
                .map(|n| {
                    if n % 2 == 1 {
                        Message::user(words(&format!("U{n}"), BIG))
                    } else {
                        Message::assistant(words(&format!("A{n}"), BIG))
                    }
                })
                .collect();
            s.append(&id, None, &rows).unwrap();
            s.db_for_tests().execute_batch("DROP TABLE runs").unwrap();
        })
        .await;
    let (agent, model) = rig.agent([reply("done", 5, 0)]);

    let turn = rig.say(&agent, "go on").await.unwrap();

    assert_eq!(turn.reply, words("done", 5));
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(labels(&sent), ["system", "summary", "U5", "A6", "go"]);
    // The telemetry rows could not be saved, and that was said, not hidden.
    let warnings = rig.warnings.lock().unwrap().clone();
    assert!(
        warnings.iter().any(|w| w.contains("telemetry not saved")),
        "{warnings:?}"
    );
}

// ------------------------------------------------------ saving the outcome

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_that_cannot_be_saved_is_a_warning_and_the_reply_still_arrives() {
    let rig = rig([summary("SUMMARY unsaved")]).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 3_000)]].concat());
    talk_three_turns(&rig, &agent).await;
    rig.store
        .call(|s| {
            s.db_for_tests()
                .execute_batch(
                    "CREATE TRIGGER fail BEFORE INSERT ON compactions
                     BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                )
                .unwrap()
        })
        .await;

    let turn = rig.say(&agent, "What was the code word?").await.unwrap();

    assert_eq!(turn.reply.split(' ').next(), Some("A4"));
    let warnings = rig.warnings.lock().unwrap().clone();
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("compaction not saved")),
        "{warnings:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_without_a_compactor_sends_every_message_every_turn() {
    let store = Store::open_in_memory().unwrap();
    let service = Service::new(store, "test/model", |_| {}).with_compactor(None);
    let user = service.user("cli", "local").await.unwrap();
    let session = service.open_session(&user, "s").await.unwrap();
    let model =
        MockCompletionModel::new(three_big_turns().into_iter().chain([reply("A4", 5, 9_999)]));
    let agent = agent::configure(AgentBuilder::new(model.clone()).memory(service.memory()));
    for text in ["U1", "U2", "U3", "U4"] {
        service
            .send(&agent, &user, &session.id, text)
            .await
            .unwrap();
    }

    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(
        labels(&sent),
        ["system", "U1", "A1", "U2", "A2", "U3", "A3", "U4"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_hook_has_decided_nothing() {
    let store = Store::open_in_memory().unwrap();
    let settings = Settings {
        compact_at: 0.8,
        model: "m".into(),
        context_tokens: Some(WINDOW),
    };
    let summarizer = ModelSummarizer(MockCompletionModel::new([]));
    let compactor = Arc::new(Compactor::new(settings, "m", summarizer));

    let hook = ContextHook::new(compactor, store, "some-session");

    assert!(format!("{hook:?}").contains("some-session"));
    let nothing = hook.finish();
    assert!(nothing.checkpoint.is_none() && nothing.summaries.is_empty());
}

// ------------------------------------------------- what the review asked for

#[tokio::test(flavor = "multi_thread")]
async fn a_summarizer_that_is_down_is_not_asked_again_by_the_turns_that_follow() {
    let rig = rig([MockTurn::error("rate limited"), summary("SUMMARY retry")]).await;
    let (agent, _) = rig.agent(
        [
            three_big_turns(),
            vec![
                reply("A4", 10, 8_900),
                reply("A5", 10, 9_000),
                reply("A6", 10, 3_000),
            ],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;

    // The fourth turn is the first over the line, and the summary fails.
    rig.say(&agent, "q4").await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 1);
    // The fifth is over the line too, but barely bigger: not tried again, so
    // it does not wait for a summarizer that is not answering.
    rig.say(&agent, "q5").await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 1);
    assert!(rig.checkpoints().await.is_empty());

    // A big prompt makes the request more than a twentieth of the window
    // bigger than when it failed, and it is tried again.
    rig.say(&agent, &words("U6", 1_500)).await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 2);
    let through: Vec<i64> = rig.checkpoints().await.iter().map(|c| c.0).collect();
    assert_eq!(through, [4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_reply_and_the_new_prompt_count_on_top_of_what_the_provider_reported() {
    // The provider said 7 300 for the last request. The reply it got back
    // (about 1 000) and the new prompt are on top: 8 300, over the line. With
    // either left out it is not.
    let over = rig([summary("SUMMARY edge")]).await;
    let (agent, _) = over.agent(
        [
            vec![
                reply("A1", BIG, 1_100),
                reply("A2", BIG, 3_100),
                reply("A3", BIG, 7_300),
            ],
            vec![reply("A4", 10, 3_000)],
        ]
        .concat(),
    );
    talk_three_turns(&over, &agent).await;
    over.say(&agent, "q").await.unwrap();
    assert_eq!(over.summarizer.request_count(), 1);

    // The same, 400 tokens smaller: 7 900, under it.
    let under = rig([summary("SUMMARY unused")]).await;
    let (agent, _) = under.agent(
        [
            vec![
                reply("A1", BIG, 1_100),
                reply("A2", BIG, 3_100),
                reply("A3", BIG, 6_900),
            ],
            vec![reply("A4", 10, 3_000)],
        ]
        .concat(),
    );
    talk_three_turns(&under, &agent).await;
    under.say(&agent, "q").await.unwrap();
    assert_eq!(under.summarizer.request_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_inside_a_later_turn_of_a_session_that_is_already_summarized_points_at_the_right_row()
{
    let rig = rig([summary("SUMMARY first"), summary("SUMMARY second")]).await;
    let (agent, model) = blob_agent(
        &rig,
        [
            // A short first turn, so the second turn's rows start at seq 2.
            reply("ok", 1, 60),
            // Turn two: six tool calls, compacted before the seventh request.
            blob_call("c1", 1_500, 500),
            blob_call("c2", 1_500, 2_100),
            blob_call("c3", 1_500, 3_700),
            blob_call("c4", 1_500, 5_300),
            blob_call("c5", 1_500, 6_900),
            blob_call("c6", 1_500, 3_000),
            reply("done", 5, 4_000),
            // Turn three starts from that summary: three more calls, and the
            // fourth request is over the line again.
            blob_call("c7", 1_500, 4_100),
            blob_call("c8", 1_500, 5_700),
            blob_call("c9", 1_500, 7_300),
            reply("ok2", 5, 3_500),
        ],
    );
    rig.say(&agent, "hi").await.unwrap();
    rig.say(&agent, "go").await.unwrap();
    // Rows 0 and 1 are the first turn; the second compacted rows 0 to 8.
    assert_eq!(
        rig.checkpoints()
            .await
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>(),
        [8]
    );
    assert_eq!(rig.history_len().await, 16);
    assert_eq!(rig.visible().await.len(), 8);

    rig.say(&agent, "go2").await.unwrap();

    // The third turn loaded the summary and rows 9 to 15, then its own rows
    // began at 16. Its summary covers the first two of its exchanges: the
    // second call's result is row 18.
    let saved = rig.checkpoints().await;
    assert_eq!(
        saved
            .iter()
            .map(|c| (c.0, c.1.as_str()))
            .collect::<Vec<_>>(),
        [(8, "SUMMARY first"), (18, "SUMMARY second")]
    );
    let asked = rig.summarizer.requests()[1]
        .chat_history
        .last()
        .unwrap()
        .rag_text()
        .unwrap();
    assert!(asked.contains("SUMMARY first"), "{asked}");
    let requests = model.requests();
    assert_eq!(
        labels(&requests.last().unwrap().chat_history),
        ["summary", "call", "result", "call", "result"]
    );
    assert_well_formed(&model);
    assert_eq!(rig.history_len().await, 24);
    assert_eq!(
        rig.visible().await,
        ["summary", "call", "result", "call", "result", "ok2"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_that_is_not_the_stores_is_never_compacted() {
    let rig = rig([summary("SUMMARY none")]).await;
    // Nothing here was loaded from the store, so no row could be named.
    let model = MockCompletionModel::new([reply("never", 5, 0)]);
    let agent = agent::configure(
        AgentBuilder::new(model.clone())
            .memory(rig_core::memory::InMemoryConversationMemory::new()),
    );

    let err = rig.say(&agent, &words("HUGE", 9_000)).await.unwrap_err();

    assert!(err.to_string().contains("no conversation memory"), "{err}");
    assert_eq!(model.request_count(), 1);
    assert_eq!(rig.summarizer.request_count(), 0);
}

#[test]
fn a_compaction_that_leaves_the_request_over_the_line_is_not_followed_by_another_at_once() {
    let store = Store::open_in_memory().unwrap();
    let summarizer = ModelSummarizer(MockCompletionModel::new([]));
    let compactor = Arc::new(Compactor::new(settings(Some(WINDOW)), "m", summarizer));
    let hook = ContextHook::new(compactor.clone(), store, "s");

    // Usage that arrives when nothing was just compacted puts nothing on hold.
    hook.record_usage(reports(9_000));
    assert!(!compactor.held_off("s", 9_000, WINDOW));

    // After a compaction, the first number the provider gives for the smaller
    // request is the one to grow from. No number yet: still waiting for it.
    hook.state().after_compaction = true;
    hook.record_usage(Usage::new());
    assert!(hook.state().after_compaction);
    hook.record_usage(reports(9_000));
    assert!(!hook.state().after_compaction);
    assert!(compactor.held_off("s", 9_499, WINDOW));
    assert!(!compactor.held_off("s", 9_500, WINDOW));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_short_chat_never_waits_for_the_model_list() {
    use crate::compaction::tests::catalog_sequence;
    let (url, asked) = catalog_sequence(vec![(
        200,
        serde_json::json!({"data": [{"id": "test/model", "context_length": WINDOW}]}),
    )])
    .await;
    let model = MockCompletionModel::new([summary("SUMMARY listed")]);
    let compactor = Compactor::new(settings(None), "test/model", ModelSummarizer(model.clone()))
        .catalog_url(&url);
    let rig = rig_around(compactor, model).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", 10, 3_000)]].concat());

    talk_three_turns(&rig, &agent).await;
    // Three turns, none of them near any model's window: the list was not
    // asked for.
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);

    rig.say(&agent, "q").await.unwrap();

    // Over 8 000 of a 10 000 window, which only the list could say.
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(rig.summarizer.request_count(), 1);
}

// ------------------------------------------------ review of the first pass

/// Makes saving a checkpoint fail until `allow_checkpoints` is called.
async fn refuse_checkpoints(rig: &Rig) {
    rig.store
        .call(|s| {
            s.db_for_tests()
                .execute_batch(
                    "CREATE TRIGGER no_checkpoints BEFORE INSERT ON compactions
                     BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
                )
                .unwrap()
        })
        .await;
}

async fn allow_checkpoints(rig: &Rig) {
    rig.store
        .call(|s| {
            s.db_for_tests()
                .execute_batch("DROP TRIGGER no_checkpoints")
                .unwrap()
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsaved_checkpoint_does_not_leave_a_small_prompt_size_standing_for_the_transcript() {
    let rig = rig([summary("SUMMARY one"), summary("SUMMARY two")]).await;
    // Turn four compacts, so its last call reports the small request that
    // went out (3 000). Its checkpoint cannot be saved, so turn five loads the
    // whole transcript, about 8 000 tokens before its own prompt.
    let (agent, model) = rig.agent(
        [
            three_big_turns(),
            vec![reply("A4", BIG, 3_000), reply("A5", 10, 3_100)],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;
    refuse_checkpoints(&rig).await;
    rig.say(&agent, "q4").await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 1);
    assert!(rig.checkpoints().await.is_empty());
    allow_checkpoints(&rig).await;

    rig.say(&agent, "q5").await.unwrap();

    // The 3 000 was not taken for the size of what turn five loaded: it was
    // estimated from the messages, found over the line, and compacted again.
    assert_eq!(rig.summarizer.request_count(), 2);
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(labels(&sent)[..2], ["system", "summary"]);
    assert_eq!(rig.checkpoints().await.len(), 1);
    assert_well_formed(&model);
}

// ---- the size of a summary

#[tokio::test(flavor = "multi_thread")]
async fn the_summary_is_asked_for_within_the_budget() {
    let rig = rig([summary("SUMMARY short")]).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 3_000)]].concat());
    talk_three_turns(&rig, &agent).await;

    rig.say(&agent, "q4").await.unwrap();

    // A tenth of a 10 000-token window.
    let request = &rig.summarizer.requests()[0];
    assert_eq!(request.max_tokens, Some(1_000));
    let Message::System { content } = &request.chat_history[0] else {
        panic!("{:?}", request.chat_history);
    };
    assert!(content.contains("at most 600 words"), "{content}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_over_the_budget_is_a_failed_compaction() {
    // 1 600 tokens when 1 000 were asked for, and more than the 1 250 allowed.
    let rig = rig([summary(&words("SUMMARY", 1_600)), summary("SUMMARY short")]).await;
    let (agent, model) = rig.agent(
        [
            three_big_turns(),
            vec![reply("A4", BIG, 8_900), reply("A5", 10, 9_000)],
        ]
        .concat(),
    );
    talk_three_turns(&rig, &agent).await;

    let turn = rig.say(&agent, "q4").await.unwrap();

    // Skipped like any failed summary: the whole history went out, the
    // attempt is an error run saying why, and there is no checkpoint.
    assert_eq!(turn.reply.split(' ').next(), Some("A4"));
    let sent = model.requests().pop().unwrap().chat_history;
    assert_eq!(
        labels(&sent),
        ["system", "U1", "A1", "U2", "A2", "U3", "A3", "q4"]
    );
    assert!(rig.checkpoints().await.is_empty());
    assert_eq!(rig.runs().await[4].2, "error");
    let error: String = rig
        .store
        .call(|s| {
            s.db_for_tests()
                .query_row("SELECT error FROM runs WHERE status = 'error'", [], |r| {
                    r.get(0)
                })
                .unwrap()
        })
        .await;
    assert!(error.contains("over the 1250 tokens"), "{error}");

    // And it follows the same hold-off: not asked again until the request grew.
    rig.say(&agent, "q5").await.unwrap();
    assert_eq!(rig.summarizer.request_count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_summary_a_little_over_the_budget_but_inside_the_slack_is_used() {
    // 1 200 tokens by our count: over the 1 000 asked for, under 1 250.
    let rig = rig([summary(&words("SUMMARY", 1_200))]).await;
    let (agent, _) = rig.agent([three_big_turns(), vec![reply("A4", BIG, 3_000)]].concat());
    talk_three_turns(&rig, &agent).await;

    rig.say(&agent, "q4").await.unwrap();

    assert_eq!(rig.checkpoints().await.len(), 1);
}
