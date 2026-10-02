//! Tests of the parts of compaction that do not depend on Rig's hooks. The
//! hook has its own, next to it, driven through the service.

use super::*;
use crate::media;
use rig_core::completion::AssistantContent;
use rig_core::message::{AudioMediaType, ToolResultContent, VideoMediaType};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

/// A user message of about `tokens` tokens.
fn said(tokens: usize) -> Message {
    Message::user("x".repeat(tokens * CHARS_PER_TOKEN))
}

fn replied(tokens: usize) -> Message {
    Message::assistant("y".repeat(tokens * CHARS_PER_TOKEN))
}

fn call(id: &str) -> Message {
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::tool_call(
            id,
            "add",
            json!({"a": 1, "b": 2}),
        )],
    }
}

fn result(id: &str, tokens: usize) -> Message {
    Message::tool_result(id, "add", "z".repeat(tokens * CHARS_PER_TOKEN))
}

fn view<'a>(messages: &'a [Message]) -> View<'a> {
    let (prompt, history) = messages.split_last().unwrap();
    View { history, prompt }
}

// ----------------------------------------------------------------- settings

#[test]
fn settings_default_to_eighty_percent_and_the_agents_own_model() {
    let settings = Settings::parse(None, None, None, "a/model").unwrap();
    assert_eq!(
        settings,
        Settings {
            compact_at: 0.8,
            model: "a/model".into(),
            context_tokens: None,
        }
    );
}

#[test]
fn settings_take_each_variable_and_treat_blank_as_unset() {
    let some = |s: &str| Some(s.to_string());
    let settings =
        Settings::parse(some("0.5"), some("b/cheap"), some("200000"), "a/model").unwrap();
    assert_eq!(
        settings,
        Settings {
            compact_at: 0.5,
            model: "b/cheap".into(),
            context_tokens: Some(200_000),
        }
    );
    let blank = Settings::parse(some(" "), some(""), some("  "), "a/model").unwrap();
    assert_eq!(blank, Settings::parse(None, None, None, "a/model").unwrap());
}

#[test]
fn settings_that_cannot_work_are_refused_with_the_variable_named() {
    let some = |s: &str| Some(s.to_string());
    for (at, tokens, named) in [
        ("lots", "5000", "ATHENA_COMPACT_AT"),
        ("0.2", "5000", "ATHENA_COMPACT_AT"),
        ("0.99", "5000", "ATHENA_COMPACT_AT"),
        ("NaN", "5000", "ATHENA_COMPACT_AT"),
        ("0.8", "999", "ATHENA_CONTEXT_TOKENS"),
        ("0.8", "7999", "ATHENA_CONTEXT_TOKENS"),
        ("0.8", "-5", "ATHENA_CONTEXT_TOKENS"),
        ("0.8", "128k", "ATHENA_CONTEXT_TOKENS"),
    ] {
        let err = Settings::parse(some(at), None, some(tokens), "m").unwrap_err();
        assert!(err.to_string().contains(named), "{at} {tokens}: {err}");
    }
}

// ---------------------------------------------------------- the window

#[test]
fn the_window_is_what_the_catalog_lists_for_the_model() {
    let catalog = json!({"data": [
        {"id": "a/one", "context_length": 8_000},
        {"id": "a/two", "context_length": 1_050_000},
        {"id": "a/two:free", "context_length": 32_000},
        {"id": "a/unknown", "context_length": null},
        {"id": "a/zero", "context_length": 0},
    ]});
    assert_eq!(context_length(&catalog, "a/two"), Some(1_050_000));
    // A variant the list has is its own entry; one it lacks is its base model.
    assert_eq!(context_length(&catalog, "a/two:free"), Some(32_000));
    assert_eq!(context_length(&catalog, "a/one:nitro"), Some(8_000));
    for missing in ["a/unknown", "a/zero", "b/none", "b/none:online"] {
        assert_eq!(context_length(&catalog, missing), None, "{missing}");
    }
    assert_eq!(context_length(&json!({"error": "no"}), "a/one"), None);
    assert_eq!(context_length(&json!({"data": "no"}), "a/one"), None);
}

/// A fake model list on a local port, and how many times it was asked. The
/// nth request gets the nth reply, and the last reply after that.
pub(crate) async fn catalog_sequence(
    replies: Vec<(u16, serde_json::Value)>,
) -> (String, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let app = axum::Router::new().route(
        "/models",
        axum::routing::get(move || {
            let nth = counter.fetch_add(1, Ordering::SeqCst);
            let (status, body) = replies[nth.min(replies.len() - 1)].clone();
            async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(body),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/models", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (url, asked)
}

async fn catalog(status: u16, body: serde_json::Value) -> (String, Arc<AtomicUsize>) {
    catalog_sequence(vec![(status, body)]).await
}

fn listing(tokens: u64) -> serde_json::Value {
    json!({"data": [{"id": "a/model", "context_length": tokens}]})
}

fn compactor(settings: Settings, model: &str, url: &str) -> Compactor {
    let summarizer = ModelSummarizer(MockCompletionModel::new([]));
    Compactor::new(settings, model, summarizer).catalog_url(url)
}

fn settings() -> Settings {
    Settings::parse(None, None, None, "a/model").unwrap()
}

#[tokio::test]
async fn the_window_is_looked_up_once_and_remembered() {
    let (url, asked) = catalog(200, listing(200_000)).await;
    let compactor = compactor(settings(), "a/model", &url);

    assert_eq!(compactor.window().await, 200_000);
    assert_eq!(compactor.window().await, 200_000);

    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(compactor.threshold(200_000), 160_000);
}

#[tokio::test]
async fn the_environments_window_needs_no_lookup() {
    let settings = Settings {
        context_tokens: Some(50_000),
        compact_at: 0.5,
        ..settings()
    };
    // Nothing listens here: asking would fail.
    let compactor = compactor(settings, "a/model", "http://127.0.0.1:1/models");

    assert_eq!(compactor.window().await, 50_000);
    assert_eq!(compactor.window_for(10).await, Some(50_000));
    assert_eq!(compactor.threshold(50_000), 25_000);
}

#[tokio::test]
async fn a_window_that_cannot_be_found_is_a_conservative_default() {
    let (listed, _) = catalog(200, json!({"data": [{"id": "other", "context_length": 9}]})).await;
    let (failing, _) = catalog(500, json!({"error": "down"})).await;
    let (garbled, _) = catalog(200, json!("not a catalog")).await;
    for url in [
        listed.as_str(),
        failing.as_str(),
        garbled.as_str(),
        // Nothing listens on port 1.
        "http://127.0.0.1:1/models",
    ] {
        let compactor = compactor(settings(), "a/model", url);
        assert_eq!(compactor.window().await, DEFAULT_WINDOW, "{url}");
    }
}

#[tokio::test]
async fn a_failed_lookup_is_believed_for_a_while_and_then_made_again() {
    // The first answer is an error, the rest are the real thing.
    let replies = vec![(500, json!({"error": "blip"})), (200, listing(32_000))];

    let (url, asked) = catalog_sequence(replies.clone()).await;
    let patient = compactor(settings(), "a/model", &url).window_retry(Duration::from_secs(3_600));
    assert_eq!(patient.window().await, DEFAULT_WINDOW);
    assert_eq!(patient.window().await, DEFAULT_WINDOW);
    assert_eq!(asked.load(Ordering::SeqCst), 1, "not asked again yet");

    let (url, asked) = catalog_sequence(replies).await;
    let eager = compactor(settings(), "a/model", &url).window_retry(Duration::ZERO);
    assert_eq!(eager.window().await, DEFAULT_WINDOW);
    // Asked again, and this time it is known for good.
    assert_eq!(eager.window().await, 32_000);
    assert_eq!(eager.window().await, 32_000);
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_small_request_does_not_make_the_window_be_looked_up() {
    let (url, asked) = catalog(200, listing(32_000)).await;
    let compactor = compactor(settings(), "a/model", &url);

    // Too small to be near any model's window: no network.
    assert_eq!(compactor.window_for(LOOKUP_FLOOR_TOKENS - 1).await, None);
    assert_eq!(asked.load(Ordering::SeqCst), 0);

    // A bigger one needs it, and after that every size does.
    assert_eq!(
        compactor.window_for(LOOKUP_FLOOR_TOKENS).await,
        Some(32_000)
    );
    assert_eq!(compactor.window_for(10).await, Some(32_000));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn while_a_failed_lookup_is_believed_even_a_small_request_has_a_window() {
    let (url, asked) = catalog(500, json!({"error": "down"})).await;
    let compactor = compactor(settings(), "a/model", &url);
    assert_eq!(
        compactor.window_for(LOOKUP_FLOOR_TOKENS).await,
        Some(DEFAULT_WINDOW)
    );

    assert_eq!(compactor.window_for(10).await, Some(DEFAULT_WINDOW));
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    // Once the belief has run out, a small request is back to not asking.
    let (url, _) = catalog(500, json!({"error": "down"})).await;
    let lapsed = compactor_with_retry(&url, Duration::ZERO);
    assert_eq!(
        lapsed.window_for(LOOKUP_FLOOR_TOKENS).await,
        Some(DEFAULT_WINDOW)
    );
    assert_eq!(lapsed.window_for(10).await, None);
}

fn compactor_with_retry(url: &str, retry: Duration) -> Compactor {
    compactor(settings(), "a/model", url).window_retry(retry)
}

#[test]
fn a_session_whose_compaction_did_not_help_waits_until_its_request_has_grown() {
    let compactor = compactor(settings(), "a/model", "http://127.0.0.1:1/models");
    assert!(!compactor.held_off("s", 9_000, 10_000));

    compactor.hold_off("s", 9_000);

    // A twentieth of a 10 000-token window is 500 tokens.
    assert!(compactor.held_off("s", 9_000, 10_000));
    assert!(compactor.held_off("s", 9_499, 10_000));
    assert!(!compactor.held_off("s", 9_500, 10_000));
    assert!(!compactor.held_off("other", 9_000, 10_000));
    compactor.release("s");
    assert!(!compactor.held_off("s", 9_000, 10_000));
    compactor.release("never held");
}

#[test]
fn only_so_many_sessions_are_remembered_as_held_off() {
    let compactor = compactor(settings(), "a/model", "http://127.0.0.1:1/models");
    for n in 0..MAX_HELD_OFF {
        compactor.hold_off(&format!("s{n}"), 9_000);
    }
    assert!(compactor.held_off("s0", 9_000, 10_000));

    compactor.hold_off("one more", 9_000);

    // Past the limit the old ones are forgotten rather than kept for ever.
    assert!(!compactor.held_off("s0", 9_000, 10_000));
    assert!(compactor.held_off("one more", 9_000, 10_000));
}

// ---------------------------------------------------------------- sizes

#[test]
fn text_costs_a_quarter_token_a_character_plus_the_message_around_it() {
    let small = tokens_of(&said(10));
    let big = tokens_of(&said(1010));
    assert_eq!(big - small, 1000);
    assert!((10..30).contains(&small), "{small}");
}

#[test]
fn an_image_costs_the_same_however_big_its_bytes_are() {
    let image = |bytes: &[u8]| Message::User {
        content: vec![UserContent::Image(media::image(bytes).unwrap())],
    };
    let mut big = PNG.to_vec();
    big.extend(vec![7u8; 300_000]);

    let (small, large) = (tokens_of(&image(PNG)), tokens_of(&image(&big)));

    assert_eq!(small, large);
    assert!(
        (IMAGE_TOKENS..IMAGE_TOKENS + 20).contains(&small),
        "{small}"
    );
}

#[test]
fn images_in_tool_results_and_replies_count_too() {
    let tool_image = Message::User {
        content: vec![UserContent::tool_result(
            "c",
            "screenshot",
            vec![ToolResultContent::Image(media::image(PNG).unwrap())],
        )],
    };
    let reply_image = Message::Assistant {
        id: None,
        content: vec![AssistantContent::Image(media::image(PNG).unwrap())],
    };
    for message in [tool_image, reply_image] {
        assert!(tokens_of(&message) >= IMAGE_TOKENS);
    }
}

#[test]
fn numbers_and_other_scalars_cost_something() {
    let message = Message::User {
        content: vec![UserContent::tool_result(
            "c",
            "add",
            vec![ToolResultContent::Json {
                value: json!({"n": 42.0, "ok": true, "none": null}),
            }],
        )],
    };
    assert!(tokens_of(&message) > 10);
}

// ------------------------------------------------------------- estimating

#[test]
fn what_the_provider_reported_is_the_base_and_only_new_messages_are_added() {
    let messages = [said(100), replied(100), said(100), replied(100), said(50)];
    let view = view(&messages);
    let reported = Some(Reported {
        tokens: 5_000,
        counted: 3,
    });

    let estimated = estimate(reported, None, &view);

    // The reported request held the first three messages.
    let added = tokens_of(&messages[3]) + tokens_of(&messages[4]);
    assert_eq!(estimated, 5_000 + added as u64);
}

#[test]
fn with_nothing_reported_everything_is_estimated_plus_the_fixed_overhead() {
    let messages = [said(100), replied(100), said(50)];
    let all: usize = messages.iter().map(tokens_of).sum();

    let estimated = estimate(None, None, &view(&messages));

    assert_eq!(estimated, (FIXED_OVERHEAD_TOKENS + all) as u64);
}

#[test]
fn with_nothing_reported_a_summary_stands_in_for_what_it_covers() {
    let messages = [said(1000), replied(1000), said(50), replied(20), said(10)];
    let summary = summary_message("short");
    let kept: usize = messages[2..].iter().map(tokens_of).sum();

    let estimated = estimate(None, Some((&summary, 2)), &view(&messages));

    let expected = FIXED_OVERHEAD_TOKENS + tokens_of(&summary) + kept;
    assert_eq!(estimated, expected as u64);
}

// ---------------------------------------------------------------- cutting

#[test]
fn the_cut_is_the_latest_one_that_keeps_enough() {
    // 100 tokens each (plus a little).
    let messages: Vec<Message> = (0..10)
        .map(|i| if i % 2 == 0 { said(100) } else { replied(100) })
        .collect();
    let kept = |from: usize| {
        messages[from..]
            .iter()
            .map(|m| tokens_of(m) as u64)
            .sum::<u64>()
    };

    let cut = choose_cut(&view(&messages), 1, kept(7)).unwrap();

    // The last three messages are just enough, and a later cut keeps fewer.
    assert!(kept(8) < kept(7));
    assert_eq!(
        cut,
        Cut {
            index: 7,
            kept_tokens: kept(7)
        }
    );
}

#[test]
fn a_cut_never_leaves_a_tool_result_without_its_call() {
    // The last three messages would be enough, but the third from the end is
    // a tool result: the call before it has to come along.
    let messages = [
        said(100),
        call("c1"),
        result("c1", 100),
        replied(100),
        said(100),
    ];
    let each = tokens_of(&replied(100)) as u64;

    let cut = choose_cut(&view(&messages), 1, each * 3 - 10).unwrap();

    assert_eq!(cut.index, 1);
    assert!(may_start_the_kept(&messages[cut.index]));
}

#[test]
fn when_the_prompt_is_a_tool_result_its_call_is_kept_with_it() {
    let messages = [
        said(500),
        replied(500),
        said(5),
        call("c9"),
        result("c9", 5),
    ];

    // Keeping very little would allow cutting right before the prompt, which
    // would strand it.
    let cut = choose_cut(&view(&messages), 1, 1).unwrap();

    assert_eq!(cut.index, 3);
    assert!(matches!(messages[cut.index], Message::Assistant { .. }));
}

#[test]
fn a_cut_leaves_the_prompt_and_asks_for_something_to_summarize() {
    let messages = [said(100), replied(100), said(100)];
    let each = tokens_of(&messages[0]) as u64;

    // Everything is needed to keep this much, so there is nothing to cut.
    assert_eq!(choose_cut(&view(&messages), 1, each * 4), None);
    // Nothing past the floor: the first two messages are already covered.
    assert_eq!(choose_cut(&view(&messages), 3, 1), None);
    assert_eq!(choose_cut(&view(&messages), 99, 1), None);
    // Just the prompt is kept, and the rest is summarized.
    assert_eq!(choose_cut(&view(&messages), 1, 1).unwrap().index, 2,);
}

/// A deterministic stand-in for a random generator.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The ids of the tool calls in `messages` that no earlier message answers
/// for, and the tool results that come without a call before them.
pub(crate) fn pairing_problems(messages: &[Message]) -> Vec<String> {
    let mut open: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    for message in messages {
        match message {
            Message::Assistant { content, .. } => {
                for part in content {
                    if let AssistantContent::ToolCall(call) = part {
                        open.push(call.id.as_str().to_string());
                    }
                }
            }
            Message::User { content } => {
                for part in content {
                    if let UserContent::ToolResult(result) = part {
                        match open.iter().position(|id| id == result.call.as_str()) {
                            Some(at) => {
                                open.remove(at);
                            }
                            None => problems.push(format!("result {} has no call", result.call)),
                        }
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    problems.extend(
        open.into_iter()
            .map(|id| format!("call {id} has no result")),
    );
    problems
}

/// A well-formed history: plain exchanges and tool exchanges (one or two
/// calls answered by one message), ending in a prompt that is a user
/// message or a tool result.
fn generated_history(rng: &mut Lcg) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut next_id = 0;
    for _ in 0..rng.below(12) + 2 {
        let size = (rng.below(300) + 5) as usize;
        messages.push(said(size));
        if rng.below(2) == 0 {
            let calls = rng.below(2) + 1;
            let ids: Vec<String> = (0..calls)
                .map(|_| {
                    next_id += 1;
                    format!("c{next_id}")
                })
                .collect();
            messages.push(Message::Assistant {
                id: None,
                content: ids
                    .iter()
                    .map(|id| AssistantContent::tool_call(id, "add", json!({})))
                    .collect(),
            });
            messages.push(Message::User {
                content: ids
                    .iter()
                    .map(|id| {
                        UserContent::tool_result(
                            id,
                            "add",
                            vec![ToolResultContent::text("r".repeat(size * 4))],
                        )
                    })
                    .collect(),
            });
        }
        messages.push(replied((rng.below(300) + 5) as usize));
    }
    // Sometimes the run is mid tool loop: the prompt is a tool result.
    if rng.below(2) == 0 {
        next_id += 1;
        let id = format!("c{next_id}");
        messages.push(call(&id));
        messages.push(result(&id, 50));
    } else {
        messages.push(said(30));
    }
    messages
}

#[test]
fn whatever_the_history_a_cut_never_splits_a_tool_exchange() {
    let mut rng = Lcg(7);
    let mut cuts = 0;
    for _ in 0..2_000 {
        let messages = generated_history(&mut rng);
        assert_eq!(pairing_problems(&messages), Vec::<String>::new());
        let view = view(&messages);
        let floor = rng.below(4) as usize + 1;
        let keep = rng.below(2_000);

        let Some(cut) = choose_cut(&view, floor, keep) else {
            continue;
        };
        cuts += 1;

        let kept = &messages[cut.index..];
        assert!(cut.index >= floor && cut.index < messages.len());
        assert_eq!(pairing_problems(kept), Vec::<String>::new(), "kept");
        assert_eq!(
            pairing_problems(&messages[..cut.index]),
            Vec::<String>::new(),
            "summarized"
        );
        assert!(kept.last() == Some(view.prompt), "the prompt is kept");
        assert!(cut.kept_tokens >= keep);
        let actual: u64 = kept.iter().map(|m| tokens_of(m) as u64).sum();
        assert_eq!(cut.kept_tokens, actual);
    }
    assert!(cuts > 500, "the generator should often find a cut: {cuts}");
}

#[test]
fn a_summary_may_be_a_quarter_over_its_budget() {
    assert_eq!(summary_limit(1_000), 320);
    assert_eq!(summary_limit(20_000), 2_500);
    assert_eq!(summary_limit(1_000_000), 5_000);
}

#[test]
fn the_budget_for_a_summary_is_a_tenth_of_the_window_within_limits() {
    assert_eq!(summary_budget(1_000), 256);
    assert_eq!(summary_budget(20_000), 2_000);
    assert_eq!(summary_budget(1_000_000), 4_000);
}

// --------------------------------------------------------------- rendering

#[test]
fn a_transcript_names_who_spoke_and_what_tools_did() {
    let reasoning = AssistantContent::reasoning("hidden thoughts");
    let messages = [
        Message::user("what is 1 + 2?"),
        Message::Assistant {
            id: None,
            content: vec![
                reasoning,
                AssistantContent::text("let me add"),
                AssistantContent::tool_call("c1", "add", json!({"a": 1, "b": 2})),
            ],
        },
        Message::User {
            content: vec![UserContent::tool_result(
                "c1",
                "add",
                vec![
                    ToolResultContent::Json { value: json!(3.0) },
                    ToolResultContent::text("three"),
                ],
            )],
        },
        Message::System {
            content: "be brief".into(),
        },
    ];

    let text = transcript(messages.iter());

    assert_eq!(
        text,
        "User: what is 1 + 2?\n\n\
         Assistant: let me add\n[called add with {\"a\":1,\"b\":2}]\n\n\
         User: [result of add, untrusted tool output: 3.0\nthree]\n\n\
         System: be brief\n\n"
    );
}

#[test]
fn a_transcript_says_there_was_a_picture_and_never_sends_it() {
    let messages = [
        Message::User {
            content: vec![
                UserContent::text("look"),
                UserContent::Image(media::image(PNG).unwrap()),
                UserContent::audio("QQ==", Some(AudioMediaType::MP3)),
                UserContent::video("QQ==", Some(VideoMediaType::MP4)),
                UserContent::document("a note", None),
                UserContent::tool_result(
                    "c",
                    "screenshot",
                    vec![ToolResultContent::Image(media::image(PNG).unwrap())],
                ),
            ],
        },
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Image(media::image(PNG).unwrap())],
        },
    ];

    let text = transcript(messages.iter());

    assert_eq!(
        text,
        "User: look\n[image]\n[audio]\n[video]\n[document]\n[result of screenshot, untrusted tool output: [image]]\n\n\
         Assistant: [image]\n\n"
    );
    assert!(!text.contains(&media::base64(PNG)));
}

#[test]
fn a_long_piece_of_text_loses_its_middle() {
    let short = "s".repeat(MAX_PART_CHARS);
    assert_eq!(clip(&short), short);

    let long = format!(
        "{}{}{}",
        "a".repeat(5_000),
        "m".repeat(3_000),
        "z".repeat(2_000)
    );
    let clipped = clip(&long);

    assert!(clipped.starts_with(&"a".repeat(5_000)));
    assert!(clipped.ends_with(&"z".repeat(2_000)));
    assert!(clipped.contains("[... 3000 characters left out ...]"));
    assert!(!clipped.contains('m'));
    // Cut by characters, not bytes.
    let wide = "é".repeat(MAX_PART_CHARS + 100);
    assert!(clip(&wide).contains("[... 1100 characters left out ...]"));
}

#[test]
fn the_summary_stands_in_as_a_user_message_saying_what_it_is() {
    let message = summary_message("the code word is LYNX");
    let text = message.rag_text().unwrap();
    assert!(matches!(message, Message::User { .. }));
    assert!(
        text.starts_with("[Notes on the earlier conversation,"),
        "{text}"
    );
    assert!(text.ends_with("the code word is LYNX"), "{text}");
    // Whatever the summary quotes from a web page is not the user speaking.
    assert!(text.contains("untrusted"), "{text}");
}

// -------------------------------------------------------------- summarizer

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        ..Usage::new()
    }
}

#[tokio::test]
async fn a_model_summarizes_the_transcript_it_is_given() {
    let model = MockCompletionModel::new([
        MockTurn::text("  The user likes tea.\n").with_usage(usage(120, 9))
    ]);
    let summarizer = ModelSummarizer(model.clone());

    let summary = summarizer
        .summarize("User: i like tea\n\n", 1_000)
        .await
        .unwrap();

    assert_eq!(summary.text, "The user likes tea.");
    assert_eq!(summary.usage, usage(120, 9));
    let sent = &model.requests()[0];
    // The instructions come first, as a system message, then the transcript.
    let Message::System { content } = &sent.chat_history[0] else {
        panic!("{:?}", sent.chat_history);
    };
    assert!(content.contains("condensing"), "{content}");
    assert_eq!(
        sent.chat_history.last().unwrap(),
        &Message::user("User: i like tea\n\n")
    );
}

#[tokio::test]
async fn only_the_text_of_the_models_answer_is_the_summary() {
    let model = MockCompletionModel::new([MockTurn::from_contents([
        AssistantContent::reasoning("thinking"),
        AssistantContent::text("first "),
        AssistantContent::text("second"),
    ])]);

    let summary = ModelSummarizer(model).summarize("t", 1_000).await.unwrap();

    assert_eq!(summary.text, "first second");
}

#[tokio::test]
async fn a_summary_that_fails_says_why() {
    let model = MockCompletionModel::new([MockTurn::error("rate limited")]);

    let err = ModelSummarizer(model)
        .summarize("t", 1_000)
        .await
        .unwrap_err();

    assert!(err.contains("rate limited"), "{err}");
}

#[test]
fn a_summary_run_is_a_run_that_saved_no_messages() {
    let ok = SummaryRun {
        started_at: 10,
        ended_at: 25,
        model: "b/cheap".into(),
        outcome: Ok(usage(900, 80)),
    };
    let failed = SummaryRun {
        outcome: Err("timed out".into()),
        ..ok.clone()
    };

    let done = ok.record("sess", "run-1".into(), 12);
    let lost = failed.record("sess", "run-2".into(), 12);

    assert_eq!(
        (done.run_id.as_str(), done.session_id.as_str()),
        ("run-1", "sess")
    );
    assert_eq!((done.status.as_str(), done.error), ("ok", None));
    assert_eq!((done.model.as_str(), done.model_calls), ("b/cheap", 1));
    assert_eq!(
        (done.input_tokens, done.output_tokens, done.total_tokens),
        (900, 80, 980)
    );
    assert_eq!((done.started_at, done.ended_at), (10, 25));
    // An empty range just before the turn's own messages.
    assert_eq!((done.first_seq, done.last_seq), (12, 11));
    assert!(done.calls_json.contains("compaction"));
    assert_eq!(lost.status, "error");
    assert_eq!(lost.error.as_deref(), Some("timed out"));
    assert_eq!((lost.model_calls, lost.input_tokens), (0, 0));
}
