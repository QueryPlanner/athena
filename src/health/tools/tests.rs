use super::*;
use crate::agent;
use crate::health::sync::Linked;
use crate::health::testing::{FakeGoogle, SetClock, health};
use crate::health::{Callback, Secret, hash_state};
use crate::service::Service;
use jiff::Timestamp;
use rig_agent::agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};

const T0: &str = "2026-10-10T10:00:00Z";

struct Rig {
    store: Store,
    service: Service,
    fake: FakeGoogle,
    clock: Arc<SetClock>,
    health: Arc<Health>,
    owner: i64,
    user: crate::service::User,
    session: String,
}

async fn rig() -> Rig {
    let store = Store::open_in_memory().unwrap();
    let service = Service::new(store.clone(), "m", |_| {});
    let fake = FakeGoogle::start().await;
    let clock = SetClock::at(T0);
    let health = health(&store, &fake, clock.clone());
    let user = service.user("telegram", "7").await.unwrap();
    let session = service.open_session(&user, "default").await.unwrap().id;
    Rig {
        owner: user.id(),
        store,
        service,
        fake,
        clock,
        health,
        user,
        session,
    }
}

impl Rig {
    /// The model calls each tool in turn, through the real agent loop; this
    /// returns what each call gave back.
    async fn call(&self, health: Option<Arc<Health>>, calls: &[(&str, Value)]) -> Vec<Value> {
        let mut turns = Vec::new();
        for (i, (name, args)) in calls.iter().enumerate() {
            turns.push(MockTurn::tool_call(format!("c{i}"), *name, args.clone()));
            turns.push(MockTurn::text("ok"));
        }
        let model = MockCompletionModel::new(turns);
        let builder = AgentBuilder::new(model.clone()).memory(self.service.memory());
        let agent = match health {
            Some(health) => {
                agent::configure_persistent_with_health(builder, self.store.clone(), health)
            }
            None => agent::configure_persistent(
                builder,
                None,
                &crate::custom::Custom::default(),
                &crate::mcp::Mcp::none(),
                self.store.clone(),
                None,
            ),
        };
        for _ in calls {
            self.service
                .send(&agent, &self.user, &self.session, "go")
                .await
                .unwrap();
        }
        let requests = model.requests();
        (0..calls.len())
            .map(|i| {
                let last = requests[2 * i + 1].chat_history.last().unwrap();
                let message = serde_json::to_value(last).unwrap();
                let content = &message["content"][0]["content"][0];
                match content.get("value") {
                    Some(value) => value.clone(),
                    None => content["text"].clone(),
                }
            })
            .collect()
    }

    async fn configured(&self, calls: &[(&str, Value)]) -> Vec<Value> {
        self.call(Some(self.health.clone()), calls).await
    }

    async fn connect(&self) {
        let state = crate::health::new_state();
        self.store
            .health_state_create(
                self.owner,
                &hash_state(&state),
                self.health.now(),
                self.health.now() + crate::health::STATE_TTL,
            )
            .unwrap();
        let callback = Callback {
            code: Some(Secret::new("c")),
            state: Some(state),
            error: None,
        };
        assert_eq!(
            self.health.complete(self.owner, callback).await,
            Linked::Connected
        );
    }

    fn store_days(&self, days: &[(&str, Value)]) {
        let rows: Vec<(String, String)> = days
            .iter()
            .map(|(d, m)| (d.to_string(), m.to_string()))
            .collect();
        assert!(
            self.store
                .health_store_sync(
                    self.owner,
                    "2026-01-01",
                    "2027-01-01",
                    &rows,
                    T0.parse().unwrap()
                )
                .unwrap()
        );
    }
}

fn none() -> Value {
    json!({})
}

// ---- without Google Health ----

#[tokio::test]
async fn without_google_health_the_tools_exist_and_say_it_is_not_set_up() {
    let r = rig().await;
    let got = r
        .call(
            None,
            &[
                ("health_status", none()),
                ("health_summary", json!({"days": 3})),
                ("health_sync_now", none()),
            ],
        )
        .await;
    assert_eq!(
        got[0],
        json!({"configured": false, "status": "not_connected", "hint": NOT_CONFIGURED})
    );
    assert_eq!(
        got[1],
        json!({"status": "not_connected", "hint": NOT_CONFIGURED})
    );
    assert!(
        got[2].as_str().unwrap().contains("not set up"),
        "{}",
        got[2]
    );
}

// ---- configured, not connected ----

#[tokio::test]
async fn configured_but_not_connected_they_say_how_to_connect() {
    let r = rig().await;
    let got = r
        .configured(&[
            ("health_status", none()),
            ("health_summary", none()),
            ("health_sync_now", none()),
        ])
        .await;
    assert_eq!(
        got[0],
        json!({"configured": true, "status": "not_connected", "hint": CONNECT})
    );
    assert_eq!(got[1], json!({"status": "not_connected", "hint": CONNECT}));
    assert_eq!(got[2], json!({"status": "not_connected", "hint": CONNECT}));
    assert!(r.fake.seen().is_empty());
}

// ---- connected ----

fn day(steps: i64, sleep: i64, rest: i64) -> Value {
    json!({
        "steps": steps,
        "distance_m": 6543.2,
        "active_min": 40,
        "resting_hr": rest,
        "hr_zones": [{"type": "CARDIO", "min_bpm": 120, "max_bpm": 150}],
        "hr_zone_minutes": {"CARDIO": 12},
        "sleep": [
            {"minutes": sleep, "start": "a", "end": "b", "stages": [
                {"type": "DEEP", "minutes": 60}, {"type": "REM", "minutes": 80}]},
            {"minutes": 30, "start": "c", "end": "d", "stages": [{"type": "DEEP", "minutes": 10}]},
        ],
        "workouts": [{"type": "Run", "minutes": 30}],
        "weight_kg": 80.5,
    })
}

#[tokio::test]
async fn status_and_summary_report_what_was_synced_in_the_users_time() {
    let r = rig().await;
    r.connect().await;
    r.store_days(&[
        ("2026-10-07", day(6000, 400, 54)),
        ("2026-10-09", day(9000, 450, 52)),
        ("2026-10-10", json!({"steps": 1200})),
    ]);
    let got = r
        .configured(&[
            ("health_status", none()),
            ("health_summary", none()),
            ("health_summary", json!({"days": 1})),
        ])
        .await;
    // Asia/Kolkata by default: 15:30 at the connection.
    assert_eq!(got[0]["status"], "connected");
    assert_eq!(got[0]["configured"], true);
    assert_eq!(got[0]["connected_at"], "2026-10-10T15:30+05:30");
    assert_eq!(got[0]["last_synced_at"], "2026-10-10T15:30+05:30");
    assert_eq!(got[0]["last_sync_error"], Value::Null);
    assert!(got[0].get("hint").is_none());

    let week = &got[1];
    assert_eq!(week["status"], "connected");
    assert_eq!(week["today"], "2026-10-10");
    let dates: Vec<&str> = week["days"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["date"].as_str().unwrap())
        .collect();
    assert_eq!(dates, ["2026-10-07", "2026-10-09", "2026-10-10"]);
    let nine = &week["days"][1];
    assert_eq!(nine["steps"], 9000);
    assert_eq!(nine["distance_km"], 6.54);
    assert_eq!(nine["sleep_min"], 480);
    assert_eq!(nine["sleep_stage_min"], json!({"DEEP": 70, "REM": 80}));
    assert_eq!(nine["resting_hr"], 52);
    assert_eq!(nine["workouts"], json!([{"type": "Run", "minutes": 30}]));
    // The bulky and unneeded are left out.
    for gone in ["hr_zones", "sleep", "distance_m"] {
        assert!(nine.get(gone).is_none(), "{gone}");
    }
    assert_eq!(
        week["averages"]["steps"],
        json!({"average": 5400.0, "days_with_data": 3})
    );
    assert_eq!(
        week["averages"]["resting_hr"],
        json!({"average": 53.0, "days_with_data": 2})
    );
    assert_eq!(
        week["averages"]["sleep_min"],
        json!({"average": 455.0, "days_with_data": 2})
    );

    // One day is today only.
    assert_eq!(got[2]["days"].as_array().unwrap().len(), 1);
    assert_eq!(got[2]["days"][0]["date"], "2026-10-10");
}

#[tokio::test]
async fn the_summary_window_follows_the_users_today() {
    let r = rig().await;
    r.connect().await;
    r.store_days(&[
        ("2026-10-09", json!({"steps": 1})),
        ("2026-10-10", json!({"steps": 2})),
    ]);
    r.store.set_timezone(r.owner, "Pacific/Kiritimati").unwrap();
    // 10:00 UTC is already the 11th at UTC+14.
    let got = r
        .configured(&[("health_summary", json!({"days": 2}))])
        .await;
    assert_eq!(got[0]["today"], "2026-10-11");
    assert_eq!(got[0]["days"].as_array().unwrap().len(), 1);
    assert_eq!(got[0]["days"][0]["date"], "2026-10-10");
}

#[tokio::test]
async fn days_must_be_between_1_and_30() {
    let r = rig().await;
    r.connect().await;
    let got = r
        .configured(&[
            ("health_summary", json!({"days": 0})),
            ("health_summary", json!({"days": 31})),
            ("health_summary", json!({"days": 30})),
            ("health_summary", json!({"days": "x"})),
        ])
        .await;
    for refused in [&got[0], &got[1]] {
        assert!(
            refused.as_str().unwrap().contains("between 1 and 30"),
            "{refused}"
        );
    }
    assert_eq!(got[2]["status"], "connected");
    assert!(!got[3].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn a_revoked_connection_says_so_and_its_data_is_marked_old() {
    let r = rig().await;
    r.connect().await;
    r.store_days(&[("2026-10-09", json!({"steps": 1}))]);
    r.store
        .health_mark_revoked(r.owner, "x", T0.parse().unwrap())
        .unwrap();
    let got = r
        .configured(&[("health_status", none()), ("health_summary", none())])
        .await;
    assert_eq!(got[0]["status"], "revoked");
    assert!(got[0]["hint"].as_str().unwrap().contains("/connect_health"));
    assert_eq!(got[1]["status"], "revoked");
    assert!(got[1]["hint"].as_str().unwrap().contains("old data"));
    assert_eq!(got[1]["days"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn another_users_data_is_never_returned() {
    let r = rig().await;
    r.connect().await;
    r.store_days(&[("2026-10-09", json!({"steps": 777}))]);
    let other = r.service.user("telegram", "8").await.unwrap();
    let session = r.service.open_session(&other, "default").await.unwrap().id;
    let model = MockCompletionModel::new([
        MockTurn::tool_call("c", "health_summary", json!({})),
        MockTurn::text("ok"),
    ]);
    let agent = agent::configure_persistent_with_health(
        AgentBuilder::new(model.clone()).memory(r.service.memory()),
        r.store.clone(),
        r.health.clone(),
    );
    r.service
        .send(&agent, &other, &session, "go")
        .await
        .unwrap();
    let reply = serde_json::to_string(model.requests()[1].chat_history.last().unwrap()).unwrap();
    // The other user gets only "not connected": no day, no metric, no value.
    assert!(reply.contains("not_connected"), "{reply}");
    assert!(!reply.contains("777"), "{reply}");
    assert!(!reply.contains("steps"), "{reply}");
    assert!(!reply.contains("days"), "{reply}");
}

// ---- syncing now ----

#[tokio::test]
async fn sync_now_fetches_then_waits_an_hour() {
    let r = rig().await;
    r.connect().await;
    r.fake.answer(
        "steps",
        200,
        r#"{"dataPoints":[{"steps":{"interval":{"startTime":"2026-10-09T08:00:00Z"},"count":"42"}}]}"#,
    );
    r.fake.answer("sleep", 403, "{}");
    let first = r.configured(&[("health_sync_now", none())]).await;
    assert_eq!(
        first[0],
        json!({"status": "synced", "days_synced": 1, "unavailable_data_types": ["sleep"]})
    );
    r.clock.set("2026-10-10T10:20:30Z");
    let second = r
        .configured(&[("health_sync_now", none()), ("health_summary", none())])
        .await;
    // 39 minutes 30 seconds left, rounded up.
    assert_eq!(
        second[0],
        json!({"status": "cooldown", "retry_after_minutes": 40})
    );
    assert_eq!(second[1]["days"][0]["steps"], 42);
}

// ---- what a sync's outcome tells the model ----

#[test]
fn every_outcome_has_an_answer_for_the_model() {
    let now: Timestamp = T0.parse().unwrap();
    let d = |o: Outcome| describe(&o, now);
    assert_eq!(
        d(Outcome::Synced {
            days: 3,
            unavailable: vec![]
        }),
        json!({"status": "synced", "days_synced": 3, "unavailable_data_types": []})
    );
    assert_eq!(d(Outcome::Revoked)["status"], "revoked");
    assert!(
        d(Outcome::Revoked)["hint"]
            .as_str()
            .unwrap()
            .contains("reconnect")
    );
    assert!(
        d(Outcome::AlreadyRevoked)["hint"]
            .as_str()
            .unwrap()
            .contains("earlier")
    );
    assert_eq!(d(Outcome::NotConnected)["status"], "not_connected");
    assert_eq!(d(Outcome::Running), json!({"status": "already_running"}));
    assert_eq!(
        d(Outcome::Failed("Google answered HTTP 500".into())),
        json!({"status": "failed", "error": "Google answered HTTP 500"})
    );
    let in_a_minute = now + jiff::SignedDuration::from_secs(1);
    assert_eq!(d(Outcome::Cooldown(in_a_minute))["retry_after_minutes"], 1);
    // Already past: not negative.
    let past = now - jiff::SignedDuration::from_mins(5);
    assert_eq!(d(Outcome::Cooldown(past))["retry_after_minutes"], 0);
}

#[test]
fn a_summary_of_nothing_is_empty_and_unreadable_metrics_are_dropped() {
    assert_eq!(summarize(&[]), json!({"days": [], "averages": {}}));
    let rows = vec![("2026-10-09".to_string(), "not json".to_string())];
    assert_eq!(summarize(&rows)["days"], json!([{"date": "2026-10-09"}]));
}

#[test]
fn the_range_is_inclusive_of_today() {
    assert_eq!(
        range("2026-10-10".parse().unwrap(), 7),
        ("2026-10-04".to_string(), "2026-10-10".to_string())
    );
    assert_eq!(
        range("2026-10-10".parse().unwrap(), 1),
        ("2026-10-10".to_string(), "2026-10-10".to_string())
    );
}

// ---- metrics, averages, the size cap ----

#[test]
fn asking_for_metrics_keeps_them_with_the_date_the_drop_note_and_their_omitted_counts() {
    let rows = vec![(
        "2026-10-09".to_string(),
        json!({
            "steps": 5,
            "workouts": [{"type": "Run", "minutes": 30}],
            "workouts_omitted": 2,
            "dropped": ["hr_zones"],
            "weight_kg": 80.0,
        })
        .to_string(),
    )];
    let out = summarize_metrics(&rows, &["workouts".to_string()]);
    assert_eq!(
        out["days"],
        json!([{
            "date": "2026-10-09",
            "workouts": [{"type": "Run", "minutes": 30}],
            "workouts_omitted": 2,
            "dropped": ["hr_zones"],
        }])
    );
}

#[test]
fn the_metrics_enum_names_what_a_day_can_carry() {
    // Every key a day can carry is a name the model may ask for.
    assert!(METRICS.contains(&"sleep_min"));
    assert!(METRICS.contains(&"workouts"));
    assert!(
        !METRICS.contains(&"hr_zones"),
        "a dropped day key is not a metric"
    );
    assert_eq!(METRICS.len(), 40);
}

#[test]
fn averages_are_over_the_days_that_have_the_value_and_rounded_to_a_tenth() {
    let rows = vec![
        ("2026-10-08".to_string(), json!({"steps": 100}).to_string()),
        (
            "2026-10-09".to_string(),
            json!({"steps": 201, "heart_rate": {"min": 50, "avg": 70.25, "max": 150, "n": 9}})
                .to_string(),
        ),
        (
            "2026-10-10".to_string(),
            json!({"active_min": 30}).to_string(),
        ),
    ];
    let out = summarize(&rows);
    assert_eq!(
        out["averages"]["steps"],
        json!({"average": 150.5, "days_with_data": 2})
    );
    assert_eq!(
        out["averages"]["active_min"],
        json!({"average": 30.0, "days_with_data": 1})
    );
    assert_eq!(
        out["averages"]["heart_rate_avg"],
        json!({"average": 70.3, "days_with_data": 1})
    );
    assert!(out["averages"].get("resting_hr").is_none());
}

#[test]
fn distance_is_shown_in_kilometres_to_two_places() {
    let rows = vec![(
        "2026-10-09".to_string(),
        json!({"distance_m": 6543.2, "hr_zones": [{"type": "X"}]}).to_string(),
    )];
    let day = &summarize(&rows)["days"][0];
    assert_eq!(day["distance_km"], 6.54);
    assert!(day.get("distance_m").is_none());
    // The heart-rate zone limits are not sent to the model.
    assert!(day.get("hr_zones").is_none());
}

#[test]
fn over_the_size_cap_the_oldest_days_are_left_out_and_counted() {
    let big = json!({"note": "x".repeat(3500)}).to_string();
    let rows: Vec<(String, String)> = (1..=30)
        .map(|d| (format!("2026-10-{d:02}"), big.clone()))
        .collect();
    let out = summarize(&rows);
    let kept = out["days"].as_array().unwrap();
    let cut = out["truncated_days"].as_u64().unwrap() as usize;
    assert!(cut > 0);
    assert_eq!(kept.len() + cut, 30);
    assert!(out.to_string().len() <= SUMMARY_BYTES);
    // The newest days are the ones kept.
    assert_eq!(kept.last().unwrap()["date"], "2026-10-30");
    assert_eq!(kept[0]["date"], format!("2026-10-{:02}", cut + 1));
}

#[test]
fn thirty_maximal_days_stay_under_the_result_limit_after_summarising() {
    // A day is at most 4 KiB when stored (see normalize); 30 of them, asked
    // for in full, must still fit the tool result the agent may return.
    let full =
        json!({"note": "x".repeat(crate::health::normalize::MAX_DAY_BYTES - 40)}).to_string();
    let rows: Vec<(String, String)> = (1..=30)
        .map(|d| (format!("2026-10-{d:02}"), full.clone()))
        .collect();
    let out = summarize_metrics(&rows, &[]);
    assert!(out.to_string().len() <= crate::policy::MAX_RESULT_BYTES);
    assert!(out.to_string().len() <= SUMMARY_BYTES);
}

#[test]
fn summary_arguments_refuse_fields_they_do_not_name() {
    let ok: SummaryArgs = serde_json::from_value(json!({"days": 3, "metrics": ["steps"]})).unwrap();
    assert_eq!(ok.days, Some(3));
    assert!(serde_json::from_value::<SummaryArgs>(json!({"days": 3, "bogus": true})).is_err());
    assert!(serde_json::from_value::<SummaryArgs>(json!({"day": 3})).is_err());
}

#[tokio::test]
async fn an_unknown_metric_is_refused_and_the_error_lists_the_names() {
    let r = rig().await;
    r.connect().await;
    let got = r
        .configured(&[("health_summary", json!({"metrics": ["steps", "nope"]}))])
        .await;
    let text = got[0].as_str().unwrap();
    assert!(text.contains("unknown metric `nope`"), "{text}");
    assert!(text.contains("steps, floors"), "{text}");
}

#[test]
fn the_scopes_are_ten_distinct_read_only_google_health_scopes() {
    let mut unique: Vec<&str> = crate::health::SCOPES.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 10);
    assert!(crate::health::SCOPES.iter().all(|s| {
        s.starts_with("https://www.googleapis.com/auth/googlehealth.") && s.ends_with(".readonly")
    }));
}
