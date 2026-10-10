//! Reminders: their rows, the scheduler's claims and outcomes on them, and
//! the tools through the real agent loop. Delivery is in tests/telegram.rs.
mod common;
use athena::reminders::*;
use athena::scheduler::{BACKOFF, LEASE, MAX_ATTEMPTS};
use athena::store::{Due, Store};
use athena::{agent, service::Service};
use common::*;
use jiff::{SignedDuration, Timestamp};
use rig_agent::{
    agent::AgentBuilder,
    tool::{Tool, ToolContext, ToolExecutionError},
};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use serde_json::{Value, json};

fn at(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// Saturday 2026-10-10, 03:00 UTC: 08:30 in Kolkata, the default zone.
fn now() -> Timestamp {
    at("2026-10-10T03:00:00Z")
}

fn setup() -> (TempDb, Store, i64) {
    let tmp = TempDb::new();
    let store = tmp.open();
    let id = store.user("telegram", "4242").unwrap().id();
    (tmp, store, id)
}

fn create(args: Value) -> Create {
    serde_json::from_value(args).unwrap()
}

fn notify_in(minutes: i64) -> Create {
    create(json!({"kind": "notify", "text": "stand up", "in_minutes": minutes}))
}

fn task_in(minutes: i64) -> Create {
    create(json!({"kind": "agent_task", "text": "check the news", "in_minutes": minutes}))
}

/// Create a reminder as of `at`; a task is confirmed as its user would, by
/// replying with its id and code.
fn add(store: &Store, owner: i64, args: &Create, at: Timestamp) -> anyhow::Result<Value> {
    let shown = store.create_reminder(owner, "s", args, at)?;
    let Some(code) = shown["confirmation_code"].as_str() else {
        return Ok(shown);
    };
    let id = shown["id"].as_i64().unwrap();
    store.confirm_reminder(owner, "s", id, code, &format!("confirm #{id} {code}"), at)
}

/// A job's (status, next_run_at, lease_until, attempts, sent_at, last_error).
type Row = (String, i64, Option<i64>, i64, Option<i64>, Option<String>);

fn row(tmp: &TempDb, id: i64) -> Row {
    tmp.raw()
        .query_row(
            "SELECT status, next_run_at, lease_until, attempts, sent_at, last_error
             FROM jobs WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap()
}

fn ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}

#[test]
fn a_reminder_is_stored_for_its_owner_and_shown_on_their_clock() {
    let (tmp, store, owner) = setup();
    let shown = add(
        &store,
        owner,
        &create(json!({"kind":"notify","text":"tea","repeat":"weekly",
                           "time":"09:00","weekdays":["sat","mon"]})),
        now(),
    )
    .unwrap();
    assert_eq!(
        shown,
        json!({"id": 1, "kind": "notify", "text": "tea", "status": "active",
               "repeat": "weekly on mon, sat at 09:00", "last_error": null,
               "next_run": {"datetime": "2026-10-10T09:00+05:30", "weekday": "Saturday",
                            "timezone": "Asia/Kolkata"}})
    );
    let stored: (i64, String, String, i64) = tmp
        .raw()
        .query_row(
            "SELECT user_id, kind, recurrence, created_at FROM jobs WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            owner,
            "notify".into(),
            "weekly:mon,sat@09:00".into(),
            ms(now())
        )
    );
    assert_eq!(row(&tmp, 1).1, ms(at("2026-10-10T03:30:00Z")));

    // Invalid arguments write nothing.
    assert!(add(&store, owner, &notify_in(0), now()).is_err());
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM jobs"), 1);
}

#[test]
fn only_users_with_a_telegram_chat_can_have_reminders() {
    let (tmp, store, _) = setup();
    let http = store.user("http", "api").unwrap().id();
    let e = add(&store, http, &notify_in(5), now()).unwrap_err();
    assert!(
        e.to_string().contains("never messaged the Telegram bot"),
        "{e}"
    );
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM jobs"), 0);
}

#[test]
fn limits_on_active_tasks_reminders_and_creations_hold() {
    let (tmp, store, owner) = setup();
    for _ in 0..MAX_ACTIVE_TASKS {
        add(&store, owner, &task_in(60), now()).unwrap();
    }
    let e = add(&store, owner, &task_in(60), now()).unwrap_err();
    assert!(e.to_string().contains("active agent_task"), "{e}");
    for _ in MAX_ACTIVE_TASKS..MAX_ACTIVE {
        add(&store, owner, &notify_in(60), now()).unwrap();
    }
    let e = add(&store, owner, &notify_in(60), now()).unwrap_err();
    assert!(e.to_string().contains("50 active reminders"), "{e}");
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM jobs"), MAX_ACTIVE);

    // Cancelling makes room, but creations still count for 24 hours.
    for id in 1..=MAX_ACTIVE {
        store.cancel_reminder(owner, id, now()).unwrap();
    }
    for _ in MAX_ACTIVE..MAX_CREATED_PER_DAY {
        let id = add(&store, owner, &notify_in(60), now()).unwrap()["id"]
            .as_i64()
            .unwrap();
        store.cancel_reminder(owner, id, now()).unwrap();
    }
    let e = add(&store, owner, &notify_in(60), now()).unwrap_err();
    assert!(e.to_string().contains("last 24 hours"), "{e}");
    let tomorrow = now() + SignedDuration::from_hours(24);
    add(&store, owner, &notify_in(60), tomorrow).unwrap();
}

#[test]
fn list_shows_active_ones_soonest_first_and_recent_failures() {
    let (tmp, store, owner) = setup();
    let other = store.user("telegram", "5555").unwrap().id();
    add(&store, owner, &notify_in(90), now()).unwrap();
    add(&store, owner, &task_in(30), now()).unwrap();
    add(&store, other, &notify_in(10), now()).unwrap();
    add(&store, owner, &notify_in(20), now()).unwrap();
    store.cancel_reminder(owner, 4, now()).unwrap();
    store.job_failed(1, now(), "blocked").unwrap();
    tmp.raw()
        .execute("UPDATE jobs SET recurrence = 'hourly@x' WHERE id = 2", [])
        .unwrap();

    let list = store.reminders(owner, now()).unwrap();
    let active: Vec<_> = list["active"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(active, [json!(2)]);
    // A schedule this build cannot read is shown as stored.
    assert_eq!(list["active"][0]["repeat"], "hourly@x");
    assert_eq!(list["failed"][0]["id"], 1);
    assert_eq!(list["failed"][0]["last_error"], "blocked");
    // A week later the failure is no longer listed.
    let later = store
        .reminders(owner, now() + SignedDuration::from_hours(24 * 8))
        .unwrap();
    assert_eq!(later["failed"], json!([]));
}

#[test]
fn only_the_owner_can_cancel_and_only_once() {
    let (tmp, store, owner) = setup();
    let other = store.user("telegram", "5555").unwrap().id();
    add(&store, owner, &notify_in(5), now()).unwrap();
    let e = store.cancel_reminder(other, 1, now()).unwrap_err();
    assert!(e.to_string().contains("no active reminder 1"), "{e}");
    assert_eq!(
        store.cancel_reminder(owner, 1, now()).unwrap(),
        json!({"cancelled": 1})
    );
    assert!(store.cancel_reminder(owner, 1, now()).is_err());
    assert_eq!(row(&tmp, 1).0, "cancelled");
    // A cancelled job is never claimed.
    assert!(
        store
            .claim_jobs(now() + SignedDuration::from_hours(1), 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn due_jobs_are_leased_oldest_first_and_carry_the_chat() {
    let (tmp, store, owner) = setup();
    add(&store, owner, &notify_in(30), now()).unwrap();
    add(&store, owner, &notify_in(10), now()).unwrap();
    add(&store, owner, &notify_in(120), now()).unwrap();
    let then = now() + SignedDuration::from_mins(60);

    let claimed = store.claim_jobs(then, 1).unwrap();
    assert_eq!(
        claimed,
        [Due {
            id: 2,
            owner,
            kind: "notify".into(),
            payload: "stand up".into(),
            due: now() + SignedDuration::from_mins(10),
            recurrence: None,
            attempts: 0,
            chat: Some(4242),
        }]
    );
    assert_eq!(row(&tmp, 2).2, Some(ms(then + LEASE)));
    let rest: Vec<i64> = store
        .claim_jobs(then, 10)
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(rest, [1]);
    // Leased jobs wait for their lease to lapse.
    assert!(store.claim_jobs(then, 10).unwrap().is_empty());
    let lapsed: Vec<i64> = store
        .claim_jobs(then + LEASE, 10)
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(lapsed, [2, 1]);

    // A user who lost their Telegram identity is claimed without a chat.
    tmp.raw()
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             UPDATE user_identities SET transport = 'gone' WHERE transport = 'telegram'",
        )
        .unwrap();
    let orphan = store.claim_jobs(then + LEASE + LEASE, 1).unwrap();
    assert_eq!(orphan[0].chat, None);
}

fn claim_one(store: &Store, at: Timestamp) -> Due {
    let mut claimed = store.claim_jobs(at, 1).unwrap();
    assert_eq!(claimed.len(), 1);
    claimed.remove(0)
}

#[test]
fn a_delivered_one_off_is_done_and_a_repeat_waits_for_its_next_time() {
    let (tmp, store, owner) = setup();
    add(&store, owner, &notify_in(5), now()).unwrap();
    let daily = create(json!({"kind":"notify","text":"x","repeat":"daily","time":"09:00"}));
    add(&store, owner, &daily, now()).unwrap();
    let late = at("2026-10-12T10:00:00Z");

    let once = claim_one(&store, late);
    assert_eq!(store.next_run(&once, late).unwrap(), None);
    store.job_done(once.id, late, None, true, None).unwrap();
    assert_eq!(
        row(&tmp, 1),
        ("done".into(), ms(once.due), None, 0, Some(ms(late)), None)
    );

    let repeat = claim_one(&store, late);
    // Two days late: the next run is tomorrow, not the missed ones.
    let next = store.next_run(&repeat, late).unwrap().unwrap();
    assert_eq!(next, at("2026-10-13T03:30:00Z"));
    store
        .job_done(repeat.id, late, Some(next), true, None)
        .unwrap();
    assert_eq!(
        row(&tmp, 2),
        ("active".into(), ms(next), None, 0, Some(ms(late)), None)
    );

    // A skipped run records why and leaves sent_at alone.
    let skip_at = next + SignedDuration::from_mins(1);
    let again = claim_one(&store, skip_at);
    let after = store.next_run(&again, skip_at).unwrap();
    store
        .job_done(again.id, skip_at, after, false, Some("skipped"))
        .unwrap();
    let r = row(&tmp, 2);
    assert_eq!((r.4, r.5.as_deref()), (Some(ms(late)), Some("skipped")));

    // A job cancelled while it ran stays cancelled.
    store.cancel_reminder(owner, 2, skip_at).unwrap();
    store.job_done(2, skip_at, after, true, None).unwrap();
    assert_eq!(row(&tmp, 2).0, "cancelled");
}

#[test]
fn a_repeat_whose_zone_no_longer_resolves_cannot_be_planned() {
    let (tmp, store, owner) = setup();
    let daily = create(json!({"kind":"notify","text":"x","repeat":"daily","time":"09:00"}));
    add(&store, owner, &daily, now()).unwrap();
    tmp.raw()
        .execute(
            "INSERT INTO user_settings VALUES (?1, 'Gone/Away', 0)",
            [owner],
        )
        .unwrap();
    let job = claim_one(&store, at("2026-10-11T00:00:00Z"));
    assert!(store.next_run(&job, now()).is_err());
    tmp.raw()
        .execute("UPDATE jobs SET recurrence = 'hourly@x'", [])
        .unwrap();
    let job = Due {
        recurrence: Some("hourly@x".into()),
        ..job
    };
    assert!(store.next_run(&job, now()).is_err());
}

#[test]
fn failed_deliveries_back_off_then_fail() {
    let (tmp, store, owner) = setup();
    add(&store, owner, &notify_in(5), now()).unwrap();
    let mut t = now() + SignedDuration::from_mins(5);
    let mut waits = Vec::new();
    for attempt in 1..MAX_ATTEMPTS {
        let job = claim_one(&store, t);
        assert_eq!(job.attempts, attempt - 1);
        assert!(store.job_retry(&job, t, "timeout").unwrap());
        let (status, due, lease, attempts, _, error) = row(&tmp, 1);
        assert_eq!(
            (status.as_str(), attempts, error.as_deref()),
            ("active", attempt, Some("timeout"))
        );
        // The occurrence keeps its due time, for the late note.
        assert_eq!(due, ms(now() + SignedDuration::from_mins(5)));
        let wait = lease.unwrap() - ms(t);
        waits.push(wait);
        assert!(
            store
                .claim_jobs(t + SignedDuration::from_millis(wait - 1), 1)
                .unwrap()
                .is_empty()
        );
        t += SignedDuration::from_millis(wait);
    }
    let minute = BACKOFF.as_millis() as i64;
    assert_eq!(waits, [minute, 2 * minute, 4 * minute, 8 * minute]);
    let job = claim_one(&store, t);
    assert!(!store.job_retry(&job, t, "timeout").unwrap());
    assert_eq!(row(&tmp, 1).0, "failed");
}

#[test]
fn deferred_jobs_wait_and_failed_ones_stop() {
    let (tmp, store, owner) = setup();
    add(&store, owner, &task_in(5), now()).unwrap();
    let t = now() + SignedDuration::from_mins(5);
    let job = claim_one(&store, t);
    store
        .job_defer(job.id, t + SignedDuration::from_mins(1))
        .unwrap();
    assert!(
        store
            .claim_jobs(t + SignedDuration::from_secs(59), 1)
            .unwrap()
            .is_empty()
    );
    let again = claim_one(&store, t + SignedDuration::from_mins(1));
    store.job_failed(again.id, t, "blocked").unwrap();
    assert_eq!(row(&tmp, 1).0, "failed");
    assert!(
        store
            .claim_jobs(t + SignedDuration::from_hours(2), 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn task_runs_are_counted_over_any_24_hours_whatever_became_of_them() {
    let (tmp, store, owner) = setup();
    for _ in 0..3 {
        add(&store, owner, &task_in(5), now()).unwrap();
    }
    add(&store, owner, &notify_in(5), now()).unwrap();
    let t = now() + SignedDuration::from_mins(5);
    for job in store.claim_jobs(t, 10).unwrap() {
        store.job_done(job.id, t, None, job.id != 3, None).unwrap();
    }
    store.cancel_reminder(owner, 2, t).ok();
    tmp.raw()
        .execute("UPDATE jobs SET status = 'cancelled' WHERE id = 1", [])
        .unwrap();
    // Jobs 1 and 2 ran (one since cancelled); 3 was skipped; 4 is a notify.
    let day_before = t - SignedDuration::from_hours(24);
    assert_eq!(store.task_runs_since(owner, day_before).unwrap(), 2);
    assert_eq!(store.task_runs_since(owner, t).unwrap(), 0);
}

#[test]
fn changing_zone_moves_the_next_run_of_repeats_to_the_new_wall_clock() {
    let (tmp, store, owner) = setup();
    let daily = create(json!({"kind":"notify","text":"x","repeat":"daily","time":"09:00"}));
    add(&store, owner, &daily, Timestamp::now()).unwrap();
    add(&store, owner, &notify_in(60), Timestamp::now()).unwrap();
    let once_before = row(&tmp, 2).1;
    store.set_timezone(owner, "America/New_York").unwrap();
    let next = Timestamp::from_millisecond(row(&tmp, 1).1).unwrap();
    let local = next.to_zoned(jiff::tz::TimeZone::get("America/New_York").unwrap());
    assert_eq!(local.strftime("%H:%M").to_string(), "09:00");
    assert!(next > Timestamp::now());
    // A one-off keeps the instant it was given.
    assert_eq!(row(&tmp, 2).1, once_before);

    // A schedule that cannot be read fails the change, which writes nothing.
    tmp.raw()
        .execute("UPDATE jobs SET recurrence = 'hourly@x' WHERE id = 1", [])
        .unwrap();
    assert!(store.set_timezone(owner, "Europe/Paris").is_err());
    assert_eq!(
        athena::timezone::name(&store.timezone(owner).unwrap()),
        "America/New_York"
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

#[tokio::test]
async fn the_tools_act_for_the_sessions_owner_only() {
    let (_tmp, store, _) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("telegram", "4242").await.unwrap();
    let s = session(&service, &user, "default").await;

    let unknown = invoke(ReminderList(store.clone()), "missing", NoArgs {}).await;
    assert!(unknown.unwrap_err().to_string().contains("unknown session"));
    let created = invoke(ReminderCreate(store.clone()), &s.id, notify_in(30))
        .await
        .unwrap();
    assert_eq!(created["kind"], "notify");
    let listed = invoke(ReminderList(store.clone()), &s.id, NoArgs {})
        .await
        .unwrap();
    assert_eq!(listed["active"][0]["id"], created["id"]);
    let id = created["id"].as_i64().unwrap();
    let cancelled = invoke(ReminderCancel(store.clone()), &s.id, Cancel { id })
        .await
        .unwrap();
    assert_eq!(cancelled, json!({"cancelled": id}));
    let again = invoke(ReminderCancel(store.clone()), &s.id, Cancel { id }).await;
    assert!(
        again
            .unwrap_err()
            .to_string()
            .contains("no active reminder")
    );
}

#[tokio::test]
async fn the_agent_schedules_through_the_real_loop() {
    let (tmp, store, _) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("telegram", "4242").await.unwrap();
    let s = session(&service, &user, "default").await;
    let model = MockCompletionModel::new([
        MockTurn::tool_call(
            "a",
            "reminder_create",
            json!({"kind": "agent_task", "text": "summarise the news", "repeat": "daily",
                   "time": "07:30"}),
        ),
        MockTurn::text("scheduled"),
        MockTurn::tool_call(
            "b",
            "reminder_create",
            json!({"kind": "notify", "text": "x"}),
        ),
        MockTurn::text("asked when"),
    ]);
    let agent = agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    for prompt in ["every morning at 7:30 summarise the news", "remind me"] {
        service.send(&agent, &user, &s.id, prompt).await.unwrap();
    }
    let requests = model.requests();
    let result = |i: usize| {
        let message = serde_json::to_value(requests[i].chat_history.last().unwrap()).unwrap();
        message["content"][0]["content"][0].clone()
    };
    assert_eq!(result(1)["value"]["repeat"], "daily at 07:30");
    assert_eq!(result(1)["value"]["next_run"]["timezone"], "Asia/Kolkata");
    assert!(
        result(3)
            .to_string()
            .contains("exactly one of at or in_minutes"),
        "{}",
        result(3)
    );
    assert_eq!(count(&tmp.raw(), "SELECT COUNT(*) FROM jobs"), 1);
    // The preamble tells the model how to use them, and that tasks wait.
    let preamble = serde_json::to_string(&requests[0].chat_history[0]).unwrap();
    assert!(preamble.contains("reminder_confirm"), "{preamble}");

    // The task waits. The model calling reminder_confirm with the code it
    // saw is refused until the user's own message carries it.
    let code = result(1)["value"]["confirmation_code"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(result(1)["value"]["status"], "pending");
    let confirm = json!({"id": 1, "code": code});
    let model = MockCompletionModel::new([
        MockTurn::tool_call("c", "reminder_confirm", confirm.clone()),
        MockTurn::text("you need to confirm"),
        MockTurn::tool_call("d", "reminder_confirm", confirm),
        MockTurn::text("scheduled"),
    ]);
    let agent = agent::configure_persistent(
        AgentBuilder::new(model.clone()).memory(service.memory()),
        None,
        &athena::custom::Custom::default(),
        &athena::mcp::Mcp::none(),
        store.clone(),
        None,
    );
    let reply = format!("confirm #1 {code}");
    for prompt in ["sure, go ahead", reply.as_str()] {
        service.send(&agent, &user, &s.id, prompt).await.unwrap();
    }
    let requests = model.requests();
    let result = |i: usize| {
        let message = serde_json::to_value(requests[i].chat_history.last().unwrap()).unwrap();
        message["content"][0]["content"][0].clone()
    };
    assert!(
        result(1).to_string().contains("Not confirmed"),
        "{}",
        result(1)
    );
    assert_eq!(result(3)["value"]["status"], "active");
    assert_eq!(row(&tmp, 1).0, "active");
}

#[test]
fn a_stored_time_out_of_range_is_an_error_not_a_guess() {
    let (tmp, store, owner) = setup();
    add(&store, owner, &notify_in(5), now()).unwrap();
    tmp.raw()
        .execute("UPDATE jobs SET next_run_at = ?1", [i64::MAX])
        .unwrap();
    let e = store.reminders(owner, now()).unwrap_err();
    assert!(format!("{e:#}").contains("Conversion error"), "{e:#}");
}

/// Create a task as of [`now`] in session "s" without confirming it: its
/// id and code.
fn preview(store: &Store, owner: i64, args: &Create) -> (i64, String) {
    let shown = store.create_reminder(owner, "s", args, now()).unwrap();
    assert_eq!(shown["status"], "pending");
    let code = shown["confirmation_code"].as_str().unwrap().to_string();
    (shown["id"].as_i64().unwrap(), code)
}

#[test]
fn a_task_waits_for_the_users_own_confirmation() {
    let (tmp, store, owner) = setup();
    let shown = store
        .create_reminder(owner, "s", &task_in(30), now())
        .unwrap();
    let code = shown["confirmation_code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);
    assert!(
        code.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    );
    assert!(!code.contains(['0', 'O', '1', 'I']), "{code}");
    assert_eq!(shown["status"], "pending");
    assert_eq!(shown["text"], "check the news");
    let step = shown["next_step"].as_str().unwrap();
    assert!(step.contains(&format!("confirm #1 {code}")), "{step}");
    assert!(step.contains("10 minutes"), "{step}");
    // A notify needs no confirmation.
    let plain = store
        .create_reminder(owner, "s", &notify_in(30), now())
        .unwrap();
    assert_eq!(plain["status"], "active");
    assert!(plain.get("confirmation_code").is_none());

    // Pending: never claimed, listed apart.
    let later = now() + SignedDuration::from_mins(31);
    let claimed: Vec<i64> = store
        .claim_jobs(later, 10)
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(claimed, [2]);
    let list = store.reminders(owner, now()).unwrap();
    assert_eq!(list["awaiting_confirmation"][0]["id"], 1);
    assert_eq!(list["active"].as_array().unwrap().len(), 1);

    let confirm = |session: &str, code: &str, said: &str, at: Timestamp| {
        store
            .confirm_reminder(owner, session, 1, code, said, at)
            .map_err(|e| e.to_string())
    };
    let t = now() + SignedDuration::from_mins(1);
    let lower = code.to_lowercase();
    for (session, code, said, why) in [
        (
            "s",
            code.as_str(),
            "yes please",
            "must contain #1 and the code",
        ),
        (
            "s",
            code.as_str(),
            &format!("yes {code}") as &str,
            "must contain #1",
        ),
        (
            "s",
            code.as_str(),
            "confirm #1",
            "must contain #1 and the code",
        ),
        ("s", "", "confirm #1", "must contain"),
        (
            "s",
            "ABCDEFGH",
            "confirm #1 ABCDEFGH",
            "does not confirm task 1",
        ),
        (
            "other",
            code.as_str(),
            &format!("confirm #1 {code}"),
            "does not confirm task 1",
        ),
    ] {
        let e = confirm(session, code, said, t).unwrap_err();
        assert!(e.contains(why), "{said}: {e}");
    }
    // Expired.
    let e = confirm(
        "s",
        &code,
        &format!("#1 {code}"),
        now() + SignedDuration::from_mins(10),
    )
    .unwrap_err();
    assert!(e.contains("expired"), "{e}");
    assert_eq!(row(&tmp, 1).0, "pending");

    // The user's reply, in any case and punctuation, confirms it once.
    let shown = confirm("s", &lower, &format!("Confirm #1, {lower}."), t).unwrap();
    assert_eq!(
        (shown["status"].as_str(), shown["confirmed"].as_bool()),
        (Some("active"), Some(true))
    );
    let e = confirm("s", &code, &format!("confirm #1 {code}"), t).unwrap_err();
    assert!(e.contains("no task 1 waiting"), "{e}");
    let codes = count(
        &tmp.raw(),
        "SELECT COUNT(*) FROM jobs WHERE confirm_code IS NOT NULL OR confirm_session IS NOT NULL",
    );
    assert_eq!(codes, 0);
    let claimed: Vec<i64> = store
        .claim_jobs(later, 10)
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(claimed, [1]);
}

#[test]
fn a_task_confirmed_after_its_time_moves_or_is_refused() {
    let (tmp, store, owner) = setup();
    let (once, code) = preview(&store, owner, &task_in(1));
    let late = now() + SignedDuration::from_mins(2);
    let e = store
        .confirm_reminder(owner, "s", once, &code, &format!("#{once} {code}"), late)
        .unwrap_err();
    assert!(
        e.to_string().contains("passed before it was confirmed"),
        "{e}"
    );
    assert_eq!(row(&tmp, once).0, "pending");

    // 08:31 in Kolkata: today's 08:31 run has passed by 08:35, so tomorrow.
    let daily = create(json!({"kind":"agent_task","text":"x","repeat":"daily","time":"08:31"}));
    let (id, code) = preview(&store, owner, &daily);
    let shown = store
        .confirm_reminder(owner, "s", id, &code, &format!("#{id} {code}"), late)
        .unwrap();
    assert_eq!(shown["next_run"]["datetime"], "2026-10-11T08:31+05:30");
}

#[test]
fn pending_tasks_count_until_they_expire_and_can_be_cancelled() {
    let (tmp, store, owner) = setup();
    for _ in 0..MAX_ACTIVE_TASKS {
        preview(&store, owner, &task_in(60));
    }
    let e = store
        .create_reminder(owner, "s", &task_in(60), now())
        .unwrap_err();
    assert!(e.to_string().contains("active agent_task"), "{e}");
    let expired = now() + CONFIRM_WINDOW;
    let shown = store
        .create_reminder(owner, "s", &task_in(60), expired)
        .unwrap();
    assert_eq!(shown["status"], "pending");
    // Expired previews are no longer listed.
    let list = store.reminders(owner, expired).unwrap();
    assert_eq!(list["awaiting_confirmation"].as_array().unwrap().len(), 1);

    let id = shown["id"].as_i64().unwrap();
    store.cancel_reminder(owner, id, expired).unwrap();
    assert_eq!(row(&tmp, id).0, "cancelled");
    let code: Option<String> = tmp
        .raw()
        .query_row("SELECT confirm_code FROM jobs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(code, None);
}

#[tokio::test]
async fn confirming_needs_the_users_own_text_in_the_tool_context() {
    let (_tmp, store, _) = setup();
    let service = Service::new(store.clone(), "m", |_| {});
    let user = service.user("telegram", "4242").await.unwrap();
    let s = session(&service, &user, "default").await;
    let preview = invoke(ReminderCreate(store.clone()), &s.id, task_in(30))
        .await
        .unwrap();
    let (id, code) = (
        preview["id"].as_i64().unwrap(),
        preview["confirmation_code"].as_str().unwrap().to_string(),
    );
    let args = || Confirm {
        id,
        code: code.clone(),
    };

    // No user text at all, as in a scheduled turn: refused.
    let mut bare = ToolContext::default();
    bare.insert(athena::runner::Conversation(s.id.clone()));
    let e = ReminderConfirm(store.clone())
        .call(&mut bare, args())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Not confirmed"), "{e}");

    let mut said = ToolContext::default();
    said.insert(athena::runner::Conversation(s.id.clone()));
    said.insert(athena::runner::UserText(format!(
        "yes, confirm #{id} {code}"
    )));
    let shown = ReminderConfirm(store.clone())
        .call(&mut said, args())
        .await
        .unwrap();
    assert_eq!(shown["status"], "active");
}
