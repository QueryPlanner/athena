//! The real Rig agent loop, production tools included, against a scripted
//! model and a real database file. No network.

mod common;

use athena::{agent, runner, store};
use common::*;
use rig_agent::prelude::Message;
use rig_core::test_utils::MockTurn;

/// Rig 0.42 sends the preamble as a leading System message in the request
/// history, not in `CompletionRequest::preamble`, and never stores it.
fn is_preamble(m: &Message) -> bool {
    matches!(m, Message::System { content } if content == agent::PREAMBLE)
}

#[tokio::test]
async fn a_tool_turn_runs_the_real_tool_and_records_what_it_cost() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, model) = mock_agent(add_turns());

    let reply = runner::turn(&agent, &db, "s", "mock/model", "add 21 and 21")
        .await
        .unwrap();

    assert_eq!(reply, "42");

    // Prompt, tool call, tool result, reply. The result came from the real
    // `add` tool: the mock only asked for it.
    let rows = raw_rows(&db, "s");
    assert_eq!(rows.len(), 4);
    assert!(rows[1].contains(r#""type":"toolcall""#), "{}", rows[1]);
    assert!(rows[2].contains(r#""type":"toolresult""#), "{}", rows[2]);
    assert!(rows[2].contains("42"), "{}", rows[2]);

    // The model was sent the production preamble and both tools.
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(is_preamble(&requests[0].chat_history[0]));
    let mut tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    tools.sort();
    assert_eq!(tools, ["add", "read_file"]);

    // One run, two model calls, four messages, and the summed token counts.
    assert_eq!(runs(&db, "s"), [run_row(0, 3, 2, "ok")]);
    let totals = &store::usage(&db).unwrap()[0];
    assert_eq!((totals.input_tokens, totals.output_tokens), (256, 26));
}

#[tokio::test]
async fn history_survives_a_restart_and_the_next_turn_builds_on_it() {
    let tmp = TempDb::new();
    {
        let db = tmp.open();
        let (agent, _) = mock_agent(add_turns());
        runner::turn(&agent, &db, "s", "m", "add 21 and 21")
            .await
            .unwrap();
    } // Connection closed, as when the CLI process exits.

    let db = tmp.open();
    let before = raw_rows(&db, "s");
    let restored = store::load(&db, "s").unwrap();
    let (agent, model) = mock_agent([MockTurn::text("21 and 21")]);

    runner::turn(&agent, &db, "s", "m", "what did I add?")
        .await
        .unwrap();

    // The model saw preamble, the whole earlier exchange (tool call and
    // result included), then the new prompt.
    let sent = &model.requests()[0].chat_history;
    assert_eq!(sent.len(), restored.len() + 2, "{sent:?}");
    assert!(is_preamble(&sent[0]));
    assert_eq!(&sent[1..=restored.len()], &restored[..]);
    assert_eq!(sent.last().unwrap(), &Message::user("what did I add?"));

    // save() rewrites the session; the earlier rows must come back unchanged.
    let after = raw_rows(&db, "s");
    assert_eq!(after.len(), 6);
    assert_eq!(&after[..4], &before[..]);
    assert_eq!(
        runs(&db, "s"),
        [run_row(0, 3, 2, "ok"), run_row(4, 5, 1, "ok")]
    );
}

#[tokio::test]
async fn a_provider_error_leaves_the_transcript_alone_and_is_recorded() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, _) = mock_agent(add_turns());
    runner::turn(&agent, &db, "s", "m", "add 21 and 21")
        .await
        .unwrap();
    let before = raw_rows(&db, "s");

    let (agent, _) = mock_agent([MockTurn::error("upstream unavailable")]);
    let err = runner::turn(&agent, &db, "s", "m", "again")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("upstream unavailable"), "{err}");
    assert_eq!(raw_rows(&db, "s"), before);
    // An empty range starting where the next message would have gone.
    assert_eq!(runs(&db, "s")[1], run_row(4, 3, 0, "error"));
}

#[tokio::test]
async fn sessions_do_not_see_each_other() {
    let tmp = TempDb::new();
    let db = tmp.open();
    let (agent, _) = mock_agent(add_turns());
    runner::turn(&agent, &db, "a", "m", "add 21 and 21")
        .await
        .unwrap();

    let (agent, model) = mock_agent([MockTurn::text("hello")]);
    runner::turn(&agent, &db, "b", "m", "hi").await.unwrap();

    // Session b's request carried the preamble and its own prompt, nothing of a.
    let sent = &model.requests()[0].chat_history;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(is_preamble(&sent[0]));
    assert_eq!(sent[1], Message::user("hi"));
    assert_eq!(raw_rows(&db, "a").len(), 4);
    assert_eq!(raw_rows(&db, "b").len(), 2);
    assert_eq!(runs(&db, "b"), [run_row(0, 1, 1, "ok")]);
}
