//! The daily brief's tools, through the real agent loop in front of a scripted
//! model. Each step is one turn: the model calls a tool, then answers. The
//! tool's result is read back from the request the model was sent next.
mod common;

use athena::agent;
use athena::service::{Service, Session, User};
use athena::store::Store;
use common::*;
use jiff::{ToSpan, civil::Date};
use rig_agent::agent::{Agent, AgentBuilder};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};

fn agent_for(service: &Service, store: &Store, model: &MockCompletionModel) -> Agent {
    agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    )
}

/// Runs each `(tool, arguments)` step as its own turn in `session`, and
/// returns what each tool call sent back to the model, in order.
async fn run(
    service: &Service,
    store: &Store,
    user: &User,
    session: &Session,
    steps: &[(&str, Value)],
) -> Vec<Value> {
    let turns = steps.iter().enumerate().flat_map(|(i, (tool, args))| {
        [
            MockTurn::tool_call(format!("call_{i}"), *tool, args.clone()),
            MockTurn::text(format!("done {i}")),
        ]
    });
    let model = MockCompletionModel::new(turns);
    let agent = agent_for(service, store, &model);
    for _ in steps {
        service
            .send(&agent, user, &session.id, "please")
            .await
            .unwrap();
    }
    let requests = model.requests();
    (0..steps.len())
        .map(|i| serde_json::to_value(requests[2 * i + 1].chat_history.last().unwrap()).unwrap())
        .collect()
}

/// The JSON a successful tool returned, from the model's view of it.
fn value(result: &Value) -> Value {
    result["content"][0]["content"][0]["value"].clone()
}

fn setup() -> (TempDb, Store, Service) {
    let tmp = TempDb::new();
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    (tmp, store, service)
}

/// The date part of a `next_brief` such as `2026-10-11T06:30+05:30`.
fn date_of(next: &str) -> Date {
    next[..10].parse().unwrap()
}

#[tokio::test]
async fn set_turns_the_brief_on_in_the_users_zone() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_set", json!({"time": "00:00"}))],
    )
    .await;
    let set = value(&results[0]);
    assert_eq!(set["status"], "on");
    assert_eq!(set["time"], "00:00");
    assert_eq!(set["timezone"], "Asia/Kolkata");
    // 00:00 has already passed today, so today counts as sent and the first
    // brief is tomorrow's, at 00:00 on the user's clock.
    let next = set["next_brief"].as_str().unwrap();
    assert!(next.ends_with("T00:00+05:30"), "{next}");
    let brief = store.brief(user.id()).unwrap().unwrap();
    assert_eq!(brief.local_time, "00:00");
    assert!(brief.enabled);
    let sent_today = brief.last_sent_date.unwrap();
    let day: Date = sent_today.parse().unwrap();
    assert_eq!(date_of(next), day.saturating_add(1.day()));
}

#[tokio::test]
async fn a_time_that_is_not_hh_mm_is_refused_and_stores_nothing() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_set", json!({"time": "6:30"}))],
    )
    .await;
    assert!(results[0].to_string().contains("HH:MM"), "{}", results[0]);
    assert!(store.brief(user.id()).unwrap().is_none());
}

#[tokio::test]
async fn a_user_without_a_telegram_chat_cannot_switch_the_brief_on() {
    let (_tmp, store, service) = setup();
    let cli = cli_user(&service).await;
    let s = session(&service, &cli, "default").await;

    let results = run(
        &service,
        &store,
        &cli,
        &s,
        &[("daily_brief_set", json!({"time": "00:00"}))],
    )
    .await;
    assert!(
        results[0].to_string().contains("no Telegram chat"),
        "{}",
        results[0]
    );
    assert!(store.brief(cli.id()).unwrap().is_none());
}

#[tokio::test]
async fn unknown_fields_are_refused_and_stores_nothing() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_set", json!({"time": "00:00", "when": "now"}))],
    )
    .await;
    assert!(results[0].to_string().contains("when"), "{}", results[0]);
    assert!(store.brief(user.id()).unwrap().is_none());
}

#[tokio::test]
async fn changing_the_time_after_todays_send_does_not_send_again() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[
            ("daily_brief_set", json!({"time": "00:00"})),
            ("daily_brief_set", json!({"time": "06:30"})),
        ],
    )
    .await;
    let first = store.brief(user.id()).unwrap().unwrap();
    let day = first.last_sent_date.clone().unwrap();
    // The second change is after today's claim: the day stays claimed, so the
    // next brief is tomorrow at the new time.
    let second = value(&results[1]);
    assert_eq!(second["time"], "06:30");
    let next = second["next_brief"].as_str().unwrap();
    assert!(next.ends_with("T06:30+05:30"), "{next}");
    assert_eq!(date_of(next).to_string(), {
        let day: Date = day.parse().unwrap();
        day.saturating_add(1.day()).to_string()
    });
    let after = store.brief(user.id()).unwrap().unwrap();
    assert_eq!(after.local_time, "06:30");
    assert_eq!(after.last_sent_date, Some(day));
}

#[tokio::test]
async fn off_says_whether_the_brief_was_on() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[
            ("daily_brief_set", json!({"time": "06:30"})),
            ("daily_brief_off", json!({})),
            ("daily_brief_off", json!({})),
        ],
    )
    .await;
    assert_eq!(value(&results[1]), json!({"status": "off", "was_on": true}));
    assert_eq!(
        value(&results[2]),
        json!({"status": "off", "was_on": false})
    );
    assert!(!store.brief(user.id()).unwrap().unwrap().enabled);
}

#[tokio::test]
async fn status_says_off_on_or_off_with_the_reason_telegram_gave() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_status", json!({}))],
    )
    .await;
    let never = value(&results[0]);
    assert_eq!(never["status"], "off");
    assert_eq!(never["hint"], "daily_brief_set turns it on");

    run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_set", json!({"time": "00:00"}))],
    )
    .await;
    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_status", json!({}))],
    )
    .await;
    let on = value(&results[0]);
    assert_eq!(on["status"], "on");
    assert_eq!(on["time"], "00:00");
    assert_eq!(on["timezone"], "Asia/Kolkata");
    assert!(on["last_brief_date"].is_string(), "{on}");
    assert!(on["last_error"].is_null(), "{on}");
    assert!(on["next_brief"].is_string(), "{on}");

    // Telegram refused for good: off, the reason kept, and no next time.
    let owner = user.id();
    let now = jiff::Timestamp::now();
    store
        .brief_disable(owner, "Telegram refused the brief: blocked", now)
        .unwrap();
    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[("daily_brief_status", json!({}))],
    )
    .await;
    let blocked = value(&results[0]);
    assert_eq!(blocked["status"], "off");
    assert!(
        blocked["last_error"].as_str().unwrap().contains("refused"),
        "{blocked}"
    );
    assert!(blocked.get("next_brief").is_none(), "{blocked}");
}

#[tokio::test]
async fn a_zone_that_no_longer_resolves_is_an_error_for_the_tools() {
    let (tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;
    store
        .brief_set(
            user.id(),
            "06:30",
            "2026-10-09",
            false,
            jiff::Timestamp::now(),
        )
        .unwrap();
    // A stored setting, then one that no longer names a zone.
    store.set_timezone(user.id(), "UTC").unwrap();
    tmp.raw()
        .execute(
            "UPDATE user_settings SET timezone = 'Gone/Away' WHERE user_id = ?1",
            [user.id()],
        )
        .unwrap();

    let results = run(
        &service,
        &store,
        &user,
        &s,
        &[
            ("daily_brief_set", json!({"time": "00:00"})),
            ("daily_brief_status", json!({})),
        ],
    )
    .await;
    for result in &results {
        assert!(
            result.to_string().contains("no longer resolves"),
            "{result}"
        );
    }
}

#[tokio::test]
async fn the_brief_tools_are_offered_and_the_preamble_names_them() {
    let (_tmp, store, service) = setup();
    let user = service.user("telegram", "42").await.unwrap();
    let s = session(&service, &user, "default").await;
    let model = MockCompletionModel::new([MockTurn::text("hi")]);
    let agent = agent_for(&service, &store, &model);
    service.send(&agent, &user, &s.id, "hello").await.unwrap();

    let request = &model.requests()[0];
    let offered: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    for name in ["daily_brief_set", "daily_brief_off", "daily_brief_status"] {
        assert!(offered.contains(&name), "{name} not offered");
    }
    let preamble = serde_json::to_string(&request.chat_history[0]).unwrap();
    assert!(preamble.contains("daily_brief_set"), "{preamble}");
}
