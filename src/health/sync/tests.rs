use super::*;
use crate::health::testing::{FakeGoogle, SetClock, health, test_config, token_body};
use crate::health::{Callback, Cipher, hash_state};
use crate::store::Store;
use serde_json::{Value, json};

const T0: &str = "2026-10-10T10:00:00Z";

struct World {
    store: Store,
    fake: FakeGoogle,
    clock: Arc<SetClock>,
    health: Arc<Health>,
    owner: i64,
}

async fn world() -> World {
    let store = Store::open_in_memory().unwrap();
    let fake = FakeGoogle::start().await;
    let clock = SetClock::at(T0);
    let health = health(&store, &fake, clock.clone());
    let owner = store.user("telegram", "7").unwrap().id();
    World {
        store,
        fake,
        clock,
        health,
        owner,
    }
}

fn state_of(url: &str) -> String {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned()
}

fn pasted(code: &str, state: &str) -> Callback {
    Callback {
        code: Some(Secret::new(code)),
        state: Some(state.into()),
        error: None,
    }
}

impl World {
    /// Connect `owner` the way a user does, and return the state used.
    async fn connect(&self, owner: i64) -> String {
        let state = state_of(&self.health.begin(owner).await.unwrap());
        let linked = self.health.complete(owner, pasted("code-1", &state)).await;
        assert_eq!(linked, Linked::Connected);
        state
    }

    fn token_requests(&self) -> usize {
        self.fake.seen_at("/token").len()
    }

    fn data_requests(&self) -> Vec<crate::health::testing::Seen> {
        self.fake
            .seen()
            .into_iter()
            .filter(|s| s.path.contains("/dataPoints"))
            .collect()
    }

    fn connection(&self) -> HealthConnectionView {
        let c = self.store.health_connection(self.owner).unwrap().unwrap();
        HealthConnectionView {
            status: c.status,
            error: c.last_error,
            synced: c.last_synced_at,
        }
    }

    fn days(&self) -> Vec<(String, Value)> {
        self.store
            .health_days(self.owner, "2000-01-01", "2100-01-01")
            .unwrap()
            .into_iter()
            .map(|(d, m)| (d, serde_json::from_str(&m).unwrap()))
            .collect()
    }

    /// Make the next store call of this kind fail.
    fn break_table(&self, table: &str) {
        self.store
            .db_for_tests()
            .execute_batch(&format!("ALTER TABLE {table} RENAME TO {table}_gone"))
            .unwrap();
    }
}

struct HealthConnectionView {
    status: String,
    error: Option<String>,
    synced: Option<Timestamp>,
}

fn steps(start: &str, count: i64) -> Value {
    json!({"steps": {"interval": {"startTime": start, "endTime": start}, "count": count.to_string()}})
}

fn page(points: &[Value], next: Option<&str>) -> String {
    let mut body = json!({"dataPoints": points});
    if let Some(next) = next {
        body["nextPageToken"] = next.into();
    }
    body.to_string()
}

// ---- connecting ----

#[tokio::test]
async fn begin_stores_only_the_hash_of_the_state_in_the_link() {
    let w = world().await;
    let url = w.health.begin(w.owner).await.unwrap();
    let state = state_of(&url);
    let rows: Vec<(String, i64, i64)> = w
        .store
        .db_for_tests()
        .prepare("SELECT state_hash, expires_at, created_at FROM health_oauth_states")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let now = T0.parse::<Timestamp>().unwrap().as_millisecond();
    assert_eq!(rows, [(hash_state(&state), now + 600_000, now)]);
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
    assert!(!rows[0].0.contains(&state));
}

#[tokio::test]
async fn begin_reports_a_database_failure() {
    let w = world().await;
    w.break_table("health_oauth_states");
    assert!(w.health.begin(w.owner).await.is_err());
}

#[tokio::test]
async fn a_pasted_code_connects_stores_the_sealed_token_and_uses_the_state_up() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    let linked = w.health.complete(w.owner, pasted("the-code", &state)).await;
    assert_eq!(linked, Linked::Connected);

    // Google was asked with the code and the verifier for this state.
    let exchange = &w.fake.seen_at("/token")[0];
    assert_eq!(exchange.form["code"], "the-code");
    assert_eq!(
        exchange.form["code_verifier"],
        verifier(&test_config(), &state).expose()
    );
    // The token is stored sealed, bound to the user, with the granted scopes.
    let stored = w.store.health_connection(w.owner).unwrap().unwrap();
    assert_eq!(stored.status, "connected");
    assert_eq!(stored.connected_at, T0.parse().unwrap());
    assert!(stored.scopes.ends_with("googlehealth.sleep.readonly"));
    assert!(!String::from_utf8_lossy(&stored.token).contains("rt-1"));
    let cipher = Cipher::new(&[7u8; 32]);
    assert_eq!(
        cipher.open(w.owner, &stored.token).unwrap().expose(),
        "rt-1"
    );
    assert!(cipher.open(w.owner + 1, &stored.token).is_err());
    // A second paste of the same URL does nothing.
    let again = w.health.complete(w.owner, pasted("the-code", &state)).await;
    assert_eq!(again, Linked::BadState);
    assert_eq!(w.fake.seen_at("/token").len(), 1);
}

#[tokio::test]
async fn the_first_sync_after_connecting_needs_no_new_access_token() {
    let w = world().await;
    w.connect(w.owner).await;
    assert_eq!(w.token_requests(), 1);
    let outcome = w.health.sync_manual(w.owner).await;
    assert!(matches!(outcome, Outcome::Synced { .. }), "{outcome:?}");
    assert_eq!(w.token_requests(), 1);
    assert_eq!(
        w.data_requests()[0].authorization.as_deref(),
        Some("Bearer at-1")
    );
}

#[tokio::test]
async fn a_state_that_is_missing_unknown_expired_or_another_users_connects_nothing() {
    let w = world().await;
    let other = w.store.user("telegram", "8").unwrap().id();
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    let empty = Callback {
        code: Some(Secret::new("c")),
        state: None,
        error: None,
    };
    let blank = pasted("c", "");
    assert_eq!(w.health.complete(w.owner, empty).await, Linked::BadState);
    assert_eq!(w.health.complete(w.owner, blank).await, Linked::BadState);
    assert_eq!(
        w.health.complete(w.owner, pasted("c", "unknown")).await,
        Linked::BadState
    );
    // Another user pasting this user's link is refused, and does not spend it.
    assert_eq!(
        w.health.complete(other, pasted("c", &state)).await,
        Linked::BadState
    );
    // Ten minutes later it has expired.
    w.clock.set("2026-10-10T10:10:00Z");
    assert_eq!(
        w.health.complete(w.owner, pasted("c", &state)).await,
        Linked::BadState
    );
    assert!(w.fake.seen().is_empty());
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
    assert!(w.store.health_connection(other).unwrap().is_none());
}

#[tokio::test]
async fn another_users_failed_attempt_leaves_the_real_link_usable() {
    let w = world().await;
    let other = w.store.user("telegram", "8").unwrap().id();
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    assert_eq!(
        w.health.complete(other, pasted("c", &state)).await,
        Linked::BadState
    );
    assert_eq!(
        w.health.complete(w.owner, pasted("c", &state)).await,
        Linked::Connected
    );
}

#[tokio::test]
async fn declining_at_google_or_pasting_without_a_code_connects_nothing() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    let denied = Callback {
        code: None,
        state: Some(state.clone()),
        error: Some("access_denied".into()),
    };
    assert_eq!(w.health.complete(w.owner, denied).await, Linked::Denied);
    // The link was spent either way.
    assert_eq!(
        w.health.complete(w.owner, pasted("c", &state)).await,
        Linked::BadState
    );
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    let no_code = Callback {
        code: None,
        state: Some(state),
        error: None,
    };
    assert_eq!(w.health.complete(w.owner, no_code).await, Linked::NoCode);
    assert!(w.fake.seen().is_empty());
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn google_refusing_the_code_stores_nothing_and_shows_no_secret() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    w.fake.answer(
        "token",
        400,
        r#"{"error":"invalid_grant","error_description":"code-secret was bad"}"#,
    );
    let linked = w
        .health
        .complete(w.owner, pasted("code-secret", &state))
        .await;
    let Linked::Failed(why) = linked else {
        panic!("{linked:?}")
    };
    assert_eq!(why, "Google no longer accepts the stored authorisation");
    assert!(!why.contains("code-secret"));
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn an_answer_without_a_refresh_token_stores_nothing() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    w.fake.answer("token", 200, &token_body("at", None));
    let linked = w.health.complete(w.owner, pasted("c", &state)).await;
    assert_eq!(
        linked,
        Linked::Failed("Google did not return a refresh token".into())
    );
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn without_scopes_in_the_answer_the_requested_ones_are_recorded() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    w.fake
        .answer("token", 200, r#"{"access_token":"a","refresh_token":"r"}"#);
    w.health.complete(w.owner, pasted("c", &state)).await;
    let stored = w.store.health_connection(w.owner).unwrap().unwrap();
    assert_eq!(stored.scopes, SCOPES.join(" "));
}

#[tokio::test]
async fn database_failures_while_connecting_are_reported() {
    let w = world().await;
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    w.break_table("health_connections");
    let linked = w.health.complete(w.owner, pasted("c", &state)).await;
    assert!(matches!(linked, Linked::Failed(_)), "{linked:?}");
    let state = state_of(&w.health.begin(w.owner).await.unwrap());
    w.break_table("health_oauth_states");
    let linked = w.health.complete(w.owner, pasted("c", &state)).await;
    assert!(matches!(linked, Linked::Failed(_)), "{linked:?}");
}

// ---- syncing ----

#[tokio::test]
async fn a_sync_asks_for_every_type_over_the_users_last_14_local_days() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(
            &[
                steps("2026-10-09T08:00:00Z", 100),
                steps("2026-10-09T09:00:00Z", 50),
            ],
            None,
        ),
    );
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(
        outcome,
        Outcome::Synced {
            days: 1,
            unavailable: vec![]
        }
    );

    let asked: Vec<String> = w
        .data_requests()
        .iter()
        .map(|s| s.path.split('/').nth(5).unwrap().to_string())
        .collect();
    let expected: Vec<&str> = TYPES.iter().map(|(t, _)| *t).collect();
    assert_eq!(asked, expected);
    // The default zone is Asia/Kolkata: its days start at 18:30 UTC.
    let steps_filter = &w.data_requests()[0].query["filter"];
    assert_eq!(
        steps_filter,
        r#"steps.interval.start_time >= "2026-09-26T18:30:00Z" AND steps.interval.start_time < "2026-10-10T18:30:00Z""#
    );
    let exercise = w
        .data_requests()
        .into_iter()
        .find(|s| s.path.contains("/exercise/"))
        .unwrap();
    assert!(exercise.query["filter"].contains(r#">= "2026-09-27""#));
    assert!(exercise.query["filter"].contains(r#"< "2026-10-11""#));

    assert_eq!(w.days(), [("2026-10-09".into(), json!({"steps": 150}))]);
    let c = w.connection();
    assert_eq!(
        (c.status.as_str(), c.error, c.synced),
        ("connected", None, Some(T0.parse().unwrap()))
    );
}

#[tokio::test]
async fn pages_are_followed_until_there_is_no_next_page() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T08:00:00Z", 1)], Some("p2")),
    );
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T09:00:00Z", 2)], Some("p3")),
    );
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T10:00:00Z", 4)], None),
    );
    w.health.sync(w.owner).await;
    let tokens: Vec<Option<String>> = w
        .data_requests()
        .iter()
        .filter(|s| s.path.contains("/steps/"))
        .map(|s| s.query.get("pageToken").cloned())
        .collect();
    assert_eq!(tokens, [None, Some("p2".into()), Some("p3".into())]);
    assert_eq!(w.days(), [("2026-10-08".into(), json!({"steps": 7}))]);
}

#[tokio::test]
async fn endless_pages_stop_the_sync_without_replacing_anything() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T08:00:00Z", 1)], None),
    );
    w.health.sync(w.owner).await;
    for _ in 0..MAX_PAGES {
        w.fake.answer(
            "steps",
            200,
            &page(&[steps("2026-10-08T08:00:00Z", 9)], Some("again")),
        );
    }
    let outcome = w.health.sync(w.owner).await;
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(why.contains("more pages"), "{why}");
    // The earlier day is intact, and the failure is on record.
    assert_eq!(w.days(), [("2026-10-08".into(), json!({"steps": 1}))]);
    assert_eq!(w.connection().error.as_deref(), Some(why.as_str()));
    assert_eq!(w.connection().status, "connected");
}

#[tokio::test]
async fn a_type_google_forbids_is_reported_and_the_rest_is_kept() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 10)], None),
    );
    w.fake
        .answer("sleep", 403, r#"{"error":{"status":"PERMISSION_DENIED"}}"#);
    w.fake.answer("weight", 403, "{}");
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(
        outcome,
        Outcome::Synced {
            days: 1,
            unavailable: vec!["sleep".into(), "weight".into()]
        }
    );
}

#[tokio::test]
async fn any_other_failure_keeps_the_old_days_and_records_why_without_revoking() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 10)], None),
    );
    w.health.sync(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 99)], None),
    );
    w.fake.answer("sleep", 500, r#"{"error":"boom secret"}"#);
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(outcome, Outcome::Failed("Google answered HTTP 500".into()));
    assert_eq!(w.days(), [("2026-10-09".into(), json!({"steps": 10}))]);
    let c = w.connection();
    assert_eq!(c.status, "connected");
    assert_eq!(c.error.as_deref(), Some("Google answered HTTP 500"));
    assert_eq!(c.synced, Some(T0.parse().unwrap()));
    // The next good sync clears the error.
    w.clock.set("2026-10-11T10:00:00Z");
    w.health.sync(w.owner).await;
    assert_eq!(w.connection().error, None);
}

#[tokio::test]
async fn a_refused_access_token_is_forgotten_so_the_next_sync_refreshes() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer("steps", 401, "{}");
    let outcome = w.health.sync(w.owner).await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(w.token_requests(), 1);
    w.health.sync(w.owner).await;
    assert_eq!(w.token_requests(), 2);
    assert_eq!(w.connection().error, None);
}

#[tokio::test]
async fn the_access_token_is_cached_until_a_minute_before_it_expires() {
    let w = world().await;
    w.connect(w.owner).await;
    w.health.sync(w.owner).await;
    // expires_in is 3600 s; at 3539 s it is still used.
    w.clock.set("2026-10-10T10:58:59Z");
    w.health.sync(w.owner).await;
    assert_eq!(w.token_requests(), 1);
    // A minute from expiry it is replaced, with the stored refresh token.
    w.clock.set("2026-10-10T10:59:00Z");
    w.fake.answer("token", 200, &token_body("at-2", None));
    w.health.sync(w.owner).await;
    assert_eq!(w.token_requests(), 2);
    let refresh = &w.fake.seen_at("/token")[1];
    assert_eq!(refresh.form["grant_type"], "refresh_token");
    assert_eq!(refresh.form["refresh_token"], "rt-1");
    assert_eq!(
        w.data_requests().last().unwrap().authorization.as_deref(),
        Some("Bearer at-2")
    );
}

#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_stored_one_and_the_same_one_does_not() {
    let w = world().await;
    w.connect(w.owner).await;
    let before = w.store.health_connection(w.owner).unwrap().unwrap().token;
    w.clock.set("2026-10-10T12:00:00Z");
    // Google sends back the token we hold: nothing to store.
    w.fake
        .answer("token", 200, &token_body("at-2", Some("rt-1")));
    w.health.sync(w.owner).await;
    assert_eq!(
        w.store.health_connection(w.owner).unwrap().unwrap().token,
        before
    );
    // A different one is sealed and stored.
    w.clock.set("2026-10-10T14:00:00Z");
    w.fake
        .answer("token", 200, &token_body("at-3", Some("rt-2")));
    w.health.sync(w.owner).await;
    let stored = w.store.health_connection(w.owner).unwrap().unwrap().token;
    let cipher = Cipher::new(&[7u8; 32]);
    assert_eq!(cipher.open(w.owner, &stored).unwrap().expose(), "rt-2");
}

#[tokio::test]
async fn invalid_grant_revokes_the_connection_once_and_stops_all_requests() {
    let w = world().await;
    w.connect(w.owner).await;
    w.clock.set("2026-10-10T12:00:00Z");
    w.fake.answer("token", 400, r#"{"error":"invalid_grant"}"#);
    assert_eq!(w.health.sync(w.owner).await, Outcome::Revoked);
    let c = w.connection();
    assert_eq!(c.status, "revoked");
    assert!(c.error.unwrap().contains("rejected the refresh token"));
    // Later syncs say so without calling Google.
    let calls = w.fake.seen().len();
    assert_eq!(w.health.sync(w.owner).await, Outcome::AlreadyRevoked);
    assert_eq!(w.health.sync_manual(w.owner).await, Outcome::AlreadyRevoked);
    assert_eq!(w.fake.seen().len(), calls);
    // Reconnecting clears it.
    w.connect(w.owner).await;
    assert_eq!(w.connection().status, "connected");
}

#[tokio::test]
async fn a_revocation_someone_else_recorded_first_is_not_reported_twice() {
    let w = world().await;
    w.connect(w.owner).await;
    w.clock.set("2026-10-10T12:00:00Z");
    // While Google is answering the refresh, another process notes the
    // revocation too.
    let (store, owner) = (w.store.clone(), w.owner);
    w.fake.on_request(move |seen| {
        if seen.path == "/token" {
            let at = "2026-10-10T12:00:00Z".parse().unwrap();
            store.health_mark_revoked(owner, "elsewhere", at).unwrap();
        }
    });
    w.fake.answer("token", 400, r#"{"error":"invalid_grant"}"#);
    assert_eq!(w.health.sync(w.owner).await, Outcome::AlreadyRevoked);
    assert_eq!(w.connection().error.as_deref(), Some("elsewhere"));
}

#[tokio::test]
async fn a_token_that_cannot_be_decrypted_revokes_and_is_kept() {
    let w = world().await;
    let sealed = Cipher::new(&[9u8; 32]).seal(w.owner, "rt-old");
    w.store
        .health_connect(w.owner, &sealed, "s", T0.parse().unwrap())
        .unwrap();
    assert_eq!(w.health.sync(w.owner).await, Outcome::Revoked);
    let stored = w.store.health_connection(w.owner).unwrap().unwrap();
    assert_eq!(stored.token, sealed);
    assert!(stored.last_error.unwrap().contains("cannot be decrypted"));
    assert!(w.fake.seen().is_empty());
}

#[tokio::test]
async fn a_revocation_that_cannot_be_recorded_is_a_failure() {
    let w = world().await;
    let sealed = Cipher::new(&[9u8; 32]).seal(w.owner, "rt-old");
    w.store
        .health_connect(w.owner, &sealed, "s", T0.parse().unwrap())
        .unwrap();
    w.store
        .db_for_tests()
        .execute_batch(
            "CREATE TRIGGER no_status_update BEFORE UPDATE OF status
             ON health_connections BEGIN SELECT RAISE(ABORT, 'read only'); END",
        )
        .unwrap();
    let outcome = w.health.sync(w.owner).await;
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(why.contains("read only"), "{why}");
}

#[tokio::test]
async fn a_refresh_that_fails_for_another_reason_is_recorded_not_revoked() {
    let w = world().await;
    w.store
        .health_connect(
            w.owner,
            &test_config().cipher().seal(w.owner, "rt-1"),
            "s",
            T0.parse().unwrap(),
        )
        .unwrap();
    w.fake.answer("token", 503, "down");
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(outcome, Outcome::Failed("Google answered HTTP 503".into()));
    assert_eq!(w.connection().status, "connected");
    assert_eq!(
        w.connection().error.as_deref(),
        Some("Google answered HTTP 503")
    );
}

#[tokio::test]
async fn a_failure_to_store_a_rotated_token_fails_the_sync() {
    let w = world().await;
    w.connect(w.owner).await;
    w.clock.set("2026-10-10T12:00:00Z");
    w.fake
        .answer("token", 200, &token_body("at-2", Some("rt-2")));
    w.store
        .db_for_tests()
        .execute_batch(
            "CREATE TRIGGER no_token_update BEFORE UPDATE OF encrypted_refresh_token
             ON health_connections BEGIN SELECT RAISE(ABORT, 'read only'); END",
        )
        .unwrap();
    let outcome = w.health.sync(w.owner).await;
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(why.contains("read only"), "{why}");
}

#[tokio::test]
async fn syncing_someone_who_never_connected_or_disconnected_meanwhile() {
    let w = world().await;
    assert_eq!(w.health.sync(w.owner).await, Outcome::NotConnected);
    assert_eq!(w.health.sync_manual(w.owner).await, Outcome::NotConnected);
    // Disconnected while Google was answering: nothing is written back.
    w.connect(w.owner).await;
    let store = w.store.clone();
    let owner = w.owner;
    w.fake.on_request(move |seen| {
        if seen.path.contains("/steps/") {
            store.health_disconnect(owner).unwrap();
        }
    });
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 10)], None),
    );
    assert_eq!(w.health.sync(w.owner).await, Outcome::NotConnected);
    assert!(w.days().is_empty());
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn a_failure_to_save_the_days_is_reported() {
    let w = world().await;
    w.connect(w.owner).await;
    let store = w.store.clone();
    w.fake.on_request(move |seen| {
        if seen.path.contains("/body-fat/") {
            store
                .db_for_tests()
                .execute_batch("ALTER TABLE health_daily RENAME TO health_daily_gone")
                .unwrap();
        }
    });
    let outcome = w.health.sync(w.owner).await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(w.connection().error.is_some());
}

#[tokio::test]
async fn one_sync_per_user_at_a_time_in_this_process() {
    let w = world().await;
    w.connect(w.owner).await;
    lock(&w.health.running).insert(w.owner);
    assert_eq!(w.health.sync(w.owner).await, Outcome::Running);
    // Another user is not held up, and finishing frees the user.
    lock(&w.health.running).remove(&w.owner);
    assert!(matches!(
        w.health.sync(w.owner).await,
        Outcome::Synced { .. }
    ));
    assert!(lock(&w.health.running).is_empty());
}

#[tokio::test]
async fn syncing_needs_a_time_zone_that_still_resolves_and_a_readable_database() {
    let w = world().await;
    w.connect(w.owner).await;
    w.store
        .db_for_tests()
        .execute(
            "INSERT INTO user_settings (user_id, timezone, updated_at) VALUES (?1, 'Nowhere/Land', 0)",
            [w.owner],
        )
        .unwrap();
    let outcome = w.health.sync(w.owner).await;
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert!(why.contains("no longer resolves"), "{why}");
    assert!(w.data_requests().is_empty());
    w.break_table("health_connections");
    let outcome = w.health.sync(w.owner).await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
}

#[test]
fn the_window_is_whole_local_days_even_across_a_clock_change() {
    let paris = jiff::tz::TimeZone::get("Europe/Paris").unwrap();
    // Clocks go back on 2026-10-25: that day has 25 hours.
    let w = window("2026-10-25".parse().unwrap(), &paris);
    assert_eq!(w.start_date, "2026-10-12");
    assert_eq!(w.end_date, "2026-10-26");
    assert_eq!(w.start, "2026-10-11T22:00:00Z");
    assert_eq!(w.end, "2026-10-25T23:00:00Z");
}

// ---- the manual sync ----

#[tokio::test]
async fn a_manual_sync_is_allowed_once_an_hour() {
    let w = world().await;
    w.connect(w.owner).await;
    assert!(matches!(
        w.health.sync_manual(w.owner).await,
        Outcome::Synced { .. }
    ));
    w.clock.set("2026-10-10T10:20:00Z");
    assert_eq!(
        w.health.sync_manual(w.owner).await,
        Outcome::Cooldown("2026-10-10T11:00:00Z".parse().unwrap())
    );
    w.clock.set("2026-10-10T11:00:00Z");
    assert!(matches!(
        w.health.sync_manual(w.owner).await,
        Outcome::Synced { .. }
    ));
}

#[tokio::test]
async fn a_manual_sync_reports_a_database_failure() {
    let w = world().await;
    w.break_table("health_connections");
    assert!(matches!(
        w.health.sync_manual(w.owner).await,
        Outcome::Failed(_)
    ));
}

// ---- the daily pass ----

#[tokio::test]
async fn the_daily_pass_syncs_only_users_who_are_due_and_only_once() {
    let w = world().await;
    let other = w.store.user("telegram", "8").unwrap().id();
    w.connect(w.owner).await;
    w.connect(other).await;
    // Kolkata is 15:30 at T0, so the 05:30 sync is due. The other user's
    // zone is Pacific, where it is 03:00: not yet.
    w.store.set_timezone(other, "America/Los_Angeles").unwrap();
    let ran = w.health.run_due().await.unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!((ran[0].owner, ran[0].chat), (w.owner, Some(7)));
    assert!(matches!(ran[0].outcome, Outcome::Synced { .. }));
    // Not again today.
    assert!(w.health.run_due().await.unwrap().is_empty());
    // Pacific time passes 05:30 (12:30 UTC): their turn.
    w.clock.set("2026-10-10T12:30:00Z");
    let ran = w.health.run_due().await.unwrap();
    assert_eq!(ran.iter().map(|r| r.owner).collect::<Vec<_>>(), [other]);
    // Tomorrow morning in Kolkata, the first user again.
    w.clock.set("2026-10-11T00:00:00Z");
    let ran = w.health.run_due().await.unwrap();
    assert_eq!(ran.iter().map(|r| r.owner).collect::<Vec<_>>(), [w.owner]);
}

#[tokio::test]
async fn the_daily_pass_retries_a_failure_after_an_hour_and_reports_a_revocation() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer("sleep", 500, "{}");
    let ran = w.health.run_due().await.unwrap();
    assert!(matches!(ran[0].outcome, Outcome::Failed(_)));
    w.clock.set("2026-10-10T10:59:00Z");
    assert!(w.health.run_due().await.unwrap().is_empty());
    w.fake.answer("token", 400, r#"{"error":"invalid_grant"}"#);
    w.clock.set("2026-10-10T13:00:00Z");
    // The cached access token has expired by now, so Google is asked again.
    let ran = w.health.run_due().await.unwrap();
    assert_eq!(ran[0].outcome, Outcome::Revoked);
    // A revoked user is no longer a candidate.
    w.clock.set("2026-10-11T13:00:00Z");
    assert!(w.health.run_due().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_candidate_someone_else_claimed_first_is_skipped() {
    let w = world().await;
    w.connect(w.owner).await;
    let candidate = Candidate {
        owner: w.owner,
        chat: Some(7),
        // Stale: the database has no attempt, the candidate says there was one.
        last_attempt: Some("2026-10-09T00:00:00Z".parse().unwrap()),
        failed: false,
    };
    assert_eq!(
        w.health.run_candidate(candidate, w.health.now()).await,
        None
    );
    assert!(w.data_requests().is_empty());
}

#[tokio::test]
async fn a_user_with_an_unresolvable_zone_is_noted_and_skipped() {
    let w = world().await;
    w.connect(w.owner).await;
    w.store
        .db_for_tests()
        .execute(
            "INSERT INTO user_settings (user_id, timezone, updated_at) VALUES (?1, 'Nowhere/Land', 0)",
            [w.owner],
        )
        .unwrap();
    assert!(w.health.run_due().await.unwrap().is_empty());
    assert!(w.connection().error.unwrap().contains("no longer resolves"));
}

#[tokio::test]
async fn the_daily_pass_reports_a_database_failure() {
    let w = world().await;
    w.break_table("health_connections");
    assert!(w.health.run_due().await.is_err());
}

// ---- disconnecting ----

#[tokio::test]
async fn disconnecting_revokes_at_google_deletes_the_token_and_keeps_the_days() {
    let w = world().await;
    assert_eq!(w.health.disconnect(w.owner).await.unwrap(), None);
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 10)], None),
    );
    w.health.sync(w.owner).await;
    assert_eq!(
        w.health.disconnect(w.owner).await.unwrap(),
        Some(Remote::Revoked)
    );
    let revoke = &w.fake.seen_at("/revoke")[0];
    assert_eq!(revoke.form["token"], "rt-1");
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
    assert_eq!(w.days().len(), 1);
    // The cached access token went with it: reconnecting needs a fresh one
    // only because the new connection brings its own.
    assert!(lock(&w.health.access).is_empty());
}

#[tokio::test]
async fn a_revoke_google_does_not_confirm_is_reported_but_the_token_is_still_deleted() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer("revoke", 400, r#"{"error":"invalid_token"}"#);
    assert_eq!(
        w.health.disconnect(w.owner).await.unwrap(),
        Some(Remote::Failed)
    );
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn an_unreadable_token_is_deleted_without_asking_google() {
    let w = world().await;
    let sealed = Cipher::new(&[9u8; 32]).seal(w.owner, "rt");
    w.store
        .health_connect(w.owner, &sealed, "s", T0.parse().unwrap())
        .unwrap();
    assert_eq!(
        w.health.disconnect(w.owner).await.unwrap(),
        Some(Remote::Failed)
    );
    assert!(w.fake.seen().is_empty());
}

#[tokio::test]
async fn a_revoked_connection_is_deleted_with_nothing_to_revoke() {
    let w = world().await;
    w.connect(w.owner).await;
    w.store
        .health_mark_revoked(w.owner, "x", T0.parse().unwrap())
        .unwrap();
    let calls = w.fake.seen().len();
    assert_eq!(
        w.health.disconnect(w.owner).await.unwrap(),
        Some(Remote::NotNeeded)
    );
    assert_eq!(w.fake.seen().len(), calls);
    assert!(w.store.health_connection(w.owner).unwrap().is_none());
}

#[tokio::test]
async fn disconnecting_reports_a_database_failure() {
    let w = world().await;
    w.break_table("health_connections");
    assert!(w.health.disconnect(w.owner).await.is_err());
}

// ---- construction ----

#[tokio::test]
async fn the_service_is_built_from_the_environment_or_a_config() {
    let store = Store::open_in_memory().unwrap();
    let names = [
        "GOOGLE_HEALTH_CLIENT_ID",
        "GOOGLE_HEALTH_CLIENT_SECRET",
        "GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY",
    ];
    let result = Health::from_env(&store);
    if names.iter().all(|n| std::env::var(n).is_err()) {
        assert!(matches!(result, Ok(None)));
    }
    let production = Health::production(test_config(), &store);
    assert_eq!(
        production.redirect().as_str(),
        crate::health::DEFAULT_REDIRECT_URI
    );
    // It reads the system clock until told otherwise.
    let (before, now) = (Timestamp::now(), production.now());
    assert!(before <= now && now <= Timestamp::now());
    assert!(production.shared_clock().now() >= before);
}

// ---- the rest of a sync: refusals, storage, page caps ----

fn stored_points(w: &World, data_type: &str) -> i64 {
    w.store
        .db_for_tests()
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM health_points WHERE user_id = {} AND data_type = '{data_type}'",
                w.owner
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn backfill_of(w: &World, data_type: &str) -> Backfill {
    w.store
        .health_backfill(w.owner)
        .unwrap()
        .into_iter()
        .find(|b| b.data_type == data_type)
        .unwrap()
}

/// The requests for `data_type` that are not its window: the history chunks
/// of a daily pass, whose filters end before the window starts.
fn history_requests(w: &World, data_type: &str) -> Vec<String> {
    w.data_requests()
        .into_iter()
        .filter(|s| s.path.contains(&format!("/{data_type}/")))
        .map(|s| s.query["filter"].clone())
        .skip(1)
        .collect()
}

#[tokio::test]
async fn a_refused_optional_type_is_reported_with_its_status_and_the_sync_succeeds() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "ovulation-test",
        400,
        r#"{"error":{"status":"INVALID_ARGUMENT"}}"#,
    );
    w.fake.answer("moods", 404, "{}");
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(
        outcome,
        Outcome::Synced {
            days: 0,
            // In catalog order: ovulation-test is read before moods.
            unavailable: vec![
                "ovulation-test (HTTP 400)".into(),
                "moods (HTTP 404)".into()
            ]
        }
    );
    assert_eq!(w.connection().error, None);
}

#[tokio::test]
async fn a_refused_core_type_fails_the_sync_and_says_why() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake
        .answer("steps", 400, r#"{"error":{"status":"INVALID_ARGUMENT"}}"#);
    let outcome = w.health.sync(w.owner).await;
    let Outcome::Failed(why) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(why, "Google answered HTTP 400 (INVALID_ARGUMENT)");
    assert_eq!(w.connection().error.as_deref(), Some(why.as_str()));
    assert_eq!(w.connection().status, "connected");
}

#[tokio::test]
async fn each_page_is_stored_as_it_arrives() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T08:00:00Z", 1)], Some("p2")),
    );
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-08T09:00:00Z", 2)], None),
    );
    w.health.sync(w.owner).await;
    // Two distinct points, one per page, each with its own key.
    assert_eq!(stored_points(&w, "steps"), 2);
    assert_eq!(w.days(), [("2026-10-08".into(), json!({"steps": 3}))]);
}

#[tokio::test]
async fn a_page_cap_keeps_the_points_already_read() {
    let w = world().await;
    w.connect(w.owner).await;
    for i in 0..MAX_PAGES {
        w.fake.answer(
            "steps",
            200,
            // A different count on each page, so each point is a new row.
            &page(
                &[steps("2026-10-08T08:00:00Z", i as i64 + 1)],
                Some("again"),
            ),
        );
    }
    let outcome = w.health.sync(w.owner).await;
    assert!(
        matches!(outcome, Outcome::Failed(ref why) if why.contains("more pages")),
        "{outcome:?}"
    );
    // Every page that was read is stored, though the sync did not finish.
    assert_eq!(stored_points(&w, "steps"), MAX_PAGES as i64);
    assert!(w.days().is_empty());
}

#[tokio::test]
async fn a_failure_to_store_the_points_fails_the_sync_with_a_safe_message() {
    let w = world().await;
    w.connect(w.owner).await;
    w.break_table("health_points");
    let outcome = w.health.sync(w.owner).await;
    assert_eq!(
        outcome,
        Outcome::Failed("the data points could not be saved".into())
    );
    assert_eq!(w.connection().status, "connected");
}

// ---- the history fetch, in the daily pass only ----

#[tokio::test]
async fn a_manual_sync_fetches_no_history() {
    let w = world().await;
    w.connect(w.owner).await;
    w.health.sync(w.owner).await;
    assert!(history_requests(&w, "steps").is_empty());
    assert!(w.store.health_backfill(w.owner).unwrap().is_empty());
}

#[tokio::test]
async fn the_daily_pass_fetches_three_seven_day_chunks_backwards_and_saves_the_cursor() {
    let w = world().await;
    w.connect(w.owner).await;
    // The window ends 10 October (Kolkata); its history starts 27 September.
    let ran = w.health.run_due().await.unwrap();
    assert_eq!(ran.len(), 1);
    // Each chunk ends where the previous began (local midnight, UTC+5:30).
    assert_eq!(
        history_requests(&w, "steps")
            .iter()
            .map(|f| f
                .split("< \"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap()
                .to_string())
            .collect::<Vec<_>>(),
        [
            "2026-09-26T18:30:00Z",
            "2026-09-19T18:30:00Z",
            "2026-09-12T18:30:00Z"
        ]
    );
    let steps = backfill_of(&w, "steps");
    assert_eq!(
        (steps.oldest.as_str(), steps.done, steps.empty_run),
        ("2026-09-06", false, 3)
    );
}

#[tokio::test]
async fn history_stops_at_the_three_year_floor() {
    let w = world().await;
    w.connect(w.owner).await;
    let today: Date = "2026-10-10".parse().unwrap();
    let floor = today.saturating_sub(BACKFILL_FLOOR_DAYS.days());
    // Three days above the floor: one chunk, clamped to the floor, finishes it.
    let near = Backfill {
        data_type: "steps".into(),
        oldest: floor.saturating_add(3.days()).to_string(),
        done: false,
        empty_run: 0,
    };
    w.store
        .health_backfill_save(w.owner, &near, None, T0.parse().unwrap())
        .unwrap();
    w.health.run_due().await.unwrap();
    assert_eq!(history_requests(&w, "steps").len(), 1);
    let steps = backfill_of(&w, "steps");
    assert_eq!(
        (steps.done, steps.oldest.as_str()),
        (true, floor.to_string().as_str())
    );
}

#[tokio::test]
async fn history_stops_after_twelve_empty_chunks_in_a_row() {
    let w = world().await;
    w.connect(w.owner).await;
    let eleven = Backfill {
        data_type: "steps".into(),
        oldest: "2026-09-27".into(),
        done: false,
        empty_run: 11,
    };
    w.store
        .health_backfill_save(w.owner, &eleven, None, T0.parse().unwrap())
        .unwrap();
    w.health.run_due().await.unwrap();
    // One empty chunk is the twelfth: nothing more is asked for.
    assert_eq!(history_requests(&w, "steps").len(), 1);
    let steps = backfill_of(&w, "steps");
    assert_eq!((steps.done, steps.empty_run), (true, 12));
}

#[tokio::test]
async fn a_refused_type_is_done_for_good_after_one_chunk() {
    let w = world().await;
    w.connect(w.owner).await;
    // The window's answer is an empty page; the first history chunk is refused.
    w.fake.answer("sleep", 200, "{}");
    w.fake.answer("sleep", 403, "{}");
    w.health.run_due().await.unwrap();
    assert_eq!(history_requests(&w, "sleep").len(), 1);
    let sleep = backfill_of(&w, "sleep");
    assert!(sleep.done);
}

#[tokio::test]
async fn ecg_history_is_fetched_in_one_go_back_to_the_floor() {
    let w = world().await;
    w.connect(w.owner).await;
    w.health.run_due().await.unwrap();
    let history = history_requests(&w, "electrocardiogram");
    assert_eq!(history.len(), 1, "{history:?}");
    assert!(history[0].starts_with("electrocardiogram.interval.start_time >= "));
    // ECG has no upper bound to page through, so its one request is final.
    let ecg = backfill_of(&w, "electrocardiogram");
    assert!(ecg.done);
}

#[tokio::test]
async fn a_chunk_that_hits_the_page_cap_is_kept_as_partial_and_the_cursor_moves_on() {
    let w = world().await;
    w.connect(w.owner).await;
    // Window: one page. First two history chunks: empty. Third: endless.
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 1)], None),
    );
    w.fake.answer("steps", 200, "{}");
    w.fake.answer("steps", 200, "{}");
    for _ in 0..MAX_PAGES {
        w.fake.answer(
            "steps",
            200,
            &page(&[steps("2026-09-15T08:00:00Z", 1)], Some("again")),
        );
    }
    w.health.run_due().await.unwrap();
    // Two empty chunks, then the third chunk's MAX_PAGES pages.
    assert_eq!(history_requests(&w, "steps").len(), 2 + MAX_PAGES);
    let steps = backfill_of(&w, "steps");
    assert_eq!(
        (steps.oldest.as_str(), steps.done, steps.empty_run),
        ("2026-09-06", false, 0)
    );
    let note: Option<String> = w
        .store
        .db_for_tests()
        .query_row(
            &format!(
                "SELECT last_error FROM health_backfill WHERE user_id = {} AND data_type = 'steps'",
                w.owner
            ),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(note.unwrap().starts_with("partial: "));
}

#[tokio::test]
async fn a_transport_error_in_history_ends_the_pass_and_keeps_the_sync() {
    let w = world().await;
    w.connect(w.owner).await;
    w.fake.answer(
        "steps",
        200,
        &page(&[steps("2026-10-09T08:00:00Z", 1)], None),
    );
    w.fake.answer("steps", 500, "{}");
    let ran = w.health.run_due().await.unwrap();
    assert!(
        matches!(ran[0].outcome, Outcome::Synced { .. }),
        "{:?}",
        ran[0].outcome
    );
    // The cursor stays where it was, the chunk's error is kept, and no other
    // type's history was asked for.
    let steps = backfill_of(&w, "steps");
    assert_eq!((steps.oldest.as_str(), steps.done), ("2026-09-27", false));
    assert_eq!(w.data_requests().len(), TYPES.len() + 1);
    assert_eq!(w.connection().status, "connected");
    assert_eq!(w.connection().error, None);
}

#[tokio::test]
async fn an_unreadable_history_cursor_skips_the_history_and_keeps_the_sync() {
    let w = world().await;
    w.connect(w.owner).await;
    w.break_table("health_backfill");
    let ran = w.health.run_due().await.unwrap();
    assert!(
        matches!(ran[0].outcome, Outcome::Synced { .. }),
        "{:?}",
        ran[0].outcome
    );
    assert!(history_requests(&w, "steps").is_empty());
    assert_eq!(w.connection().status, "connected");
}

#[tokio::test]
async fn a_history_cursor_that_cannot_be_saved_ends_the_pass_and_keeps_the_sync() {
    let w = world().await;
    w.connect(w.owner).await;
    // The table goes away while the first history chunk is in flight, so
    // its cursor cannot be saved. The window's own request is left alone.
    let store = w.store.clone();
    w.fake.on_request(move |s| {
        let history = s.path.contains("/steps/")
            && s.query
                .get("filter")
                .is_some_and(|f| !f.contains(r#"< "2026-10-10T18:30:00Z""#));
        if history {
            store
                .db_for_tests()
                .execute_batch("ALTER TABLE health_backfill RENAME TO health_backfill_gone")
                .unwrap();
        }
    });
    let ran = w.health.run_due().await.unwrap();
    assert!(
        matches!(ran[0].outcome, Outcome::Synced { .. }),
        "{:?}",
        ran[0].outcome
    );
    // One history chunk was asked for, and then the pass stopped.
    assert_eq!(history_requests(&w, "steps").len(), 1);
    assert_eq!(w.data_requests().len(), TYPES.len() + 1);
    assert_eq!(w.connection().status, "connected");
    assert_eq!(w.connection().error, None);
}
