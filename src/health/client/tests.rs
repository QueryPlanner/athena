use super::*;
use crate::health::testing::{CLIENT_ID, CLIENT_SECRET, FakeGoogle, test_config, token_body};

async fn setup() -> (FakeGoogle, Google) {
    let fake = FakeGoogle::start().await;
    let google = Google::new(&test_config(), fake.endpoints());
    (fake, google)
}

fn window() -> Window {
    Window {
        start: "2026-09-27T00:00:00Z".into(),
        end: "2026-10-11T00:00:00Z".into(),
        start_date: "2026-09-27".into(),
        end_date: "2026-10-11".into(),
    }
}

#[test]
fn production_endpoints_are_googles_and_a_fake_has_one_base() {
    let p = Endpoints::production();
    assert_eq!(p.token, "https://oauth2.googleapis.com/token");
    assert_eq!(p.revoke, "https://oauth2.googleapis.com/revoke");
    assert_eq!(p.api, "https://health.googleapis.com");
    let f = Endpoints::at("http://127.0.0.1:9/");
    assert_eq!(f.token, "http://127.0.0.1:9/token");
    assert_eq!(f.revoke, "http://127.0.0.1:9/revoke");
    assert_eq!(f.api, "http://127.0.0.1:9");
}

#[test]
fn every_data_type_is_filtered_on_the_field_that_places_it_in_time() {
    let w = window();
    for (data_type, expected) in [
        (
            "steps",
            r#"steps.interval.start_time >= "2026-09-27T00:00:00Z" AND steps.interval.start_time < "2026-10-11T00:00:00Z""#,
        ),
        (
            "active-energy-burned",
            r#"active_energy_burned.interval.start_time >= "2026-09-27T00:00:00Z" AND active_energy_burned.interval.start_time < "2026-10-11T00:00:00Z""#,
        ),
        (
            "sleep",
            r#"sleep.interval.end_time >= "2026-09-27T00:00:00Z" AND sleep.interval.end_time < "2026-10-11T00:00:00Z""#,
        ),
        (
            "exercise",
            r#"exercise.interval.civil_start_time >= "2026-09-27" AND exercise.interval.civil_start_time < "2026-10-11""#,
        ),
        (
            "daily-resting-heart-rate",
            r#"daily_resting_heart_rate.date >= "2026-09-27" AND daily_resting_heart_rate.date < "2026-10-11""#,
        ),
        (
            "daily-heart-rate-zones",
            r#"daily_heart_rate_zones.date >= "2026-09-27" AND daily_heart_rate_zones.date < "2026-10-11""#,
        ),
        (
            "weight",
            r#"weight.sample_time.physical_time >= "2026-09-27T00:00:00Z" AND weight.sample_time.physical_time < "2026-10-11T00:00:00Z""#,
        ),
        (
            "body-fat",
            r#"body_fat.sample_time.physical_time >= "2026-09-27T00:00:00Z" AND body_fat.sample_time.physical_time < "2026-10-11T00:00:00Z""#,
        ),
    ] {
        assert_eq!(filter(data_type, &w), expected, "{data_type}");
    }
}

#[test]
fn an_error_shows_a_status_and_a_short_code_and_nothing_else() {
    assert_eq!(
        Error::Http {
            status: 500,
            code: Some("INTERNAL".into())
        }
        .to_string(),
        "Google answered HTTP 500 (INTERNAL)"
    );
    assert_eq!(
        Error::Http {
            status: 502,
            code: None
        }
        .to_string(),
        "Google answered HTTP 502"
    );
    for (error, words) in [
        (Error::Revoked, "no longer accepts"),
        (Error::Unauthorized, "refused the access token"),
        (Error::Forbidden, "refused access"),
        (Error::Transport("t-text"), "t-text"),
        (Error::Malformed("m-text"), "m-text"),
        (Error::TooMuch("x-text"), "x-text"),
    ] {
        assert!(error.to_string().contains(words), "{error}");
    }
}

#[test]
fn only_a_short_word_survives_as_an_error_code() {
    let code = |body: &str| error_code(body.as_bytes());
    assert_eq!(
        code(r#"{"error":"invalid_grant"}"#).as_deref(),
        Some("invalid_grant")
    );
    assert_eq!(
        code(r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"free text"}}"#).as_deref(),
        Some("RESOURCE_EXHAUSTED")
    );
    assert_eq!(code(r#"{"error":"has spaces and secret=abc"}"#), None);
    assert_eq!(code(r#"{"error":""}"#), None);
    assert_eq!(code(&format!(r#"{{"error":"{}"}}"#, "a".repeat(65))), None);
    assert_eq!(code(r#"{"error":{"message":"m"}}"#), None);
    assert_eq!(code("<html>"), None);
}

// ---- token calls ----

#[tokio::test]
async fn a_code_is_exchanged_with_the_secret_and_the_verifier_in_the_body() {
    let (fake, google) = setup().await;
    fake.answer("token", 200, &token_body("access-1", Some("refresh-1")));
    let tokens = google
        .exchange_code(&Secret::new("the-code"), &Secret::new("the-verifier"))
        .await
        .unwrap();
    assert_eq!(tokens.access.expose(), "access-1");
    assert_eq!(tokens.refresh.unwrap().expose(), "refresh-1");
    assert_eq!(tokens.expires_in, 3600);
    assert_eq!(
        tokens.scopes,
        ["https://www.googleapis.com/auth/googlehealth.sleep.readonly"]
    );
    let seen = fake.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/token")
    );
    // Everything is in the body; the URL carries nothing.
    assert!(seen[0].query.is_empty());
    let form = &seen[0].form;
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["code_verifier"], "the-verifier");
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(form["client_secret"], CLIENT_SECRET);
    assert_eq!(form["redirect_uri"], crate::health::DEFAULT_REDIRECT_URI);
}

#[tokio::test]
async fn a_refresh_token_is_swapped_for_an_access_token() {
    let (fake, google) = setup().await;
    fake.answer("token", 200, r#"{"access_token":"a2"}"#);
    let tokens = google.refresh(&Secret::new("refresh-1")).await.unwrap();
    // No `expires_in`, `scope` or `refresh_token` in Google's answer.
    assert_eq!(tokens.access.expose(), "a2");
    assert_eq!((tokens.expires_in, tokens.refresh.is_none()), (3600, true));
    assert!(tokens.scopes.is_empty());
    let form = &fake.seen()[0].form;
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "refresh-1");
    assert_eq!(form["client_secret"], CLIENT_SECRET);
    assert!(!form.contains_key("code"));
}

#[tokio::test]
async fn an_unusable_token_answer_is_refused() {
    let (fake, google) = setup().await;
    for body in [
        r#"{"expires_in":5}"#,
        r#"{"access_token":""}"#,
        "[]",
        "not json",
        "",
    ] {
        fake.answer("token", 200, body);
        let e = google.refresh(&Secret::new("r")).await.err().unwrap();
        assert!(matches!(e, Error::Malformed(_)), "{body}: {e:?}");
    }
}

#[tokio::test]
async fn invalid_grant_at_the_token_endpoint_means_revoked() {
    let (fake, google) = setup().await;
    fake.answer(
        "token",
        400,
        r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
    );
    assert_eq!(
        google.refresh(&Secret::new("r")).await.err(),
        Some(Error::Revoked)
    );
    // Other refusals are not a revocation.
    fake.answer("token", 400, r#"{"error":"invalid_request"}"#);
    assert_eq!(
        google.refresh(&Secret::new("r")).await.err(),
        Some(Error::Http {
            status: 400,
            code: Some("invalid_request".into())
        })
    );
    fake.answer("token", 401, r#"{"error":"invalid_client"}"#);
    assert_eq!(
        google.refresh(&Secret::new("r")).await.err(),
        Some(Error::Unauthorized)
    );
    fake.answer("token", 503, "<html>down</html>");
    assert_eq!(
        google.refresh(&Secret::new("r")).await.err(),
        Some(Error::Http {
            status: 503,
            code: None
        })
    );
}

#[tokio::test]
async fn invalid_grant_from_the_api_is_not_a_revocation() {
    let (fake, google) = setup().await;
    fake.answer("steps", 400, r#"{"error":"invalid_grant"}"#);
    let e = google
        .data_page(&Secret::new("a"), "steps", &window(), None)
        .await
        .err()
        .unwrap();
    assert_eq!(
        e,
        Error::Http {
            status: 400,
            code: Some("invalid_grant".into())
        }
    );
}

#[tokio::test]
async fn no_error_carries_what_google_sent_or_what_we_sent() {
    let (fake, google) = setup().await;
    let leak = "SECRET-LEAK-9f2c";
    let body = format!(
        r#"{{"error":"{leak} with spaces","error_description":"{leak}","refresh_token":"{leak}"}}"#
    );
    for status in [400, 403, 404, 429, 500] {
        fake.answer("token", status, &body);
        fake.answer("token", status, &body);
        fake.answer("steps", status, &body);
        let errors = [
            google
                .refresh(&Secret::new("rt-secret"))
                .await
                .err()
                .unwrap(),
            google
                .exchange_code(&Secret::new("code-secret"), &Secret::new("v-secret"))
                .await
                .err()
                .unwrap(),
            google
                .data_page(&Secret::new("at-secret"), "steps", &window(), None)
                .await
                .err()
                .unwrap(),
        ];
        for e in errors {
            let shown = format!("{e} {e:?}");
            for secret in [
                leak,
                "rt-secret",
                "code-secret",
                "v-secret",
                "at-secret",
                CLIENT_SECRET,
            ] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
    }
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let (fake, google) = setup().await;
    fake.answer("token", 302, "");
    let e = google.refresh(&Secret::new("r")).await.err().unwrap();
    assert_eq!(
        e,
        Error::Http {
            status: 302,
            code: None
        }
    );
    // One request: the `Location` was not visited.
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test]
async fn a_token_answer_over_the_limit_is_not_read() {
    let (fake, google) = setup().await;
    fake.answer("token", 200, &"x".repeat(TOKEN_BODY_LIMIT + 1));
    let e = google.refresh(&Secret::new("r")).await.err().unwrap();
    assert!(matches!(e, Error::TooMuch(_)), "{e:?}");
}

#[tokio::test]
async fn an_unreachable_google_is_a_transport_error_that_names_no_address() {
    let config = test_config();
    let google = Google::new(&config, Endpoints::at("http://127.0.0.1:1"));
    let e = google
        .refresh(&Secret::new("rt-secret"))
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Transport(_)), "{e:?}");
    assert!(!format!("{e} {e:?}").contains("127.0.0.1"));
}

#[tokio::test]
async fn revoking_sends_the_token_in_the_body_and_reports_a_refusal() {
    let (fake, google) = setup().await;
    google.revoke(&Secret::new("rt-1")).await.unwrap();
    let seen = &fake.seen()[0];
    assert_eq!(seen.path, "/revoke");
    assert_eq!(seen.form["token"], "rt-1");
    assert!(seen.query.is_empty());
    fake.answer("revoke", 400, r#"{"error":"invalid_token"}"#);
    assert_eq!(
        google.revoke(&Secret::new("rt-1")).await.err(),
        Some(Error::Http {
            status: 400,
            code: Some("invalid_token".into())
        })
    );
}

// ---- the Health API ----

#[tokio::test]
async fn a_page_of_points_is_asked_for_with_a_bearer_token_and_a_filter() {
    let (fake, google) = setup().await;
    fake.answer(
        "steps",
        200,
        r#"{"dataPoints":[{"steps":{"count":"5"}},{"steps":{"count":"6"}}],"nextPageToken":"p2"}"#,
    );
    let access = Secret::new("access-xyz");
    let page = google
        .data_page(&access, "steps", &window(), None)
        .await
        .unwrap();
    assert_eq!(page.points.len(), 2);
    assert_eq!(page.next.as_deref(), Some("p2"));
    let first = &fake.seen()[0];
    assert_eq!(first.method, "GET");
    assert_eq!(first.path, "/v4/users/me/dataTypes/steps/dataPoints");
    assert_eq!(first.authorization.as_deref(), Some("Bearer access-xyz"));
    assert_eq!(first.query["pageSize"], PAGE_SIZE.to_string());
    assert_eq!(first.query["filter"], filter("steps", &window()));
    assert!(!first.query.contains_key("pageToken"));
    // The token is in no URL.
    assert!(!first.query.values().any(|v| v.contains("access-xyz")));

    let last = google
        .data_page(&access, "steps", &window(), Some("p2"))
        .await
        .unwrap();
    assert!(last.points.is_empty() && last.next.is_none());
    assert_eq!(fake.seen()[1].query["pageToken"], "p2");
}

#[tokio::test]
async fn the_api_says_forbidden_unauthorized_or_too_much() {
    let (fake, google) = setup().await;
    let access = Secret::new("a");
    fake.answer("sleep", 403, r#"{"error":{"status":"PERMISSION_DENIED"}}"#);
    fake.answer("sleep", 401, "{}");
    fake.answer("sleep", 200, &"x".repeat(PAGE_BODY_LIMIT + 1));
    fake.answer("sleep", 200, "[1]");
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(
            google
                .data_page(&access, "sleep", &window(), None)
                .await
                .err()
                .unwrap(),
        );
    }
    assert_eq!(got[0], Error::Forbidden);
    assert_eq!(got[1], Error::Unauthorized);
    assert!(matches!(got[2], Error::TooMuch(_)));
    assert!(matches!(got[3], Error::Malformed(_)));
}

#[tokio::test]
async fn an_access_token_that_cannot_be_a_header_is_refused_before_sending() {
    let (fake, google) = setup().await;
    let e = google
        .data_page(&Secret::new("bad\ntoken"), "steps", &window(), None)
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Malformed(_)), "{e:?}");
    assert!(!format!("{e}").contains("bad"));
    assert!(fake.seen().is_empty());
}

#[test]
fn an_ecg_is_filtered_by_start_only_because_that_is_all_google_supports() {
    assert_eq!(
        filter("electrocardiogram", &window()),
        r#"electrocardiogram.interval.start_time >= "2026-09-27T00:00:00Z""#
    );
}

#[test]
fn a_sample_type_with_hyphens_is_filtered_by_its_snake_case_name() {
    assert_eq!(
        filter("respiratory-rate-sleep-summary", &window()),
        r#"respiratory_rate_sleep_summary.sample_time.physical_time >= "2026-09-27T00:00:00Z" AND respiratory_rate_sleep_summary.sample_time.physical_time < "2026-10-11T00:00:00Z""#
    );
}

#[tokio::test]
async fn each_type_asks_for_its_documented_page_size_and_never_the_data_source_family() {
    let (fake, google) = setup().await;
    let access = Secret::new("a");
    for data_type in [
        "sleep",
        "exercise",
        "electrocardiogram",
        "nutrition-log",
        "steps",
    ] {
        google
            .data_page(&access, data_type, &window(), None)
            .await
            .unwrap();
    }
    let sizes: Vec<(String, String)> = fake
        .seen()
        .iter()
        .map(|s| {
            let kind = s.path.split('/').nth(5).unwrap().to_string();
            (kind, s.query["pageSize"].clone())
        })
        .collect();
    assert_eq!(
        sizes,
        [
            ("sleep".to_string(), "25".to_string()),
            ("exercise".to_string(), "25".to_string()),
            ("electrocardiogram".to_string(), "10".to_string()),
            ("nutrition-log".to_string(), "200".to_string()),
            ("steps".to_string(), "1000".to_string()),
        ]
    );
    // No request sends a dataSourceFamily parameter.
    assert!(
        fake.seen()
            .iter()
            .all(|s| !s.query.contains_key("dataSourceFamily"))
    );
}
