//! Calorie storage contracts and the real native-tool loop.
mod common;
use athena::{agent, calories::*, service::Service, store::Store};
use common::*;
use rig_agent::{
    agent::AgentBuilder,
    tool::{Tool, ToolContext},
};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};

fn meal() -> Meal {
    Meal {
        description: "rice and eggs".into(),
        consumed_date: "2026-10-05".into(),
        calories: Some(400.0),
        protein_g: Some(20.0),
        carbs_g: None,
        fat_g: None,
        meal_type: Some("lunch".into()),
        source: Source::User,
    }
}
fn log(key: &str) -> Log {
    Log {
        request_key: key.into(),
        meal: meal(),
    }
}
fn range() -> Range {
    Range {
        start_date: "2026-10-01".into(),
        end_date: "2026-10-31".into(),
    }
}
fn history(limit: Option<u32>, before_id: Option<i64>) -> History {
    History {
        start_date: range().start_date,
        end_date: range().end_date,
        limit,
        before_id,
    }
}
fn setup() -> (TempDb, Store, i64) {
    let tmp = TempDb::new();
    let store = tmp.open();
    let id = store.user("http", "a").unwrap().id();
    (tmp, store, id)
}

#[test]
fn occurrences_retries_corrections_deletion_and_reopening() {
    let (tmp, store, id) = setup();
    let first = store.calorie_log(id, log("one")).unwrap();
    let entry_id = first["entry"]["id"].as_i64().unwrap();
    assert_eq!(first["already_existed"], false);
    assert_eq!(
        store.calorie_log(id, log("one")).unwrap()["already_existed"],
        true
    );
    let mut conflict = log("one");
    conflict.meal.calories = Some(500.0);
    assert!(store.calorie_log(id, conflict).is_err());
    store.calorie_log(id, log("two")).unwrap();
    let mut corrected = meal();
    corrected.calories = Some(450.0);
    corrected.source = Source::Estimated;
    let correction = Update {
        id: entry_id,
        expected_version: 1,
        meal: corrected.clone(),
    };
    assert_eq!(
        store.calorie_update(id, correction.clone()).unwrap()["version"],
        2
    );
    assert!(store.calorie_update(id, correction).is_err());
    let retry = store.calorie_log(id, log("one")).unwrap();
    assert_eq!(retry["entry"]["calories"], 450.0);
    assert_eq!(retry["entry"]["version"], 2);
    assert!(
        store
            .calorie_remove(
                id,
                Remove {
                    id: entry_id,
                    expected_version: 1
                }
            )
            .is_err()
    );
    assert_eq!(
        store
            .calorie_remove(
                id,
                Remove {
                    id: entry_id,
                    expected_version: 2
                }
            )
            .unwrap()["version"],
        3
    );
    assert!(
        store
            .calorie_remove(
                id,
                Remove {
                    id: entry_id,
                    expected_version: 3
                }
            )
            .is_err()
    );
    assert!(
        store
            .calorie_update(
                id,
                Update {
                    id: entry_id,
                    expected_version: 3,
                    meal: meal()
                }
            )
            .is_err()
    );
    assert!(store.calorie_log(id, log("one")).unwrap()["entry"]["deleted_at"].is_number());
    drop(store);
    let reopened = tmp.open();
    assert_eq!(
        reopened.calorie_history(id, history(None, None)).unwrap()["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened.calorie_summary(id, range()).unwrap()["totals"]["calories"],
        400.0
    );
    assert!(reopened.calorie_log(id, log("one")).unwrap()["entry"]["deleted_at"].is_number());
}
#[test]
fn isolation_nullable_totals_inclusive_dates_and_pagination() {
    let (_tmp, store, id) = setup();
    let other = store.user("http", "b").unwrap().id();
    let empty = store.calorie_summary(id, range()).unwrap();
    assert_eq!(empty["entry_count"], 0);
    assert!(empty["totals"]["calories"].is_null());
    let mut unknown = log("unknown");
    unknown.meal.calories = None;
    unknown.meal.protein_g = None;
    unknown.meal.meal_type = None;
    store.calorie_log(id, unknown).unwrap();
    let missing = store.calorie_summary(id, range()).unwrap();
    assert!(missing["totals"]["calories"].is_null());
    assert_eq!(missing["missing"]["calories"], 1);
    let first = store.calorie_log(id, log("known")).unwrap()["entry"]["id"]
        .as_i64()
        .unwrap();
    let mut outside = log("outside");
    outside.meal.consumed_date = "2026-11-01".into();
    store.calorie_log(id, outside).unwrap();
    let page = store.calorie_history(id, history(Some(1), None)).unwrap();
    assert_eq!(page["entries"][0]["id"], first);
    assert_eq!(page["next_before_id"], first);
    let last = store
        .calorie_history(id, history(Some(1), Some(first)))
        .unwrap();
    assert!(last["next_before_id"].is_null());
    assert_eq!(last["entries"][0]["description"], "rice and eggs");
    let sum = store
        .calorie_summary(
            id,
            Range {
                start_date: "2026-10-05".into(),
                end_date: "2026-10-05".into(),
            },
        )
        .unwrap();
    assert_eq!(sum["entry_count"], 2);
    assert_eq!(sum["totals"]["calories"], 400.0);
    assert_eq!(sum["missing"]["calories"], 1);
    assert_eq!(sum["missing"]["fat_g"], 2);
    assert_eq!(
        store.calorie_summary(other, range()).unwrap()["entry_count"],
        0
    );
    assert_eq!(
        store.calorie_history(other, history(None, None)).unwrap()["entries"],
        json!([])
    );
    assert!(
        store
            .calorie_update(
                other,
                Update {
                    id: first,
                    expected_version: 1,
                    meal: meal()
                }
            )
            .is_err()
    );
    assert!(
        store
            .calorie_remove(
                other,
                Remove {
                    id: first,
                    expected_version: 1
                }
            )
            .is_err()
    );
    // Retry keys are scoped to each owner.
    assert_eq!(
        store.calorie_log(other, log("known")).unwrap()["already_existed"],
        false
    );
}
#[test]
fn invalid_input_never_writes_or_panics() {
    let (_tmp, store, id) = setup();
    for d in [
        "",
        "2026-2-01",
        "2026-02-29",
        "1900-02-29",
        "2026-13-01",
        "0000-01-01",
        "2026-01-00",
        "2026-04-31",
        "éé-10-01",
        "abcd-01-01",
    ] {
        let mut l = log("bad");
        l.meal.consumed_date = d.into();
        assert!(store.calorie_log(id, l).is_err(), "{d}");
    }
    for d in [
        "2000-02-29",
        "2024-02-29",
        "2026-02-28",
        "2026-04-30",
        "2026-12-31",
    ] {
        let mut l = log(d);
        l.meal.consumed_date = d.into();
        store.calorie_log(id, l).unwrap();
    }
    for n in [-1.0, f64::NAN, f64::INFINITY, 1_000_001.0] {
        for field in 0..4 {
            let mut l = log("bad");
            match field {
                0 => l.meal.calories = Some(n),
                1 => l.meal.protein_g = Some(n),
                2 => l.meal.carbs_g = Some(n),
                _ => l.meal.fat_g = Some(n),
            };
            assert!(store.calorie_log(id, l).is_err());
        }
    }
    for s in ["".into(), " \t".into(), "a\nb".into(), "a".repeat(513)] {
        let mut l = log("bad");
        l.meal.description = s;
        assert!(store.calorie_log(id, l).is_err());
    }
    for s in ["".into(), "a".repeat(65)] {
        let mut l = log("bad");
        l.meal.meal_type = Some(s);
        assert!(store.calorie_log(id, l).is_err());
    }
    for s in ["".into(), "a".repeat(129)] {
        let mut l = log("bad");
        l.request_key = s;
        assert!(store.calorie_log(id, l).is_err());
    }
    for limit in [0, 51] {
        assert!(
            store
                .calorie_history(id, history(Some(limit), None))
                .is_err()
        );
    }
    assert!(store.calorie_history(id, history(None, Some(0))).is_err());
    for r in [
        Range {
            start_date: "bad".into(),
            end_date: "bad".into(),
        },
        Range {
            start_date: "2026-01-01".into(),
            end_date: "bad".into(),
        },
        Range {
            start_date: "2026-12-01".into(),
            end_date: "2026-01-01".into(),
        },
    ] {
        assert!(store.calorie_summary(id, r.clone()).is_err());
        assert!(
            store
                .calorie_history(
                    id,
                    History {
                        start_date: r.start_date,
                        end_date: r.end_date,
                        limit: None,
                        before_id: None
                    }
                )
                .is_err()
        );
    }
    for (entry, version) in [(0, 1), (1, 0)] {
        assert!(
            store
                .calorie_update(
                    id,
                    Update {
                        id: entry,
                        expected_version: version,
                        meal: meal()
                    }
                )
                .is_err()
        );
        assert!(
            store
                .calorie_remove(
                    id,
                    Remove {
                        id: entry,
                        expected_version: version
                    }
                )
                .is_err()
        );
    }
    let mut invalid = meal();
    invalid.description = "".into();
    assert!(
        store
            .calorie_update(
                id,
                Update {
                    id: 1,
                    expected_version: 1,
                    meal: invalid
                }
            )
            .is_err()
    );
    assert!(store.calorie_log(99999, log("orphan")).is_err());
    assert!(
        serde_json::from_value::<Log>(json!({"request_key":"x","meal":meal(),"user_id":id}))
            .is_err()
    );
}

async fn invoke<T: Tool<Error = rig_agent::tool::ToolExecutionError, Output = Value>>(
    tool: T,
    session: &str,
    args: T::Args,
) -> Result<Value, rig_agent::tool::ToolExecutionError> {
    let mut context = ToolContext::default();
    context.insert(athena::runner::Conversation(session.into()));
    tool.call(&mut context, args).await
}
#[tokio::test]
async fn tools_require_host_context_and_report_storage_errors() {
    let (_tmp, store, id) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("http", "a").await.unwrap();
    let session = session(&service, &user, "food").await;
    assert!(
        CalorieLog(store.clone())
            .call(&mut ToolContext::default(), log("x"))
            .await
            .is_err()
    );
    assert!(
        invoke(CalorieLog(store.clone()), "missing", log("x"))
            .await
            .is_err()
    );
    assert!(
        invoke(CalorieLog(store.clone()), &session.id, log("x"))
            .await
            .is_ok()
    );
    assert!(
        invoke(
            CalorieHistory(store.clone()),
            &session.id,
            history(None, None)
        )
        .await
        .is_ok()
    );
    assert!(
        invoke(CalorieSummary(store.clone()), &session.id, range())
            .await
            .is_ok()
    );
    let entry = store.calorie_history(id, history(None, None)).unwrap()["entries"][0]["id"]
        .as_i64()
        .unwrap();
    assert!(
        invoke(
            CalorieUpdate(store.clone()),
            &session.id,
            Update {
                id: entry,
                expected_version: 1,
                meal: meal()
            }
        )
        .await
        .is_ok()
    );
    assert!(
        invoke(
            CalorieRemove(store.clone()),
            &session.id,
            Remove {
                id: entry,
                expected_version: 2
            }
        )
        .await
        .is_ok()
    );
    assert!(
        invoke(
            CalorieUpdate(store.clone()),
            &session.id,
            Update {
                id: entry,
                expected_version: 1,
                meal: meal()
            }
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            CalorieRemove(store.clone()),
            &session.id,
            Remove {
                id: entry,
                expected_version: 2
            }
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            CalorieHistory(store.clone()),
            "missing",
            history(None, None)
        )
        .await
        .is_err()
    );
    assert!(
        invoke(CalorieSummary(store.clone()), "missing", range())
            .await
            .is_err()
    );
    assert!(
        invoke(
            CalorieUpdate(store.clone()),
            "missing",
            Update {
                id: entry,
                expected_version: 1,
                meal: meal()
            }
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            CalorieRemove(store.clone()),
            "missing",
            Remove {
                id: entry,
                expected_version: 1
            }
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn linked_channels_share_food_through_the_real_agent_loop() {
    let tmp = TempDb::new();
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    let tg = service.user("telegram", "123").await.unwrap();
    service.link_http_user(&tg, "my-api").await.unwrap();
    let http = service.user("http", "my-api").await.unwrap();
    let a = session(&service, &tg, "telegram").await;
    let b = session(&service, &http, "api").await;
    let model = MockCompletionModel::new([
        MockTurn::tool_call(
            "a",
            "calorie_log",
            json!({"request_key":"occurrence-1","meal":meal()}),
        ),
        MockTurn::text("saved"),
        MockTurn::tool_call(
            "b",
            "calorie_log",
            json!({"request_key":"occurrence-1","meal":meal()}),
        ),
        MockTurn::text("retried"),
        MockTurn::tool_call(
            "c",
            "calorie_log",
            json!({"request_key":"occurrence-2","meal":meal()}),
        ),
        MockTurn::text("saved again"),
        MockTurn::tool_call(
            "d",
            "calorie_summary",
            json!({"start_date":"2026-10-05","end_date":"2026-10-05"}),
        ),
        MockTurn::text("800 kcal"),
    ]);
    let agent = agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    for (u, s, p) in [
        (&tg, &a, "log"),
        (&http, &b, "retry"),
        (&http, &b, "another meal"),
        (&tg, &a, "summary"),
    ] {
        service.send(&agent, u, &s.id, p).await.unwrap();
    }
    assert_eq!(
        store.calorie_summary(tg.id(), range()).unwrap()["totals"]["calories"],
        800.0
    );
    let requests = model.requests();
    let result = serde_json::to_value(requests[7].chat_history.last().unwrap()).unwrap();
    assert_eq!(result["content"][0]["content"][0]["type"], "json");
    let result = &result["content"][0]["content"][0]["value"];
    assert_eq!(result["entry_count"], 2);
    assert_eq!(result["totals"]["calories"], 800.0);
    assert_eq!(result["missing"]["carbs_g"], 2);
    // Drive unrelated ownership through native tool context, not only Store methods.
    let other = service.user("http", "unrelated").await.unwrap();
    let c = session(&service, &other, "food").await;
    let entry_id = store.calorie_history(tg.id(), history(None, None)).unwrap()["entries"][0]["id"]
        .as_i64()
        .unwrap();
    let outsider = MockCompletionModel::new([
        MockTurn::tool_call(
            "e",
            "calorie_history",
            json!({"start_date":"2026-10-01","end_date":"2026-10-31"}),
        ),
        MockTurn::text("read"),
        MockTurn::tool_call(
            "f",
            "calorie_summary",
            json!({"start_date":"2026-10-01","end_date":"2026-10-31"}),
        ),
        MockTurn::text("total"),
        MockTurn::tool_call(
            "g",
            "calorie_update",
            json!({"id":entry_id,"expected_version":1,"meal":meal()}),
        ),
        MockTurn::text("attempted"),
        MockTurn::tool_call(
            "h",
            "calorie_remove",
            json!({"id":entry_id,"expected_version":1}),
        ),
        MockTurn::text("attempted"),
    ]);
    let outsider_agent = agent::configure_persistent(
        AgentBuilder::new(outsider.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    for prompt in ["history", "summary", "correct", "remove"] {
        service
            .send(&outsider_agent, &other, &c.id, prompt)
            .await
            .unwrap();
    }
    let outside_requests = outsider.requests();
    let results: Vec<_> = [1, 3, 5, 7]
        .into_iter()
        .map(|i| serde_json::to_value(outside_requests[i].chat_history.last().unwrap()).unwrap())
        .collect();
    let history_result = &results[0]["content"][0]["content"][0]["value"];
    assert_eq!(history_result["entries"], json!([]));
    let summary_result = &results[1]["content"][0]["content"][0]["value"];
    assert_eq!(summary_result["entry_count"], 0);
    for result in &results[2..] {
        assert!(
            result
                .to_string()
                .contains("entry missing, deleted, or version conflicts"),
            "{result}"
        );
    }
    assert_eq!(
        store.calorie_history(tg.id(), history(None, None)).unwrap()["entries"][0]["version"],
        1
    );
    assert_eq!(
        store.calorie_summary(tg.id(), range()).unwrap()["entry_count"],
        2
    );
    drop(outsider_agent);
    drop((agent, service, store));
    assert_eq!(
        tmp.open().calorie_summary(http.id(), range()).unwrap()["entry_count"],
        2
    );
}

#[test]
fn retries_normalize_zero_and_concurrent_writers_apply_one_correction() {
    let (tmp, store, id) = setup();
    let mut zero = log("zero");
    zero.meal.calories = Some(-0.0);
    zero.meal.protein_g = Some(0.0);
    zero.meal.carbs_g = Some(0.0);
    zero.meal.fat_g = Some(0.0);
    let first = store.calorie_log(id, zero.clone()).unwrap();
    zero.meal.calories = Some(0.0);
    assert_eq!(
        store.calorie_log(id, zero).unwrap()["already_existed"],
        true
    );
    let entry = first["entry"]["id"].as_i64().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let store = Store::open(tmp.path()).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .calorie_update(
                        id,
                        Update {
                            id: entry,
                            expected_version: 1,
                            meal: meal(),
                        },
                    )
                    .is_ok()
            })
        })
        .collect();
    let successes = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|success| *success)
        .count();
    assert_eq!(successes, 1);
    assert_eq!(
        store.calorie_history(id, history(None, None)).unwrap()["entries"][0]["version"],
        2
    );
}

#[test]
fn failed_sql_writes_are_reported_without_partial_records() {
    let (tmp, store, id) = setup();
    // Simulate a SQLite write failure, rather than assuming failed writes are safe.
    tmp.raw().execute_batch("CREATE TRIGGER reject_food BEFORE INSERT ON calorie_logs BEGIN SELECT RAISE(ABORT,'write unavailable'); END;").unwrap();
    assert!(store.calorie_log(id, log("retryable")).is_err());
    assert_eq!(
        store.calorie_summary(id, range()).unwrap()["entry_count"],
        0
    );
    tmp.raw().execute_batch("DROP TRIGGER reject_food").unwrap();
    assert_eq!(
        store.calorie_log(id, log("retryable")).unwrap()["already_existed"],
        false
    );
}
