//! Each user's time zone: storage, the native tools, and the real agent loop.
mod common;
use athena::{agent, service::Service, store::Store, timezone::*};
use common::*;
use jiff::Timestamp;
use rig_agent::{
    agent::AgentBuilder,
    tool::{Tool, ToolContext, ToolExecutionError},
};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};

/// 2026-10-09 20:00 UTC: already the 10th in Kolkata, still the 9th in New York.
fn instant() -> Timestamp {
    "2026-10-09T20:00:00Z".parse().unwrap()
}

fn setup() -> (TempDb, Store, i64) {
    let tmp = TempDb::new();
    let store = tmp.open();
    let id = store.user("http", "a").unwrap().id();
    (tmp, store, id)
}

fn settings_rows(tmp: &TempDb) -> i64 {
    count(&tmp.raw(), "SELECT COUNT(*) FROM user_settings")
}

#[test]
fn a_user_without_a_setting_keeps_the_default_and_reads_write_nothing() {
    let (tmp, store, id) = setup();
    assert_eq!(name(&store.timezone(id).unwrap()), DEFAULT_TIMEZONE);
    assert_eq!(
        store.today(id, instant()).unwrap().to_string(),
        "2026-10-10"
    );
    assert_eq!(
        describe(&store.local_now(id, instant()).unwrap())["datetime"],
        "2026-10-10T01:30:00+05:30"
    );
    assert_eq!(settings_rows(&tmp), 0);

    // Setting the default explicitly changes nothing a reader sees.
    let other = store.user("http", "b").unwrap().id();
    let (previous, _) = store.set_timezone(other, DEFAULT_TIMEZONE).unwrap();
    assert_eq!(previous, DEFAULT_TIMEZONE);
    assert_eq!(
        describe(&store.local_now(other, instant()).unwrap()),
        describe(&store.local_now(id, instant()).unwrap())
    );
}

#[test]
fn setting_a_zone_moves_today_for_that_user_only_and_survives_a_restart() {
    let (tmp, store, id) = setup();
    let other = store.user("http", "b").unwrap().id();
    let (previous, zone) = store.set_timezone(id, "america/new_york").unwrap();
    assert_eq!(
        (previous.as_str(), name(&zone)),
        ("Asia/Kolkata", "America/New_York")
    );
    assert_eq!(
        store.today(id, instant()).unwrap().to_string(),
        "2026-10-09"
    );
    assert_eq!(
        store.today(other, instant()).unwrap().to_string(),
        "2026-10-10"
    );

    let (previous, _) = store.set_timezone(id, "Europe/London").unwrap();
    assert_eq!(previous, "America/New_York");
    assert_eq!(settings_rows(&tmp), 1);
    drop(store);
    let reopened = tmp.open();
    assert_eq!(name(&reopened.timezone(id).unwrap()), "Europe/London");
    assert_eq!(name(&reopened.timezone(other).unwrap()), DEFAULT_TIMEZONE);
}

#[test]
fn linked_identities_share_one_zone() {
    let (_tmp, store, _) = setup();
    let tg = store.user("telegram", "123").unwrap();
    let service = Service::new(store.clone(), "m", |_| {});
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(service.link_http_user(&tg, "my-api"))
        .unwrap();
    store.set_timezone(tg.id(), "Asia/Tokyo").unwrap();
    let http = store.user("http", "my-api").unwrap();
    assert_eq!(name(&store.timezone(http.id()).unwrap()), "Asia/Tokyo");
}

#[test]
fn invalid_names_and_unknown_users_write_nothing() {
    let (tmp, store, id) = setup();
    for bad in ["+05:30", "IST", "Mars/Base", ""] {
        let error = store.set_timezone(id, bad).unwrap_err().to_string();
        assert!(error.contains("not an IANA time zone name"), "{error}");
    }
    // The foreign key refuses a user who does not exist.
    assert!(store.set_timezone(9999, "UTC").is_err());
    assert_eq!(settings_rows(&tmp), 0);
}

#[test]
fn a_stored_zone_that_no_longer_resolves_is_an_error_not_a_guess() {
    let (tmp, store, id) = setup();
    tmp.raw()
        .execute(
            "INSERT INTO user_settings VALUES (?1, 'Gone/Away', 0)",
            [id],
        )
        .unwrap();
    let error = format!("{:#}", store.today(id, instant()).unwrap_err());
    assert!(
        error.contains("stored time zone Gone/Away no longer resolves"),
        "{error}"
    );
    // Setting a valid zone repairs it, and reports what it replaced.
    let (previous, _) = store.set_timezone(id, "UTC").unwrap();
    assert_eq!(previous, "Gone/Away");
    assert_eq!(
        store.today(id, instant()).unwrap().to_string(),
        "2026-10-09"
    );
}

async fn invoke<T: Tool<Error = ToolExecutionError, Output = Value>>(
    tool: T,
    session: &str,
    args: T::Args,
) -> Result<Value, ToolExecutionError> {
    let mut context = ToolContext::default();
    context.insert(athena::runner::Conversation(session.into()));
    tool.call(&mut context, args).await
}

fn set(zone: &str) -> SetTimezone {
    SetTimezone {
        timezone: zone.into(),
    }
}

#[tokio::test]
async fn tools_need_the_hosts_session_and_report_errors() {
    let (tmp, store, _) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("http", "a").await.unwrap();
    let s = session(&service, &user, "time").await;

    let no_context = Now(store.clone())
        .call(&mut ToolContext::default(), NoArgs {})
        .await;
    assert!(no_context.is_err());
    let unknown = invoke(Now(store.clone()), "missing", NoArgs {}).await;
    assert!(unknown.unwrap_err().to_string().contains("unknown session"));
    let unknown = invoke(TimezoneSet(store.clone()), "missing", set("UTC")).await;
    assert!(unknown.unwrap_err().to_string().contains("unknown session"));
    let invalid = invoke(TimezoneSet(store.clone()), &s.id, set("+05:30")).await;
    assert!(invalid.unwrap_err().to_string().contains("IANA"));
    assert_eq!(settings_rows(&tmp), 0);

    let now = invoke(Now(store.clone()), &s.id, NoArgs {}).await.unwrap();
    assert_eq!(now["timezone"], DEFAULT_TIMEZONE);
    assert_eq!(now["utc_offset"], "+05:30");
    assert_consistent(&now);

    let set_result = invoke(TimezoneSet(store.clone()), &s.id, set("Europe/London"))
        .await
        .unwrap();
    assert_eq!(set_result["previous_timezone"], DEFAULT_TIMEZONE);
    assert_eq!(set_result["now"]["timezone"], "Europe/London");
    assert_consistent(&set_result["now"]);

    tmp.raw()
        .execute(
            "UPDATE user_settings SET timezone = 'Gone/Away' WHERE user_id = ?1",
            [user.id()],
        )
        .unwrap();
    let broken = invoke(Now(store.clone()), &s.id, NoArgs {}).await;
    assert!(
        broken
            .unwrap_err()
            .to_string()
            .contains("no longer resolves")
    );
}

/// The fields of a `now` result taken at the real current time agree with
/// each other and with the clock.
fn assert_consistent(now: &Value) {
    let datetime: jiff::Timestamp = now["datetime"].as_str().unwrap().parse().unwrap();
    let zoned =
        datetime.to_zoned(jiff::tz::TimeZone::get(now["timezone"].as_str().unwrap()).unwrap());
    assert_eq!(describe(&zoned), *now);
    let drift = Timestamp::now().duration_since(datetime).as_secs();
    assert!((0..60).contains(&drift), "{now}");
}

#[tokio::test]
async fn the_agent_reads_and_sets_the_users_zone_through_the_real_loop() {
    let (_tmp, store, _) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("telegram", "777").await.unwrap();
    let s = session(&service, &user, "default").await;
    let model = MockCompletionModel::new([
        MockTurn::tool_call("a", "now", json!({})),
        MockTurn::text("it is today"),
        MockTurn::tool_call("b", "timezone_set", json!({"timezone": "Asia/Tokyo"})),
        MockTurn::text("set"),
        MockTurn::tool_call("c", "timezone_set", json!({"timezone": "GMT+9"})),
        MockTurn::text("asked for a city"),
    ]);
    let agent = agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    for prompt in ["what day is it?", "I live in Tokyo", "use GMT+9"] {
        service.send(&agent, &user, &s.id, prompt).await.unwrap();
    }

    let requests = model.requests();
    let offered: Vec<_> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert!(offered.contains(&"now") && offered.contains(&"timezone_set"));
    let result = |i: usize| {
        let message = serde_json::to_value(requests[i].chat_history.last().unwrap()).unwrap();
        message["content"][0]["content"][0].clone()
    };
    assert_eq!(result(1)["value"]["timezone"], DEFAULT_TIMEZONE);
    assert_consistent(&result(1)["value"]);
    assert_eq!(result(3)["value"]["previous_timezone"], DEFAULT_TIMEZONE);
    assert_eq!(result(3)["value"]["now"]["timezone"], "Asia/Tokyo");
    assert!(
        result(5).to_string().contains("not an IANA time zone name"),
        "{}",
        result(5)
    );
    // The refused name left the zone the agent set before it.
    assert_eq!(name(&store.timezone(user.id()).unwrap()), "Asia/Tokyo");
}
