//! The real Rig agent loop, production tools and conversation memory
//! included, through the service, against a scripted model and a real
//! database file. No network.

mod common;

use athena::agent;
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
    let (service, warnings) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let (agent, model) = mock_agent(&service, add_turns());

    let turn = service
        .send(&agent, &user, &s.id, "add 21 and 21")
        .await
        .unwrap();

    assert_eq!(turn.reply, "42");
    assert_eq!((turn.run.first_seq, turn.run.last_seq), (0, 3));

    // Prompt, tool call, tool result, reply. The result came from the real
    // `add` tool: the mock only asked for it.
    let db = tmp.raw();
    let rows = raw_rows(&db, &s.id);
    assert_eq!(rows.len(), 4);
    assert!(rows[1].contains(r#""type":"toolcall""#), "{}", rows[1]);
    assert!(rows[2].contains(r#""type":"toolresult""#), "{}", rows[2]);
    assert!(rows[2].contains("42"), "{}", rows[2]);

    // The model was sent the production preamble and its one host tool.
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(is_preamble(&requests[0].chat_history[0]));
    let mut tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    tools.sort();
    assert_eq!(tools, ["add"]);

    // One run, two model calls, four messages, and the summed token counts.
    assert_eq!(runs(&db, &s.id), [run_row(0, 3, 2, "ok")]);
    let totals = &service.usage(&user).await.unwrap()[0];
    assert_eq!((totals.input_tokens, totals.output_tokens), (256, 26));
    assert!(warnings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn history_survives_a_restart_and_the_next_turn_builds_on_it() {
    let tmp = TempDb::new();
    let id = {
        let (service, _) = tmp.service();
        let user = cli_user(&service).await;
        let s = session(&service, &user, "s").await;
        let (agent, _) = mock_agent(&service, add_turns());
        service
            .send(&agent, &user, &s.id, "add 21 and 21")
            .await
            .unwrap();
        s.id
    }; // Store dropped, as when the CLI process exits.

    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let before = raw_rows(&tmp.raw(), &id);
    let restored = service.history(&user, &id).await.unwrap();
    let (agent, model) = mock_agent(&service, [MockTurn::text("21 and 21")]);

    service
        .send(&agent, &user, &id, "what did I add?")
        .await
        .unwrap();

    // The model saw preamble, the whole earlier exchange (tool call and
    // result included), then the new prompt. Rig loaded it from memory.
    let sent = &model.requests()[0].chat_history;
    assert_eq!(sent.len(), restored.len() + 2, "{sent:?}");
    assert!(is_preamble(&sent[0]));
    assert_eq!(&sent[1..=restored.len()], &restored[..]);
    assert_eq!(sent.last().unwrap(), &Message::user("what did I add?"));

    // Appended, not rewritten: the earlier rows are the same bytes.
    let db = tmp.raw();
    let after = raw_rows(&db, &id);
    assert_eq!(after.len(), 6);
    assert_eq!(&after[..4], &before[..]);
    assert_eq!(
        runs(&db, &id),
        [run_row(0, 3, 2, "ok"), run_row(4, 5, 1, "ok")]
    );
}

#[tokio::test]
async fn a_provider_error_leaves_the_transcript_alone_and_is_recorded() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let s = session(&service, &user, "s").await;
    let (agent, _) = mock_agent(&service, add_turns());
    service
        .send(&agent, &user, &s.id, "add 21 and 21")
        .await
        .unwrap();
    let before = raw_rows(&tmp.raw(), &s.id);

    let (agent, _) = mock_agent(&service, [MockTurn::error("upstream unavailable")]);
    let err = service
        .send(&agent, &user, &s.id, "again")
        .await
        .unwrap_err();

    assert!(err.to_string().contains("upstream unavailable"), "{err}");
    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &s.id), before);
    // An empty range starting where the next message would have gone.
    assert_eq!(runs(&db, &s.id)[1], run_row(4, 3, 0, "error"));
}

#[tokio::test]
async fn sessions_do_not_see_each_other() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = cli_user(&service).await;
    let a = session(&service, &user, "a").await;
    let b = session(&service, &user, "b").await;
    let (agent, _) = mock_agent(&service, add_turns());
    service
        .send(&agent, &user, &a.id, "add 21 and 21")
        .await
        .unwrap();

    let (agent, model) = mock_agent(&service, [MockTurn::text("hello")]);
    service.send(&agent, &user, &b.id, "hi").await.unwrap();

    // Session b's request carried the preamble and its own prompt, nothing of a.
    let sent = &model.requests()[0].chat_history;
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(is_preamble(&sent[0]));
    assert_eq!(sent[1], Message::user("hi"));
    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &a.id).len(), 4);
    assert_eq!(raw_rows(&db, &b.id).len(), 2);
    assert_eq!(runs(&db, &b.id), [run_row(0, 1, 1, "ok")]);
}

#[tokio::test]
async fn two_users_with_the_same_session_name_have_separate_conversations() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let alice = service.user("telegram", "1001").await.unwrap();
    let bob = service.user("telegram", "1002").await.unwrap();
    let hers = session(&service, &alice, "default").await;
    let his = session(&service, &bob, "default").await;
    let (agent, _) = mock_agent(&service, [MockTurn::text("noted")]);
    service
        .send(&agent, &alice, &hers.id, "my code word is ZEBRA-7391")
        .await
        .unwrap();

    let (agent, model) = mock_agent(&service, [MockTurn::text("no idea")]);
    service
        .send(&agent, &bob, &his.id, "what is my code word?")
        .await
        .unwrap();

    assert_ne!(hers.id, his.id);
    let sent = format!("{:?}", model.requests()[0].chat_history);
    assert!(!sent.contains("ZEBRA"), "{sent}");
    // And Bob cannot reach Alice's session by id.
    assert!(service.history(&bob, &hers.id).await.is_err());
    assert_eq!(service.sessions(&bob).await.unwrap().len(), 1);
}

#[tokio::test]
async fn two_messages_arriving_together_on_one_session_run_in_turn() {
    let tmp = TempDb::new();
    let (service, _) = tmp.service();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "chat").await;
    let (agent, model) = mock_agent(&service, [MockTurn::text("one"), MockTurn::text("two")]);

    // Both in flight at once, as two Telegram updates for one chat would be.
    let (a, b) = tokio::join!(
        service.send(&agent, &user, &s.id, "first"),
        service.send(&agent, &user, &s.id, "second"),
    );

    a.unwrap();
    b.unwrap();
    // Whichever ran second was sent the other's whole exchange.
    let requests = model.requests();
    assert_eq!(requests[0].chat_history.len(), 2);
    assert_eq!(requests[1].chat_history.len(), 4);
    let db = tmp.raw();
    assert_eq!(raw_rows(&db, &s.id).len(), 4);
    assert_eq!(
        runs(&db, &s.id),
        [run_row(0, 1, 1, "ok"), run_row(2, 3, 1, "ok")]
    );
}
