//! The sign-in pages: `/browser/{token}` shows a session's browser to
//! whoever opens the link, and passes their taps and typing to it
//! ([`crate::sandbox::login`]).
//!
//! The token in the path is the only credential. These routes take no
//! `X-Athena-User`, because a link opened on a phone cannot send one; they
//! still answer only the allowed hosts. Spans name the route, never the
//! token, and request bodies (what the user types) are not recorded.

use super::{ApiError, App, allowed_host, field};
use crate::sandbox::login::Link;
use crate::sandbox::tools::web_url;
use crate::sandbox::{self, Sandboxes};
use axum::Router;
use axum::body::Bytes;
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, header, request::Parts};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use std::sync::Arc;

/// The page itself: a screenshot to tap on and a few controls.
const PAGE: &str = include_str!("viewer.html");
/// The most text typed at once, in characters.
const MAX_TEXT: usize = 1000;
/// The largest coordinate a tap may name, in CSS pixels.
const MAX_COORDINATE: u64 = 10_000;
/// The most fields one submit fills.
const MAX_FILLS: usize = 20;
/// How far one scroll moves, in pixels.
const SCROLL_PX: &str = "400";

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/browser/{token}", get(page))
        .route("/browser/{token}/start", post(start))
        .route("/browser/{token}/screen", get(screen))
        .route("/browser/{token}/click", post(click))
        .route("/browser/{token}/type", post(type_text))
        .route("/browser/{token}/press", post(press))
        .route("/browser/{token}/scroll", post(scroll))
        .route("/browser/{token}/open", post(open))
        .route("/browser/{token}/controls", get(controls))
        .route("/browser/{token}/submit", post(submit))
        .route("/browser/{token}/done", post(done))
}

/// A link that has not expired, and the sandboxes to drive its browser.
struct Viewer {
    sandboxes: Arc<Sandboxes>,
    link: Link,
}

fn unknown_link() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        code: "not_found",
        message: "this sign-in link is unknown or has expired; ask Athena for a new one".into(),
    }
}

/// What went wrong in the sandbox. The user of a sign-in page is the one
/// who can act on it, so they are told.
fn sandbox_error(e: sandbox::Error) -> ApiError {
    match e {
        sandbox::Error::Invalid(why) => ApiError::invalid(why),
        other => ApiError {
            status: StatusCode::BAD_GATEWAY,
            code: "sandbox",
            message: other.to_string(),
        },
    }
}

/// The token in `/browser/{token}/...`.
fn token(path: &str) -> &str {
    path.split('/').nth(2).unwrap_or_default()
}

impl FromRequestParts<App> for Viewer {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, ApiError> {
        allowed_host(&app.hosts, &parts.headers)?;
        let sandboxes = app.sandboxes.clone().ok_or_else(unknown_link)?;
        let link = sandboxes
            .linked(token(parts.uri.path()))
            .await
            .map_err(sandbox_error)?
            .ok_or_else(unknown_link)?;
        Ok(Self { sandboxes, link })
    }
}

impl Viewer {
    /// Run agent-browser commands on the link's browser, in order.
    async fn act(&self, steps: &[&[&str]]) -> Result<Json<Value>, ApiError> {
        self.sandboxes
            .browser(&self.link.session_id, steps)
            .await
            .map_err(sandbox_error)?;
        Ok(Json(json!({"ok": true})))
    }
}

async fn page(_: Viewer) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE)).into_response()
}

/// Open the link's start page if the browser shows nothing.
async fn start(viewer: Viewer) -> Result<Json<Value>, ApiError> {
    viewer
        .sandboxes
        .resume(&viewer.link)
        .await
        .map_err(sandbox_error)?;
    Ok(Json(json!({"ok": true})))
}

async fn screen(viewer: Viewer) -> Result<Response, ApiError> {
    let png = viewer
        .sandboxes
        .screen(&viewer.link.session_id)
        .await
        .map_err(sandbox_error)?;
    let headers = [
        (header::CONTENT_TYPE, "image/png"),
        (header::CACHE_CONTROL, "no-store"),
    ];
    Ok((headers, png).into_response())
}

/// A non-negative whole-number field of a JSON object body, as text.
fn coordinate(body: &[u8], name: &str) -> Result<String, ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::invalid(format!("the body must be JSON: {e}")))?;
    match value.get(name).and_then(Value::as_u64) {
        Some(n) if n <= MAX_COORDINATE => Ok(n.to_string()),
        _ => Err(ApiError::invalid(format!(
            "`{name}` must be a whole number of pixels from 0 to {MAX_COORDINATE}"
        ))),
    }
}

/// A tap at `x`, `y` in the screenshot's pixels, which are the page's.
async fn click(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let (x, y) = (coordinate(&body, "x")?, coordinate(&body, "y")?);
    viewer
        .act(&[
            &["mouse", "move", &x, &y],
            &["mouse", "down"],
            &["mouse", "up"],
        ])
        .await
}

/// Text typed into whatever has focus, key by key.
async fn type_text(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let text = field(&body, "text")?;
    if text.is_empty() || text.chars().count() > MAX_TEXT {
        return Err(ApiError::invalid(format!(
            "`text` must be 1 to {MAX_TEXT} characters"
        )));
    }
    viewer.act(&[&["keyboard", "type", &text]]).await
}

/// A key or chord: letters, digits and `+`, such as `Enter` or `Shift+Tab`.
fn checked_key(key: &str) -> Result<&str, ApiError> {
    let ok = (1..=32).contains(&key.len())
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+');
    match ok {
        true => Ok(key),
        false => Err(ApiError::invalid(format!(
            "`{key}` is not a key; use a name like Enter, Tab or Escape"
        ))),
    }
}

async fn press(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let key = field(&body, "key")?;
    viewer.act(&[&["press", checked_key(&key)?]]).await
}

async fn scroll(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let direction = field(&body, "direction")?;
    if !matches!(direction.as_str(), "up" | "down" | "left" | "right") {
        return Err(ApiError::invalid(format!(
            "`{direction}` is not a direction; use up, down, left or right"
        )));
    }
    viewer.act(&[&["scroll", &direction, SCROLL_PX]]).await
}

async fn open(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let url = web_url(&field(&body, "url")?).map_err(sandbox_error)?;
    viewer.act(&[&["open", &url]]).await
}

/// The page's fields and buttons, for the sign-in page to show as its own.
async fn controls(viewer: Viewer) -> Result<Json<Value>, ApiError> {
    let controls = viewer
        .sandboxes
        .controls(&viewer.link.session_id)
        .await
        .map_err(sandbox_error)?;
    Ok(Json(json!({"controls": controls})))
}

/// An element ref from `controls`: `e` and digits.
fn checked_ref(reference: &str) -> Result<&str, ApiError> {
    match reference.strip_prefix('e') {
        Some(n) if (1..=9).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(reference)
        }
        _ => Err(ApiError::invalid(format!(
            "`{reference}` is not a field; use a ref like e4 from controls"
        ))),
    }
}

#[derive(serde::Deserialize)]
struct Fill {
    #[serde(rename = "ref")]
    reference: String,
    text: String,
}

#[derive(serde::Deserialize)]
struct Submit {
    #[serde(default)]
    fills: Vec<Fill>,
    #[serde(default)]
    click: Option<String>,
}

/// What a submit asks for: (ref, text) pairs to fill, then maybe a button.
type Form = (Vec<(String, String)>, Option<String>);

/// `{"fills": [{"ref", "text"}], "click": "e3"}`, checked.
fn submission(body: &[u8]) -> Result<Form, ApiError> {
    let form: Submit = serde_json::from_slice(body).map_err(|e| {
        ApiError::invalid(format!(
            "the body must be {{\"fills\": [{{\"ref\", \"text\"}}], \"click\": ref}}: {e}"
        ))
    })?;
    if form.fills.is_empty() && form.click.is_none() {
        return Err(ApiError::invalid("nothing to fill or click".into()));
    }
    if form.fills.len() > MAX_FILLS {
        return Err(ApiError::invalid(format!(
            "at most {MAX_FILLS} fields at once"
        )));
    }
    let mut fills = Vec::new();
    for fill in form.fills {
        checked_ref(&fill.reference)?;
        if fill.text.chars().count() > MAX_TEXT {
            return Err(ApiError::invalid(format!(
                "a field's text must be at most {MAX_TEXT} characters"
            )));
        }
        fills.push((fill.reference, fill.text));
    }
    if let Some(click) = &form.click {
        checked_ref(click)?;
    }
    Ok((fills, form.click))
}

/// Fill the page's fields with what the user typed into the sign-in
/// page's own, then click a button: one command, so one round trip.
async fn submit(viewer: Viewer, body: Bytes) -> Result<Json<Value>, ApiError> {
    let (fills, click) = submission(&body)?;
    viewer
        .sandboxes
        .submit(&viewer.link.session_id, &fills, click.as_deref())
        .await
        .map_err(sandbox_error)?;
    Ok(Json(json!({"ok": true})))
}

/// Save the browser's sign-ins for the link's user.
async fn done(viewer: Viewer) -> Result<Json<Value>, ApiError> {
    let saved = viewer
        .sandboxes
        .save_login(&viewer.link.session_id)
        .await
        .map_err(sandbox_error)?;
    Ok(Json(json!({"saved_bytes": saved})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_the_second_path_segment() {
        assert_eq!(token("/browser/abc"), "abc");
        assert_eq!(token("/browser/abc/click"), "abc");
        assert_eq!(token("/"), "");
    }

    #[test]
    fn coordinates_are_whole_pixels_in_range() {
        let body = br#"{"x": 12, "y": 10000, "z": 10001, "w": -1, "v": 1.5}"#;
        assert_eq!(coordinate(body, "x").unwrap(), "12");
        assert_eq!(coordinate(body, "y").unwrap(), "10000");
        for bad in ["z", "w", "v", "missing"] {
            assert_eq!(coordinate(body, bad).unwrap_err().code, "invalid", "{bad}");
        }
        assert!(coordinate(b"not json", "x").is_err());
    }

    #[test]
    fn keys_are_names_not_text() {
        assert_eq!(checked_key("Shift+Tab").unwrap(), "Shift+Tab");
        for bad in ["", "a b", "Enter;", &"k".repeat(33)] {
            assert!(checked_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn refs_are_e_and_digits() {
        assert_eq!(checked_ref("e4").unwrap(), "e4");
        for bad in ["", "e", "@e4", "e4;id", "x4", "e1234567890"] {
            assert!(checked_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_submission_fills_then_clicks_and_is_checked_first() {
        let (fills, click) =
            submission(br#"{"fills":[{"ref":"e4","text":"me@x.example"},{"ref":"e2","text":""}],"click":"e3"}"#)
                .unwrap();
        let expected = [("e4", "me@x.example"), ("e2", "")].map(|(r, t)| (r.into(), t.into()));
        assert_eq!(fills, expected);
        assert_eq!(click.as_deref(), Some("e3"));
        assert!(submission(br#"{"click":"e3"}"#).unwrap().0.is_empty());

        let many: Vec<Value> = (0..=MAX_FILLS)
            .map(|_| json!({"ref": "e1", "text": ""}))
            .collect();
        let long = "x".repeat(MAX_TEXT + 1);
        for bad in [
            json!({}),
            json!({"fills": many}),
            json!({"fills": [{"ref": "e1", "text": long}]}),
            json!({"fills": [{"ref": "@e1", "text": "a"}]}),
            json!({"fills": [{"ref": "e1"}]}),
            json!({"click": "e3; reboot"}),
        ] {
            let err = submission(bad.to_string().as_bytes()).unwrap_err();
            assert_eq!(err.code, "invalid", "{bad}");
        }
    }

    #[test]
    fn sandbox_errors_say_whose_fault_they_are() {
        let bad = sandbox_error(sandbox::Error::Invalid("no".into()));
        assert_eq!((bad.status, bad.code), (StatusCode::BAD_REQUEST, "invalid"));
        let down = sandbox_error(sandbox::Error::Http("refused".into()));
        assert_eq!(
            (down.status, down.code),
            (StatusCode::BAD_GATEWAY, "sandbox")
        );
        assert!(down.message.contains("refused"));
    }
}
