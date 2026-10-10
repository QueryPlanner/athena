//! Workout storage contracts, progressive-overload recall, and the real
//! native-tool loop.
mod common;
use athena::{agent, service::Service, store::Store, timezone::NoArgs, workouts::*};
use common::*;
use jiff::Timestamp;
use rig_agent::{
    agent::AgentBuilder,
    tool::{Tool, ToolContext, ToolExecutionError},
};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};

/// 2026-10-10 06:00 in Kolkata, the default zone.
fn instant() -> Timestamp {
    "2026-10-10T00:30:00Z".parse().unwrap()
}

fn workout(value: Value) -> Workout {
    serde_json::from_value(value).unwrap()
}

fn push(date: &str, bench_kg: f64, reps: u32) -> Workout {
    workout(json!({
        "session_date": date, "day_type": "push",
        "exercises": [
            {"name": "Bench Press", "sets": [
                {"reps": 10, "weight_kg": 40, "is_warmup": true},
                {"reps": reps, "weight_kg": bench_kg, "target_reps": 8},
                {"reps": reps - 1, "weight_kg": bench_kg, "target_reps": 8}]},
            {"name": "Dips", "sets": [{"reps": 12, "weight_kg": 0}]}
        ]
    }))
}

fn day(date: &str, day_type: &str) -> Workout {
    workout(json!({
        "session_date": date, "day_type": day_type,
        "exercises": [{"name": "Squat", "sets": [{"reps": 5, "weight_kg": 100}]}]
    }))
}

fn row(date: &str, time: &str) -> Workout {
    workout(json!({"session_date": date, "day_type": "vo2", "rowing": {"time": time}}))
}

fn log(store: &Store, owner: i64, key: &str, w: Workout) -> Value {
    store
        .workout_log(
            owner,
            Log {
                request_key: key.into(),
                workout: w,
            },
        )
        .unwrap()
}

fn id_of(v: &Value) -> i64 {
    v["session"]["id"].as_i64().unwrap()
}

fn last(day_type: DayType, before_date: Option<&str>) -> Last {
    Last {
        day_type,
        before_date: before_date.map(Into::into),
    }
}

fn progress(kind: Kind, exercise: Option<&str>, distance_m: Option<u32>) -> Progress {
    Progress {
        kind,
        exercise: exercise.map(Into::into),
        distance_m,
        limit: None,
    }
}

fn history(limit: Option<u32>, before_id: Option<i64>) -> History {
    History {
        start_date: "2026-10-01".into(),
        end_date: "2026-10-31".into(),
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
fn the_previous_push_day_is_recalled_with_every_set_and_a_suggestion() {
    let (tmp, store, id) = setup();
    log(&store, id, "p1", push("2026-10-01", 77.5, 8));
    log(&store, id, "l1", day("2026-10-03", "legs"));
    let latest = log(&store, id, "p2", push("2026-10-06", 80.0, 8));
    // Logged this morning: today's push is not "last time".
    log(&store, id, "p3", push("2026-10-10", 82.5, 6));

    let recalled = store
        .workout_last(id, last(DayType::Push, None), instant())
        .unwrap();
    assert_eq!(recalled["before_date"], "2026-10-10");
    let session = &recalled["session"];
    assert_eq!(session["id"], latest["session"]["id"]);
    assert_eq!(session["session_date"], "2026-10-06");
    let bench = &session["exercises"][0];
    assert_eq!(bench["name"], "Bench Press");
    assert_eq!(
        bench["sets"],
        json!([
            {"set": 1, "reps": 10, "weight_kg": 40.0, "target_reps": null, "is_warmup": true, "notes": null},
            {"set": 2, "reps": 8, "weight_kg": 80.0, "target_reps": 8, "is_warmup": false, "notes": null},
            {"set": 3, "reps": 7, "weight_kg": 80.0, "target_reps": 8, "is_warmup": false, "notes": null}
        ])
    );
    // The top set hit 8 of 8: 2.5 kg more. Bodyweight dips: one more rep.
    assert_eq!(bench["progression"]["hit_target"], true);
    assert_eq!(
        bench["progression"]["suggested"],
        json!({"weight_kg": 82.5, "reps": 8})
    );
    assert_eq!(
        session["exercises"][1]["progression"]["suggested"],
        json!({"weight_kg": 0.0, "reps": 13})
    );
    assert!(session["rowing"].is_null());

    // An explicit exclusive date reaches further back; nothing before is null.
    let earlier = store
        .workout_last(id, last(DayType::Push, Some("2026-10-06")), instant())
        .unwrap();
    assert_eq!(earlier["session"]["session_date"], "2026-10-01");
    assert!(
        store
            .workout_last(id, last(DayType::Push, Some("2026-10-01")), instant())
            .unwrap()["session"]
            .is_null()
    );
    assert!(
        store
            .workout_last(id, last(DayType::Pull, None), instant())
            .unwrap()["session"]
            .is_null()
    );
    assert!(
        store
            .workout_last(id, last(DayType::Push, Some("2026-13-01")), instant())
            .is_err()
    );

    // It survives a restart.
    drop(store);
    let reopened = tmp.open();
    assert_eq!(
        reopened
            .workout_last(id, last(DayType::Push, None), instant())
            .unwrap()["session"]["session_date"],
        "2026-10-06"
    );
}

#[test]
fn the_rotation_follows_lifting_days_before_today_and_vo2_is_weekly() {
    let (_tmp, store, id) = setup();
    let fresh = store.workout_next(id, instant()).unwrap();
    assert_eq!(
        fresh,
        json!({"today": "2026-10-10", "next_day_type": "push", "last_lifting": null,
               "vo2": {"due": true, "last_date": null}, "logged_today": [], "previous": null})
    );

    log(&store, id, "push", push("2026-10-05", 80.0, 8));
    log(&store, id, "pull", day("2026-10-07", "pull"));
    // Neither a VO2 day, an "other" day nor a future day moves the rotation.
    log(&store, id, "vo2", row("2026-10-04", "7:05.3"));
    log(&store, id, "other", day("2026-10-08", "other"));
    log(&store, id, "future", day("2026-10-12", "legs"));
    let legs = store.workout_next(id, instant()).unwrap();
    assert_eq!(legs["next_day_type"], "legs");
    assert_eq!(
        legs["last_lifting"],
        json!({"session_date": "2026-10-07", "day_type": "pull"})
    );
    assert!(legs["previous"].is_null());
    // 2026-10-04 is in the 7 days ending 2026-10-10; 2026-10-03 is not.
    assert_eq!(
        legs["vo2"],
        json!({"due": false, "last_date": "2026-10-04"})
    );
    let later: Timestamp = "2026-10-11T00:30:00Z".parse().unwrap();
    assert_eq!(store.workout_next(id, later).unwrap()["vo2"]["due"], true);

    // Today's legs session, once logged, is today's and not yesterday's.
    let today = log(&store, id, "legs", day("2026-10-10", "legs"));
    let after = store.workout_next(id, instant()).unwrap();
    assert_eq!(after["next_day_type"], "legs");
    assert_eq!(
        after["logged_today"],
        json!([{"id": id_of(&today), "day_type": "legs", "version": 1}])
    );
    // A full cycle wraps to push, with the last push and its suggestion.
    let tomorrow: Timestamp = "2026-10-11T00:30:00Z".parse().unwrap();
    let wrapped = store.workout_next(id, tomorrow).unwrap();
    assert_eq!(wrapped["next_day_type"], "push");
    assert_eq!(wrapped["previous"]["session_date"], "2026-10-05");
    assert_eq!(
        wrapped["previous"]["exercises"][0]["progression"]["suggested"]["weight_kg"],
        82.5
    );

    // Today is the user's own: in New York it is still the 9th.
    store.set_timezone(id, "America/New_York").unwrap();
    let ny = store.workout_next(id, instant()).unwrap();
    assert_eq!(ny["today"], "2026-10-09");
    assert_eq!(ny["logged_today"], json!([]));
}

#[test]
fn progress_tracks_the_top_set_estimated_max_and_rowing_splits() {
    let (_tmp, store, id) = setup();
    log(&store, id, "p1", push("2026-10-01", 75.0, 8));
    log(&store, id, "p2", push("2026-10-04", 80.0, 8));
    // A different spelling is the same lift; a warm-up-only block has no top set.
    log(
        &store,
        id,
        "p3",
        workout(
            json!({"session_date": "2026-10-07", "day_type": "push", "exercises": [
            {"name": "bench  press", "sets": [{"reps": 10, "weight_kg": 40, "is_warmup": true}]}]}),
        ),
    );
    let bench = store
        .exercise_progress(id, progress(Kind::Lift, Some("BENCH PRESS"), None))
        .unwrap();
    assert_eq!(bench["exercise"], "bench  press");
    let sessions = bench["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    assert!(sessions[0]["top_set"].is_null());
    assert_eq!(sessions[0]["working_sets"], 0);
    assert_eq!(
        sessions[1]["top_set"],
        json!({"weight_kg": 80.0, "reps": 8})
    );
    // Epley: 80 x (1 + 8/30) = 101.3; 75 x (1 + 8/30) = 95.0.
    assert_eq!(sessions[1]["estimated_1rm_kg"], 101.3);
    assert_eq!(sessions[2]["estimated_1rm_kg"], 95.0);
    assert_eq!(bench["best"]["session_date"], "2026-10-04");
    // The newest session in the window has no estimate, so no change.
    assert!(bench["estimated_1rm_change_kg"].is_null());
    let mut window = progress(Kind::Lift, Some("bench press"), None);
    window.limit = Some(1);
    let one = store.exercise_progress(id, window).unwrap();
    assert_eq!(one["sessions"].as_array().unwrap().len(), 1);
    // The best is all-time, not only within the window.
    assert_eq!(one["best"]["session_date"], "2026-10-04");

    log(&store, id, "p4", push("2026-10-09", 82.5, 8));
    let mut recent = progress(Kind::Lift, Some("Bench Press"), None);
    recent.limit = Some(3);
    let trend = store.exercise_progress(id, recent).unwrap();
    // 82.5 x (1 + 8/30) = 104.5, against 101.3 three sessions back.
    assert_eq!(trend["estimated_1rm_change_kg"], 3.2);

    let unknown = store
        .exercise_progress(id, progress(Kind::Lift, Some("Benchpress"), None))
        .unwrap();
    assert_eq!(unknown["sessions"], json!([]));
    assert!(unknown["best"].is_null());
    assert_eq!(unknown["known_exercises"], json!(["Bench Press", "Dips"]));

    log(&store, id, "r1", row("2026-10-02", "7:10.0"));
    log(&store, id, "r2", row("2026-10-09", "7:05.3"));
    log(
        &store,
        id,
        "r3",
        workout(json!({"session_date": "2026-10-08", "day_type": "vo2",
                       "rowing": {"distance_m": 5000, "time": "19:30"}})),
    );
    let rowing = store
        .exercise_progress(id, progress(Kind::Rowing, None, None))
        .unwrap();
    assert_eq!(rowing["distance_m"], 2000);
    assert_eq!(rowing["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(rowing["sessions"][0]["time"], "7:05.3");
    assert_eq!(rowing["sessions"][0]["split_500m"], "1:46.3");
    assert_eq!(rowing["best"]["time_ms"], 425_300);
    assert_eq!(rowing["time_change_s"], -4.7);
    let five_k = store
        .exercise_progress(id, progress(Kind::Rowing, None, Some(5000)))
        .unwrap();
    assert_eq!(five_k["sessions"][0]["split_500m"], "1:57.0");
    assert!(five_k["time_change_s"].is_null());
    let none = store
        .exercise_progress(id, progress(Kind::Rowing, None, Some(1000)))
        .unwrap();
    assert!(none["best"].is_null());

    for bad in [
        progress(Kind::Lift, None, None),
        progress(Kind::Lift, Some("Bench Press"), Some(2000)),
        progress(Kind::Lift, Some(""), None),
        progress(Kind::Rowing, Some("Row"), None),
        Progress {
            limit: Some(51),
            ..progress(Kind::Rowing, None, None)
        },
        Progress {
            limit: Some(0),
            ..progress(Kind::Rowing, None, None)
        },
    ] {
        assert!(store.exercise_progress(id, bad).is_err());
    }
}

#[test]
fn retries_corrections_deletion_and_conflicts() {
    let (_tmp, store, id) = setup();
    let first = log(&store, id, "one", push("2026-10-05", 80.0, 8));
    assert_eq!(first["already_existed"], false);
    let session = id_of(&first);
    // The same original, spelled differently, is a retry.
    let mut same = push("2026-10-05", 80.004, 8);
    same.exercises[0].sets[0].weight_kg = 40.0;
    assert_eq!(log(&store, id, "one", same)["already_existed"], true);
    let mut timed = row("2026-10-05", "7:05");
    let r = log(&store, id, "row", timed.clone());
    timed.rowing.as_mut().unwrap().time = "7:05.0".into();
    assert_eq!(
        log(&store, id, "row", timed)["session"]["id"],
        r["session"]["id"]
    );
    // A different workout under the same key conflicts and writes nothing.
    let changed = Log {
        request_key: "one".into(),
        workout: push("2026-10-05", 85.0, 8),
    };
    let e = store.workout_log(id, changed).unwrap_err().to_string();
    assert!(e.contains("conflicts with a different original"), "{e}");

    let correction = Update {
        id: session,
        expected_version: 1,
        workout: row("2026-10-06", "7:01.2"),
    };
    let updated = store.workout_update(id, correction.clone()).unwrap();
    assert_eq!(updated["version"], 2);
    assert_eq!(updated["day_type"], "vo2");
    assert_eq!(updated["exercises"], json!([]));
    assert_eq!(updated["rowing"]["time"], "7:01.2");
    let e = store
        .workout_update(id, correction)
        .unwrap_err()
        .to_string();
    assert!(e.contains("version conflicts"), "{e}");
    // A retry returns the corrected record.
    let retry = log(&store, id, "one", push("2026-10-05", 80.0, 8));
    assert_eq!(retry["session"]["version"], 2);

    // Removing the row piece drops it from progress; a retry stays deleted.
    let stale = Remove {
        id: session,
        expected_version: 1,
    };
    assert!(store.workout_remove(id, stale).is_err());
    let removed = store
        .workout_remove(
            id,
            Remove {
                id: session,
                expected_version: 2,
            },
        )
        .unwrap();
    assert_eq!(removed["version"], 3);
    assert!(removed["deleted_at"].is_number());
    let again = Remove {
        id: session,
        expected_version: 3,
    };
    assert!(store.workout_remove(id, again).is_err());
    let late = Update {
        id: session,
        expected_version: 3,
        workout: day("2026-10-06", "legs"),
    };
    assert!(store.workout_update(id, late).is_err());
    assert!(
        log(&store, id, "one", push("2026-10-05", 80.0, 8))["session"]["deleted_at"].is_number()
    );
    let rowing = store
        .exercise_progress(id, progress(Kind::Rowing, None, None))
        .unwrap();
    assert_eq!(rowing["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(rowing["sessions"][0]["time"], "7:05.0");
}

#[test]
fn another_user_never_reads_or_changes_a_session() {
    let (tmp, store, id) = setup();
    let other = store.user("http", "b").unwrap().id();
    let mine = log(&store, id, "k", push("2026-10-05", 80.0, 8));
    let session = id_of(&mine);
    let sets_before = count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sets");

    assert!(
        store
            .workout_last(other, last(DayType::Push, None), instant())
            .unwrap()["session"]
            .is_null()
    );
    assert!(store.workout_next(other, instant()).unwrap()["previous"].is_null());
    assert_eq!(
        store.workout_history(other, history(None, None)).unwrap()["sessions"],
        json!([])
    );
    let theirs = store
        .exercise_progress(other, progress(Kind::Lift, Some("Bench Press"), None))
        .unwrap();
    assert_eq!(theirs["known_exercises"], json!([]));
    let wipe = Update {
        id: session,
        expected_version: 1,
        workout: row("2026-10-05", "7:00"),
    };
    assert!(store.workout_update(other, wipe).is_err());
    let delete = Remove {
        id: session,
        expected_version: 1,
    };
    assert!(store.workout_remove(other, delete).is_err());
    // The refused update touched none of the owner's sets.
    assert_eq!(
        count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sets"),
        sets_before
    );
    assert_eq!(
        store
            .workout_last(id, last(DayType::Push, None), instant())
            .unwrap()["session"]["version"],
        1
    );
    // Retry keys are per owner.
    assert_eq!(
        log(&store, other, "k", push("2026-10-05", 60.0, 8))["already_existed"],
        false
    );
    assert!(
        store
            .workout_log(
                99_999,
                Log {
                    request_key: "orphan".into(),
                    workout: push("2026-10-05", 80.0, 8)
                }
            )
            .is_err()
    );
}

#[test]
fn history_pages_newest_logged_first_with_full_sessions() {
    let (_tmp, store, id) = setup();
    let a = id_of(&log(&store, id, "a", push("2026-10-05", 80.0, 8)));
    let b = id_of(&log(&store, id, "b", row("2026-10-02", "7:05")));
    log(&store, id, "c", day("2026-11-01", "legs"));
    let page = store.workout_history(id, history(Some(1), None)).unwrap();
    assert_eq!(page["sessions"][0]["id"], b);
    assert_eq!(page["sessions"][0]["rowing"]["split_500m"], "1:46.3");
    assert_eq!(page["next_before_id"], b);
    let rest = store
        .workout_history(id, history(Some(1), Some(b)))
        .unwrap();
    assert_eq!(rest["sessions"][0]["id"], a);
    assert_eq!(rest["sessions"][0]["exercises"][1]["name"], "Dips");
    assert!(rest["next_before_id"].is_null());
    assert!(
        rest["sessions"][0]["exercises"][0]
            .get("progression")
            .is_none()
    );

    for bad in [
        history(Some(0), None),
        history(Some(21), None),
        history(None, Some(0)),
        History {
            start_date: "2026-11-01".into(),
            ..history(None, None)
        },
    ] {
        assert!(store.workout_history(id, bad).is_err());
    }
}

#[test]
fn invalid_input_never_writes() {
    let (tmp, store, id) = setup();
    let empty = workout(json!({"session_date": "2026-10-05", "day_type": "push"}));
    for (key, w) in [
        ("k", empty),
        ("", push("2026-10-05", 80.0, 8)),
        (&"k".repeat(129), push("2026-10-05", 80.0, 8)),
        ("k", push("2026-10-05", 1000.5, 8)),
    ] {
        let attempt = Log {
            request_key: key.into(),
            workout: w,
        };
        assert!(store.workout_log(id, attempt).is_err());
    }
    for (session, version) in [(0, 1), (1, 0)] {
        let update = Update {
            id: session,
            expected_version: version,
            workout: push("2026-10-05", 80.0, 8),
        };
        assert!(store.workout_update(id, update).is_err());
        let remove = Remove {
            id: session,
            expected_version: version,
        };
        assert!(store.workout_remove(id, remove).is_err());
    }
    let invalid = Update {
        id: 1,
        expected_version: 1,
        workout: push("2026-10-05", -1.0, 8),
    };
    assert!(store.workout_update(id, invalid).is_err());
    assert_eq!(
        count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sessions"),
        0
    );
    // The model cannot pick the user.
    assert!(
        serde_json::from_value::<Log>(json!({"request_key": "x", "user_id": id,
            "workout": {"session_date": "2026-10-05", "day_type": "vo2", "rowing": {"time": "7:05"}}}))
        .is_err()
    );
}

#[test]
fn a_failed_set_insert_leaves_no_partial_session() {
    let (tmp, store, id) = setup();
    tmp.raw()
        .execute_batch(
            "CREATE TRIGGER reject_sets BEFORE INSERT ON workout_sets
             BEGIN SELECT RAISE(ABORT, 'write unavailable'); END;",
        )
        .unwrap();
    let attempt = Log {
        request_key: "k".into(),
        workout: push("2026-10-05", 80.0, 8),
    };
    assert!(store.workout_log(id, attempt).is_err());
    assert_eq!(
        count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sessions"),
        0
    );
    tmp.raw().execute_batch("DROP TRIGGER reject_sets").unwrap();
    assert_eq!(
        log(&store, id, "k", push("2026-10-05", 80.0, 8))["already_existed"],
        false
    );
}

#[test]
fn concurrent_corrections_apply_once() {
    let (tmp, store, id) = setup();
    let session = id_of(&log(&store, id, "k", push("2026-10-05", 80.0, 8)));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [80.0, 82.5]
        .into_iter()
        .map(|kg| {
            let store = Store::open(tmp.path()).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let update = Update {
                    id: session,
                    expected_version: 1,
                    workout: push("2026-10-05", kg, 8),
                };
                store.workout_update(id, update).is_ok()
            })
        })
        .collect();
    let successes = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(successes, 1);
    // One whole workout won: three bench sets and one dip set, never six.
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sets"), 4);
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

#[tokio::test]
async fn every_tool_acts_for_the_sessions_owner_only() {
    let (_tmp, store, id) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("http", "a").await.unwrap();
    let s = session(&service, &user, "gym").await.id;
    assert!(
        WorkoutNext(store.clone())
            .call(&mut ToolContext::default(), NoArgs {})
            .await
            .is_err()
    );
    let logged = invoke(
        WorkoutLog(store.clone()),
        &s,
        Log {
            request_key: "k".into(),
            workout: push("2000-01-03", 80.0, 8),
        },
    )
    .await
    .unwrap();
    let entry = id_of(&logged);
    let last = invoke(WorkoutLast(store.clone()), &s, last(DayType::Push, None))
        .await
        .unwrap();
    assert_eq!(last["session"]["id"], entry);
    // Whatever today is, the last lifting day was a push long ago.
    let next = invoke(WorkoutNext(store.clone()), &s, NoArgs {})
        .await
        .unwrap();
    assert_eq!(next["next_day_type"], "pull");
    assert_eq!(next["vo2"]["due"], true);
    let progress_result = invoke(
        ExerciseProgress(store.clone()),
        &s,
        progress(Kind::Lift, Some("Bench Press"), None),
    )
    .await
    .unwrap();
    assert_eq!(progress_result["sessions"][0]["session_id"], entry);
    let page = invoke(
        WorkoutHistory(store.clone()),
        &s,
        History {
            start_date: "2000-01-01".into(),
            end_date: "2000-01-31".into(),
            limit: None,
            before_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(page["sessions"][0]["id"], entry);
    let update = Update {
        id: entry,
        expected_version: 1,
        workout: push("2000-01-03", 82.5, 8),
    };
    assert_eq!(
        invoke(WorkoutUpdate(store.clone()), &s, update)
            .await
            .unwrap()["version"],
        2
    );
    let remove = Remove {
        id: entry,
        expected_version: 2,
    };
    assert_eq!(
        invoke(WorkoutRemove(store.clone()), &s, remove)
            .await
            .unwrap()["version"],
        3
    );
    // An unknown session is refused before any storage call.
    let e = invoke(WorkoutNext(store.clone()), "missing", NoArgs {})
        .await
        .unwrap_err();
    assert!(e.to_string().contains("unknown session"), "{e}");
    assert_eq!(
        store.workout_history(id, history(None, None)).unwrap()["sessions"],
        json!([])
    );
}

#[tokio::test]
async fn linked_channels_recall_the_last_push_through_the_real_agent_loop() {
    let tmp = TempDb::new();
    let store = tmp.open();
    let service = Service::new(store.clone(), "m", |_| {});
    let tg = service.user("telegram", "123").await.unwrap();
    service.link_http_user(&tg, "my-api").await.unwrap();
    let http = service.user("http", "my-api").await.unwrap();
    let a = session(&service, &tg, "telegram").await;
    let b = session(&service, &http, "api").await;
    let pushed = json!({"session_date": "2000-01-03", "day_type": "push", "exercises": [
        {"name": "Overhead Press", "sets": [{"reps": 6, "weight_kg": 50, "target_reps": 6}]}]});
    let model = MockCompletionModel::new([
        MockTurn::tool_call(
            "a",
            "workout_log",
            json!({"request_key": "push-1", "workout": pushed}),
        ),
        MockTurn::text("logged"),
        MockTurn::tool_call("b", "workout_last", json!({"day_type": "push"})),
        MockTurn::text("last time: 50 kg x 6, try 52.5"),
    ]);
    let agent = agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    service
        .send(&agent, &tg, &a.id, "log my push")
        .await
        .unwrap();
    service
        .send(&agent, &http, &b.id, "what did I press last push day?")
        .await
        .unwrap();
    let requests = model.requests();
    let result = serde_json::to_value(requests[3].chat_history.last().unwrap()).unwrap();
    let recalled = &result["content"][0]["content"][0]["value"]["session"];
    assert_eq!(recalled["exercises"][0]["name"], "Overhead Press");
    assert_eq!(
        recalled["exercises"][0]["progression"]["suggested"],
        json!({"weight_kg": 52.5, "reps": 6})
    );
    // The preamble tells the model when to reach for these tools.
    let system = serde_json::to_value(&requests[0].chat_history[0]).unwrap();
    assert!(system.to_string().contains("call workout_next"), "{system}");
    assert_eq!(
        count(&tmp.raw(), "SELECT COUNT(*) FROM workout_sessions"),
        1
    );
}
