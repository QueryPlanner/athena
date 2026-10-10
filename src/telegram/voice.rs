//! Voice notes to text, through Cloudflare Workers AI's hosted Whisper.
//!
//! The bot downloads a voice note or an audio file, sends it here, and runs
//! the transcript as an ordinary text turn. The audio is held in memory only
//! for the call and never stored or logged.
//!
//! Memory matters: the bot runs under `MemoryMax=128M`. A note costs its
//! bytes, then its base64 (a third larger), then the request body. So notes
//! are capped at [`VOICE_LIMIT`], the raw bytes are dropped once encoded, the
//! answer is read up to [`ANSWER_LIMIT`], and the bot transcribes one note at
//! a time.
//!
//! The API token is only ever in a header marked sensitive. Errors name the
//! HTTP status and Cloudflare's first error message, never the token, the
//! audio or the transcript.

use crate::media;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::Value;
use std::time::Duration;

/// The Workers AI model that transcribes.
pub const MODEL: &str = "@cf/openai/whisper-large-v3-turbo";

/// The Cloudflare API every production call goes to.
pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// The largest voice note or audio file transcribed, in bytes.
pub const VOICE_LIMIT: usize = 2 * 1024 * 1024;

/// The longest voice note or audio file transcribed, in seconds, as the
/// sender's client reports it. A voice note this long is about 1.2 MB.
pub const VOICE_SECONDS: u32 = 5 * 60;

/// The most of Cloudflare's answer that is read. A transcript of
/// [`VOICE_SECONDS`] with its segments fits many times over.
pub const ANSWER_LIMIT: usize = 256 * 1024;

/// How much of Cloudflare's error message an error keeps, in characters.
const DETAIL_CHARS: usize = 200;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(90);

/// A Cloudflare Workers AI client for [`MODEL`]. Deliberately not `Debug`:
/// it holds the API token.
pub struct Whisper {
    client: reqwest::Client,
    endpoint: url::Url,
    auth: HeaderValue,
}

impl Whisper {
    /// `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN`: both, or neither
    /// to leave voice notes off.
    pub fn from_env() -> Result<Option<Self>> {
        whisper(
            std::env::var("CLOUDFLARE_ACCOUNT_ID").ok(),
            std::env::var("CLOUDFLARE_API_TOKEN").ok(),
        )
    }

    /// A client for `account` at `api`, the API's base URL. Production uses
    /// [`CLOUDFLARE_API`]; tests pass a fake server's.
    pub fn new(account: &str, token: &str, api: &str) -> Result<Self> {
        if account.is_empty() || !account.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("CLOUDFLARE_ACCOUNT_ID must be letters and digits only");
        }
        let endpoint = format!(
            "{}/accounts/{account}/ai/run/{MODEL}",
            api.trim_end_matches('/')
        );
        let endpoint = url::Url::parse(&endpoint)
            .with_context(|| format!("the Cloudflare API URL `{api}` is not a URL"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow!("CLOUDFLARE_API_TOKEN holds characters a header cannot"))?;
        auth.set_sensitive(true);
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client with rustls builds");
        Ok(Self {
            client,
            endpoint,
            auth,
        })
    }

    /// The URL transcriptions are posted to.
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    /// The text spoken in `audio`, an OGG voice note or an audio file.
    pub async fn transcribe(&self, audio: Vec<u8>) -> Result<String> {
        if audio.is_empty() {
            bail!("the voice note is empty");
        }
        let encoded = media::base64(&audio);
        drop(audio);
        // Base64 needs no JSON escaping, so the body is built without a copy
        // in a `serde_json::Value`.
        let body = format!(r#"{{"audio":"{encoded}","task":"transcribe"}}"#);
        drop(encoded);
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, self.auth.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status().as_u16();
        let mut answer = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            answer.extend_from_slice(&chunk);
            if answer.len() > ANSWER_LIMIT {
                bail!("Cloudflare's answer is over {ANSWER_LIMIT} bytes");
            }
        }
        transcript(status, &answer)
    }
}

/// The client `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` ask for:
/// none when both are unset or blank, an error when only one is set.
pub fn whisper(account: Option<String>, token: Option<String>) -> Result<Option<Whisper>> {
    let set = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match (set(account), set(token)) {
        (None, None) => Ok(None),
        (Some(account), Some(token)) => Whisper::new(&account, &token, CLOUDFLARE_API).map(Some),
        (Some(_), None) => bail!(
            "CLOUDFLARE_ACCOUNT_ID is set but CLOUDFLARE_API_TOKEN is not; \
             set both to transcribe voice notes, or neither"
        ),
        (None, Some(_)) => bail!(
            "CLOUDFLARE_API_TOKEN is set but CLOUDFLARE_ACCOUNT_ID is not; \
             set both to transcribe voice notes, or neither"
        ),
    }
}

/// A failed call. The URL is left out: it says nothing the operator needs.
fn failed(e: reqwest::Error) -> anyhow::Error {
    anyhow!("calling Cloudflare Workers AI failed: {}", e.without_url())
}

/// The transcript in Cloudflare's answer: HTTP 2xx, `success: true` and a
/// non-blank `result.text`, trimmed.
pub fn transcript(status: u16, answer: &[u8]) -> Result<String> {
    let Ok(body) = serde_json::from_slice::<Value>(answer) else {
        bail!("Cloudflare answered HTTP {status} with something other than JSON");
    };
    if !(200..300).contains(&status) || body["success"] != Value::Bool(true) {
        let detail: String = body["errors"][0]["message"]
            .as_str()
            .map(|m| format!(": {}", m.chars().take(DETAIL_CHARS).collect::<String>()))
            .unwrap_or_default();
        bail!("Cloudflare answered HTTP {status} without success{detail}");
    }
    match body["result"]["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => bail!("Cloudflare's transcription is empty"),
    }
}

/// A fake Workers AI endpoint for unit tests: records each request and
/// answers from a script.
#[cfg(test)]
pub(crate) mod fake {
    use axum::Router;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{Semaphore, watch};

    /// A request as the fake saw it.
    #[derive(Debug, Clone)]
    pub struct Seen {
        pub path: String,
        pub authorization: Option<String>,
        pub body: Value,
    }

    #[derive(Default)]
    struct State {
        seen: Mutex<Vec<Seen>>,
        answers: Mutex<VecDeque<(u16, String)>>,
        /// How many requests have arrived, for waiting on.
        arrived: watch::Sender<usize>,
        /// Requests wait for a permit here once `park` was called.
        gate: Mutex<Option<Arc<Semaphore>>>,
    }

    pub struct FakeCloudflare {
        pub url: String,
        state: Arc<State>,
    }

    /// What the fake answers when nothing is scripted.
    pub fn heard(text: &str) -> String {
        json!({"success": true, "errors": [], "result": {"text": text}}).to_string()
    }

    impl FakeCloudflare {
        pub async fn start() -> Self {
            let state = Arc::new(State::default());
            let shared = state.clone();
            let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
                let state = shared.clone();
                async move { answer(&state, uri, headers, body).await }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self { url, state }
        }

        /// Answer the next request with `status` and `body`.
        pub fn answer(&self, status: u16, body: &str) {
            let next = (status, body.to_string());
            self.state.answers.lock().unwrap().push_back(next);
        }

        /// Hold every request from now on until [`FakeCloudflare::release`].
        pub fn park(&self) {
            *self.state.gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
        }

        /// Let `n` held requests through.
        pub fn release(&self, n: usize) {
            let gate = self.state.gate.lock().unwrap().clone().unwrap();
            gate.add_permits(n);
        }

        pub fn seen(&self) -> Vec<Seen> {
            self.state.seen.lock().unwrap().clone()
        }

        /// Wait until `n` requests have arrived.
        pub async fn arrived(&self, n: usize) {
            // No branch on whether they are already here: `wait_for` checks
            // the current count first, so the test reads the same either way.
            let mut count = self.state.arrived.subscribe();
            let wait = count.wait_for(|&arrived| arrived >= n);
            tokio::time::timeout(std::time::Duration::from_secs(10), wait)
                .await
                .expect("the requests arrive")
                .expect("the fake is running");
        }
    }

    async fn answer(
        state: &State,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, String) {
        let seen = Seen {
            path: uri.path().to_string(),
            authorization: headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string()),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        };
        state.seen.lock().unwrap().push(seen);
        state.arrived.send_modify(|arrived| *arrived += 1);
        let gate = state.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        let (status, body) = state
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| (200, heard("hello there")));
        (StatusCode::from_u16(status).unwrap(), body)
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeCloudflare, heard};
    use super::*;

    const TOKEN: &str = "cf-test-token-not-real";

    #[test]
    fn voice_is_off_without_either_setting_and_refused_with_only_one() {
        assert!(whisper(None, None).unwrap().is_none());
        assert!(
            whisper(Some(" ".into()), Some("".into()))
                .unwrap()
                .is_none()
        );
        let err = whisper(Some("abc".into()), None).err().unwrap().to_string();
        assert!(err.contains("CLOUDFLARE_API_TOKEN is not"), "{err}");
        let err = whisper(None, Some(TOKEN.into())).err().unwrap().to_string();
        assert!(err.contains("CLOUDFLARE_ACCOUNT_ID is not"), "{err}");
    }

    #[test]
    fn both_settings_point_at_cloudflares_whisper_for_that_account() {
        let w = whisper(Some(" abc123 ".into()), Some(format!(" {TOKEN}\n")))
            .unwrap()
            .unwrap();
        assert_eq!(
            w.endpoint(),
            "https://api.cloudflare.com/client/v4/accounts/abc123/ai/run/@cf/openai/whisper-large-v3-turbo"
        );
        // The trimmed token, in a header that never prints.
        assert_eq!(w.auth.to_str().unwrap(), format!("Bearer {TOKEN}"));
        assert!(w.auth.is_sensitive());
        assert!(!format!("{:?}", w.auth).contains(TOKEN));
    }

    #[test]
    fn a_bad_account_token_or_url_fails_at_startup_without_echoing_the_token() {
        for account in ["a/b", "../x", "ab cd", "é"] {
            let err = Whisper::new(account, TOKEN, CLOUDFLARE_API).err().unwrap();
            assert!(err.to_string().contains("letters and digits"), "{err}");
        }
        let err = Whisper::new("abc", "to\u{7f}ken", CLOUDFLARE_API)
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "CLOUDFLARE_API_TOKEN holds characters a header cannot"
        );
        let err = Whisper::new("abc", TOKEN, "not a url").err().unwrap();
        assert!(err.to_string().contains("is not a URL"), "{err}");
        assert!(!err.to_string().contains(TOKEN));
    }

    #[test]
    fn the_transcript_is_the_trimmed_text_of_a_successful_answer() {
        assert_eq!(
            transcript(200, heard("  hi there \n").as_bytes()).unwrap(),
            "hi there"
        );
    }

    #[test]
    fn an_unsuccessful_or_empty_answer_is_an_error_naming_the_status_and_cause() {
        let failed =
            r#"{"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}"#;
        for (status, body, expected) in [
            (
                401,
                failed,
                "Cloudflare answered HTTP 401 without success: Authentication error",
            ),
            (
                200,
                failed,
                "Cloudflare answered HTTP 200 without success: Authentication error",
            ),
            (
                500,
                r#"{"success": true, "result": {"text": "hi"}}"#,
                "Cloudflare answered HTTP 500 without success",
            ),
            (
                200,
                r#"{"result": {"text": "hi"}}"#,
                "Cloudflare answered HTTP 200 without success",
            ),
            (
                502,
                "<html>bad gateway</html>",
                "Cloudflare answered HTTP 502 with something other than JSON",
            ),
            (
                200,
                r#"{"success": true, "result": {"text": "  "}}"#,
                "Cloudflare's transcription is empty",
            ),
            (
                200,
                r#"{"success": true, "result": {}}"#,
                "Cloudflare's transcription is empty",
            ),
            (
                200,
                r#"{"success": true}"#,
                "Cloudflare's transcription is empty",
            ),
        ] {
            let err = transcript(status, body.as_bytes()).unwrap_err().to_string();
            assert_eq!(err, expected, "{status} {body}");
        }
    }

    #[test]
    fn a_long_error_message_is_cut_on_a_character_boundary() {
        let message = "é".repeat(DETAIL_CHARS + 50);
        let body = json_error(&message);
        let err = transcript(400, body.as_bytes()).unwrap_err().to_string();
        let kept = err.split_once(": ").unwrap().1;
        assert_eq!(kept, "é".repeat(DETAIL_CHARS));
    }

    fn json_error(message: &str) -> String {
        serde_json::json!({"success": false, "errors": [{"message": message}]}).to_string()
    }

    #[tokio::test]
    async fn a_note_is_posted_as_base64_with_the_token_and_its_text_returned() {
        let cf = FakeCloudflare::start().await;
        cf.answer(200, &heard(" log my squat "));
        let w = Whisper::new("acct1", TOKEN, &format!("{}/client/v4/", cf.url)).unwrap();

        let text = w.transcribe(b"OggS voice".to_vec()).await.unwrap();

        assert_eq!(text, "log my squat");
        let seen = cf.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].path,
            "/client/v4/accounts/acct1/ai/run/@cf/openai/whisper-large-v3-turbo"
        );
        assert_eq!(
            seen[0].authorization.as_deref(),
            Some(&*format!("Bearer {TOKEN}"))
        );
        assert_eq!(
            seen[0].body,
            serde_json::json!({"audio": media::base64(b"OggS voice"), "task": "transcribe"})
        );
    }

    #[tokio::test]
    async fn failures_never_carry_the_token_or_the_audio() {
        let cf = FakeCloudflare::start().await;
        let w = Whisper::new("acct1", TOKEN, &cf.url).unwrap();
        let mut errors = Vec::new();

        let err = w.transcribe(Vec::new()).await.unwrap_err().to_string();
        assert_eq!(err, "the voice note is empty");
        assert!(cf.seen().is_empty());
        errors.push(err);

        cf.answer(403, &json_error("Authentication error"));
        let err = w.transcribe(b"secret words".to_vec()).await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Cloudflare answered HTTP 403 without success: Authentication error"
        );
        errors.push(err.to_string());

        cf.answer(200, &"x".repeat(ANSWER_LIMIT + 1));
        let err = w.transcribe(b"secret words".to_vec()).await.unwrap_err();
        assert_eq!(err.to_string(), "Cloudflare's answer is over 262144 bytes");
        errors.push(err.to_string());

        // Nothing listens here.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let w = Whisper::new("acct1", TOKEN, &url).unwrap();
        let err = format!(
            "{:#}",
            w.transcribe(b"secret words".to_vec()).await.unwrap_err()
        );
        let failed = "calling Cloudflare Workers AI failed: ";
        assert!(err.starts_with(failed), "{err}");
        assert!(!err.contains("acct1"), "{err}");
        errors.push(err);

        for err in errors {
            assert!(!err.contains(TOKEN), "{err}");
            assert!(!err.contains(&media::base64(b"secret words")), "{err}");
        }
    }
}
