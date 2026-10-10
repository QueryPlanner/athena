use super::testing::{CLIENT_SECRET, key, test_config};
use super::*;

fn cfg(redirect: Option<&str>) -> Result<Option<Config>> {
    config(
        Some("id".into()),
        Some("secret".into()),
        Some(key()),
        redirect.map(str::to_string),
    )
}

fn err(result: Result<Option<Config>>) -> String {
    result.expect_err("an error").to_string()
}

fn at(s: &str) -> Timestamp {
    s.parse().unwrap()
}

// ---- settings ----

#[test]
fn without_the_three_settings_google_health_is_off() {
    assert!(config(None, None, None, None).unwrap().is_none());
    let blank = || Some("  ".to_string());
    assert!(config(blank(), blank(), blank(), None).unwrap().is_none());
    // The redirect alone is not a setting that turns it on.
    assert!(
        config(None, None, None, Some("https://x.example/cb".into()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_partly_set_configuration_names_what_is_missing_but_never_a_value() {
    let message = err(config(
        Some("the-client-id".into()),
        None,
        Some("the-key-value".into()),
        None,
    ));
    assert!(message.contains("GOOGLE_HEALTH_CLIENT_SECRET"), "{message}");
    assert!(!message.contains("CLIENT_ID"), "{message}");
    assert!(!message.contains("the-client-id"), "{message}");
    assert!(!message.contains("the-key-value"), "{message}");
    let message = err(config(None, Some("s".into()), None, None));
    assert!(message.contains("GOOGLE_HEALTH_CLIENT_ID"), "{message}");
    assert!(
        message.contains("GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY"),
        "{message}"
    );
}

#[test]
fn the_key_must_be_url_safe_base64_of_exactly_32_bytes() {
    let padded = URL_SAFE.encode([9u8; 32]);
    assert!(padded.ends_with('='));
    assert!(decode_key(&padded).is_ok());
    assert_eq!(decode_key(padded.trim_end_matches('=')).unwrap(), [9u8; 32]);
    // A Fernet key's alphabet: `-` and `_`, never `+` and `/`.
    let tricky = [0xfb, 0xff, 0xfe].repeat(11);
    let encoded = URL_SAFE.encode(&tricky[..32]);
    assert!(encoded.contains(['-', '_']), "{encoded}");
    assert_eq!(decode_key(&encoded).unwrap(), tricky[..32]);
    for bad in [
        "not base64!",
        &URL_SAFE.encode([1u8; 31]),
        &URL_SAFE.encode([1u8; 33]),
        &base64::engine::general_purpose::STANDARD.encode(&tricky[..32]),
    ] {
        let message = err(config(
            Some("id".into()),
            Some("s".into()),
            Some(bad.into()),
            None,
        ));
        assert!(message.contains("exactly 32 bytes"), "{message}");
        assert!(!message.contains(bad), "{message}");
    }
}

#[test]
fn the_redirect_defaults_to_blackis_and_must_be_https_or_local_http() {
    let config = cfg(None).unwrap().unwrap();
    assert_eq!(config.redirect.as_str(), DEFAULT_REDIRECT_URI);
    assert_eq!(config.client_id, "id");
    let custom = cfg(Some(" https://athena.example.com/health/cb "))
        .unwrap()
        .unwrap();
    assert_eq!(
        custom.redirect.as_str(),
        "https://athena.example.com/health/cb"
    );
    assert!(cfg(Some("http://localhost:3000/cb")).is_ok());
    // A blank value is the default.
    assert_eq!(
        cfg(Some("  ")).unwrap().unwrap().redirect.as_str(),
        DEFAULT_REDIRECT_URI
    );
    for bad in [
        "http://athena.example.com/cb",
        "ftp://127.0.0.1/cb",
        "not a url",
        "https://x.example/cb?a=b",
        "https://x.example/cb#frag",
        "mailto:a@b.example",
    ] {
        let message = err(cfg(Some(bad)));
        assert!(
            message.contains("GOOGLE_HEALTH_REDIRECT_URI"),
            "{bad}: {message}"
        );
    }
}

#[test]
fn settings_are_read_from_the_environment_by_one_function() {
    // The test cannot set variables safely, so it checks the function
    // agrees with the environment it runs in.
    let names = [
        "GOOGLE_HEALTH_CLIENT_ID",
        "GOOGLE_HEALTH_CLIENT_SECRET",
        "GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY",
    ];
    let result = Config::from_env();
    if names.iter().all(|n| std::env::var(n).is_err()) {
        assert!(matches!(result, Ok(None)));
    }
}

#[test]
fn nothing_secret_shows_in_debug_output() {
    let config = test_config();
    let shown = format!(
        "{config:?} {:?} {:?}",
        config.client_secret,
        Callback {
            code: Some(Secret::new("the-code")),
            state: Some("the-state".into()),
            error: None
        }
    );
    for secret in [CLIENT_SECRET, "the-code", "the-state", &key()] {
        assert!(!shown.contains(secret), "{shown}");
    }
    assert!(shown.contains(DEFAULT_REDIRECT_URI), "{shown}");
}

// ---- the stored token ----

fn cipher() -> Cipher {
    Cipher::new(&[7u8; 32])
}

#[test]
fn a_sealed_token_opens_for_its_user_and_only_with_its_key() {
    let sealed = cipher().seal(5, "refresh-token-value");
    assert_eq!(sealed[0], 1);
    assert!(!String::from_utf8_lossy(&sealed).contains("refresh-token"));
    assert_eq!(
        cipher().open(5, &sealed).unwrap().expose(),
        "refresh-token-value"
    );
    // Random nonces: the same token never seals the same way twice.
    assert_ne!(sealed, cipher().seal(5, "refresh-token-value"));
    // Not another user's row, not another key.
    let wrong_user = cipher().open(6, &sealed).err().unwrap().to_string();
    assert!(wrong_user.contains("cannot be decrypted"), "{wrong_user}");
    assert!(Cipher::new(&[8u8; 32]).open(5, &sealed).is_err());
    // Not after damage.
    let mut damaged = sealed.clone();
    *damaged.last_mut().unwrap() ^= 1;
    assert!(cipher().open(5, &damaged).is_err());
}

#[test]
fn an_unreadable_stored_token_is_refused_without_its_contents() {
    let sealed = cipher().seal(5, "refresh-token-value");
    let mut newer = sealed.clone();
    newer[0] = 2;
    for (bad, why) in [
        (&newer[..], "format this build does not know"),
        (&[][..], "format this build does not know"),
        (&sealed[..NONCE], "too short"),
    ] {
        let message = cipher().open(5, bad).err().unwrap().to_string();
        assert!(message.contains(why), "{message}");
    }
    // Sealed correctly, but not text.
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = 5i64.to_le_bytes();
    let body = cipher()
        .0
        .encrypt(
            &nonce,
            Payload {
                msg: &[0xff, 0xfe],
                aad: &aad,
            },
        )
        .unwrap();
    let mut blob = vec![VERSION];
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&body);
    let message = cipher().open(5, &blob).err().unwrap().to_string();
    assert!(message.contains("not text"), "{message}");
}

#[test]
fn the_configs_cipher_uses_its_key() {
    let sealed = test_config().cipher().seal(1, "t");
    assert_eq!(cipher().open(1, &sealed).unwrap().expose(), "t");
}

// ---- the consent link ----

#[test]
fn states_are_long_random_and_stored_only_as_a_hash() {
    let (a, b) = (new_state(), new_state());
    assert_ne!(a, b);
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    // SHA-256 of "abc".
    assert_eq!(
        hash_state("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_ne!(hash_state(&a), a);
}

#[test]
fn the_pkce_verifier_comes_from_the_secret_and_the_state() {
    let config = test_config();
    let v = verifier(&config, "state-1");
    assert_eq!(v.expose().len(), 64);
    assert_eq!(v.expose(), verifier(&config, "state-1").expose());
    assert_ne!(v.expose(), verifier(&config, "state-2").expose());
    let other = config_with_secret("another-secret");
    assert_ne!(v.expose(), verifier(&other, "state-1").expose());
    assert!(!v.expose().contains("state-1"));
    // SHA-256 of "abc", URL-safe base64 without padding.
    assert_eq!(
        challenge(&Secret::new("abc")),
        "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
    );
}

fn config_with_secret(secret: &str) -> Config {
    config(Some("id".into()), Some(secret.into()), Some(key()), None)
        .unwrap()
        .unwrap()
}

#[test]
fn the_consent_link_asks_for_offline_read_only_access_with_pkce() {
    let config = test_config();
    let link = Url::parse(&config.authorization_url("the-state")).unwrap();
    assert_eq!(
        link.origin().ascii_serialization(),
        "https://accounts.google.com"
    );
    assert_eq!(link.path(), "/o/oauth2/v2/auth");
    let query: std::collections::HashMap<_, _> = link.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "test-client-id");
    assert_eq!(query["redirect_uri"], DEFAULT_REDIRECT_URI);
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["access_type"], "offline");
    assert_eq!(query["prompt"], "consent");
    assert_eq!(query["include_granted_scopes"], "true");
    assert_eq!(query["state"], "the-state");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(
        query["code_challenge"],
        challenge(&verifier(&config, "the-state"))
    );
    let scopes: Vec<&str> = query["scope"].split(' ').collect();
    assert_eq!(scopes, SCOPES);
    assert!(scopes.iter().all(|s| s.ends_with(".readonly")));
    assert!(!query["scope"].contains("writeonly"));
    // The client secret is not in the link.
    assert!(!link.as_str().contains(CLIENT_SECRET));
}

// ---- recognising a pasted callback ----

fn found(text: &str, redirect: Option<&Url>) -> Callback {
    find_callback(text, redirect).unwrap_or_else(|| panic!("no callback in {text:?}"))
}

fn parts(c: &Callback) -> (Option<&str>, Option<&str>, Option<&str>) {
    (
        c.code.as_ref().map(Secret::expose),
        c.state.as_deref(),
        c.error.as_deref(),
    )
}

#[test]
fn a_pasted_redirect_url_gives_its_code_and_state_decoded_once() {
    let redirect = Url::parse(DEFAULT_REDIRECT_URI).unwrap();
    let url = format!("{DEFAULT_REDIRECT_URI}?state=abc123&code=4%2F0AxY-z_9&scope=a%20b");
    let c = found(&url, Some(&redirect));
    assert_eq!(parts(&c), (Some("4/0AxY-z_9"), Some("abc123"), None));
    // Around other words, in brackets, or as a Markdown link.
    for text in [
        format!("here it is {url} thanks"),
        format!("<{url}>"),
        format!("({url})"),
        format!("[open]({url})"),
        format!("\"{url}\""),
        format!("line one\n{url}\nline three"),
        format!("{url}#fragment"),
    ] {
        let c = found(&text, Some(&redirect));
        assert_eq!(
            parts(&c),
            (Some("4/0AxY-z_9"), Some("abc123"), None),
            "{text}"
        );
    }
}

#[test]
fn the_scheme_may_be_missing_and_the_whole_url_may_be_percent_encoded() {
    let redirect = Url::parse(DEFAULT_REDIRECT_URI).unwrap();
    let bare = "127.0.0.1:8080/integrations/google-health/callback?code=c1&state=s1";
    assert_eq!(
        parts(&found(bare, Some(&redirect))),
        (Some("c1"), Some("s1"), None)
    );
    let encoded = "http%3A%2F%2F127.0.0.1%3A8080%2Fintegrations%2Fgoogle-health%2Fcallback%3Fcode%3Dc2%26state%3Ds2";
    assert_eq!(
        parts(&found(encoded, Some(&redirect))),
        (Some("c2"), Some("s2"), None)
    );
}

#[test]
fn a_declined_or_partial_callback_is_still_a_callback() {
    let redirect = Url::parse(DEFAULT_REDIRECT_URI).unwrap();
    let denied = format!("{DEFAULT_REDIRECT_URI}?error=access_denied&state=s1");
    assert_eq!(
        parts(&found(&denied, Some(&redirect))),
        (None, Some("s1"), Some("access_denied"))
    );
    let empty = DEFAULT_REDIRECT_URI;
    assert_eq!(parts(&found(empty, Some(&redirect))), (None, None, None));
    // An unrelated parameter is ignored.
    let extra = format!("{DEFAULT_REDIRECT_URI}?x=1&code=c");
    assert_eq!(
        parts(&found(&extra, Some(&redirect))),
        (Some("c"), None, None)
    );
}

#[test]
fn another_redirect_uri_is_matched_by_its_host_and_path() {
    let redirect = Url::parse("https://athena.example.com/health/cb").unwrap();
    let url = "https://athena.example.com/health/cb?code=c&state=s";
    assert_eq!(
        parts(&found(url, Some(&redirect))),
        (Some("c"), Some("s"), None)
    );
    assert!(find_callback("see athena.example.com/health/other", Some(&redirect)).is_none());
}

#[test]
fn without_google_health_a_url_with_a_code_and_state_is_still_caught() {
    let url = "http://127.0.0.1:8080/whatever?code=c&state=s";
    assert_eq!(parts(&found(url, None)), (Some("c"), Some("s"), None));
    // But the path alone is not a callback when no redirect is known.
    let path = format!("{DEFAULT_REDIRECT_URI}?foo=1");
    assert!(find_callback(&path, None).is_none());
}

#[test]
fn ordinary_messages_are_not_callbacks() {
    let redirect = Url::parse(DEFAULT_REDIRECT_URI).unwrap();
    for text in [
        "",
        "hello there",
        "what is my state of mind",
        "the code=5 is wrong",
        "my state=ok",
        "https://example.com/page?code=1",
        "/connect_health",
    ] {
        assert!(find_callback(text, Some(&redirect)).is_none(), "{text:?}");
    }
}

#[test]
fn the_first_matching_word_wins() {
    let text = "a?code=first&state=one b?code=second&state=two";
    assert_eq!(
        parts(&found(text, None)),
        (Some("first"), Some("one"), None)
    );
}

#[test]
fn malformed_escapes_are_left_alone() {
    assert_eq!(unescape("a%2Fb%zz%4"), "a/b%zz%4");
    assert_eq!(unescape("100%"), "100%");
    assert_eq!(unescape("%e2%82%ac"), "\u{20ac}");
    // Bytes that are not UTF-8 do not panic.
    assert_eq!(unescape("%ff"), "\u{fffd}");
}

// ---- the daily schedule ----

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap()
}

#[test]
fn the_daily_sync_is_due_after_0530_local_once() {
    let paris = zone("Europe/Paris");
    // 05:30 on 2026-10-10 in Paris (UTC+2) is 03:30 UTC.
    let (before, after) = (at("2026-10-10T03:29:59Z"), at("2026-10-10T03:30:00Z"));
    assert!(!due(before, None, false, &paris));
    assert!(due(after, None, false, &paris));
    // Attempted yesterday: due. Attempted since the time: not due.
    assert!(due(after, Some(at("2026-10-09T03:31:00Z")), false, &paris));
    assert!(!due(after, Some(at("2026-10-10T03:30:00Z")), false, &paris));
    assert!(!due(after, Some(at("2026-10-10T03:31:00Z")), false, &paris));
    // Last attempt yesterday but it is not yet 05:30 today: wait.
    assert!(!due(
        before,
        Some(at("2026-10-09T10:00:00Z")),
        false,
        &paris
    ));
}

#[test]
fn it_follows_the_users_zone_and_the_clock_change() {
    // Kolkata: 05:30 is 00:00 UTC.
    let kolkata = zone("Asia/Kolkata");
    assert!(!due(at("2026-10-09T23:59:00Z"), None, false, &kolkata));
    assert!(due(at("2026-10-10T00:00:00Z"), None, false, &kolkata));
    // Paris on the night the clocks go back (2026-10-25): 05:30 is CET, 04:30 UTC.
    let paris = zone("Europe/Paris");
    assert!(!due(at("2026-10-25T04:29:00Z"), None, false, &paris));
    assert!(due(at("2026-10-25T04:30:00Z"), None, false, &paris));
}

#[test]
fn a_failed_sync_is_tried_again_after_an_hour() {
    let paris = zone("Europe/Paris");
    let failed_at = at("2026-10-10T03:30:00Z");
    // 59 minutes later: not yet. An hour later: again, failed or not by then.
    assert!(!due(
        at("2026-10-10T04:29:00Z"),
        Some(failed_at),
        true,
        &paris
    ));
    assert!(due(
        at("2026-10-10T04:30:00Z"),
        Some(failed_at),
        true,
        &paris
    ));
    // And when it did not fail, the hour means nothing.
    assert!(!due(
        at("2026-10-10T08:00:00Z"),
        Some(failed_at),
        false,
        &paris
    ));
}
