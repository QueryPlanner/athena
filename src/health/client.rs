//! The HTTP side of Google Health: OAuth token calls and Health API reads.
//!
//! Every request has a connect and a total timeout, follows no redirect (so
//! a header or form can never be carried to another host), and has its
//! response read in chunks up to a cap. Tokens and the client secret travel
//! only in a header marked sensitive or in a form body. An [`Error`] holds a
//! status and Google's short error code at most, never a URL, a body, a
//! token or a code.
use super::catalog::{self, Filter};
use super::{Config, Secret};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue};
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(30);
/// The most of a token call's answer that is read.
pub const TOKEN_BODY_LIMIT: usize = 64 * 1024;
/// The most of one page of data points that is read. The bot runs under
/// `MemoryMax=128M`: a page is held, parsed and dropped before the next.
pub const PAGE_BODY_LIMIT: usize = 2 * 1024 * 1024;
pub use super::catalog::{MAX_PAGES, PAGE_SIZE};
/// The longest error code from Google that is kept.
const CODE_CHARS: usize = 64;

/// A failed call, safe to show: it never holds a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Google no longer accepts the refresh token (`invalid_grant`).
    Revoked,
    /// The access token was refused (HTTP 401).
    Unauthorized,
    /// This data type is not allowed to this grant (HTTP 403).
    Forbidden,
    Http {
        status: u16,
        code: Option<String>,
    },
    /// The request did not complete.
    Transport(&'static str),
    /// Google answered with something unusable.
    Malformed(&'static str),
    /// More than this build reads.
    TooMuch(&'static str),
    /// The points read could not be saved.
    Storage(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Revoked => f.write_str("Google no longer accepts the stored authorisation"),
            Error::Unauthorized => f.write_str("Google refused the access token"),
            Error::Forbidden => f.write_str("Google refused access to that data"),
            Error::Http {
                status,
                code: Some(code),
            } => write!(f, "Google answered HTTP {status} ({code})"),
            Error::Http { status, code: None } => write!(f, "Google answered HTTP {status}"),
            Error::Transport(why)
            | Error::Malformed(why)
            | Error::TooMuch(why)
            | Error::Storage(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

/// Where each call goes. Production uses [`Endpoints::production`]; tests
/// pass a fake server's.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub token: String,
    pub revoke: String,
    pub api: String,
}

impl Endpoints {
    pub fn production() -> Self {
        Self {
            token: super::TOKEN_URL.into(),
            revoke: super::REVOKE_URL.into(),
            api: super::API_BASE.into(),
        }
    }

    /// All three on one base URL, as a fake server answers them: `/token`,
    /// `/revoke` and `/v4/...`.
    pub fn at(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            token: format!("{base}/token"),
            revoke: format!("{base}/revoke"),
            api: base.into(),
        }
    }
}

/// What Google's token endpoint returned.
pub struct Tokens {
    pub access: Secret,
    /// Seconds the access token lasts.
    pub expires_in: i64,
    /// Present on a code exchange, and when Google rotates the token.
    pub refresh: Option<Secret>,
    pub scopes: Vec<String>,
}

/// One page of data points.
pub struct Page {
    pub points: Vec<Value>,
    pub next: Option<String>,
}

/// The days a sync reads, in the user's time: whole local days from
/// `start_date` up to but not including `end_date`, and the same span as
/// instants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: String,
    pub end: String,
    pub start_date: String,
    pub end_date: String,
}

/// The Google client. Not `Debug`: it holds the client secret.
pub struct Google {
    http: reqwest::Client,
    client_id: String,
    client_secret: Secret,
    redirect: String,
    endpoints: Endpoints,
}

impl Google {
    pub fn new(config: &Config, endpoints: Endpoints) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client with rustls builds");
        Self {
            http,
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            redirect: config.redirect.as_str().into(),
            endpoints,
        }
    }

    /// Swap an authorisation `code` (and the PKCE `verifier` that goes with
    /// it) for tokens.
    pub async fn exchange_code(&self, code: &Secret, verifier: &Secret) -> Result<Tokens, Error> {
        self.token_call(&[
            ("grant_type", "authorization_code"),
            ("code", code.expose()),
            ("code_verifier", verifier.expose()),
            ("redirect_uri", &self.redirect),
        ])
        .await
    }

    /// A new access token for `refresh`.
    pub async fn refresh(&self, refresh: &Secret) -> Result<Tokens, Error> {
        self.token_call(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.expose()),
        ])
        .await
    }

    async fn token_call(&self, form: &[(&str, &str)]) -> Result<Tokens, Error> {
        let mut pairs = vec![
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.expose()),
        ];
        pairs.extend_from_slice(form);
        let request = self.http.post(&self.endpoints.token).form(&pairs);
        let body = answer(request, TOKEN_BODY_LIMIT, true).await?;
        let access = body["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or(Error::Malformed(
                "Google's token answer has no access token",
            ))?;
        Ok(Tokens {
            access: Secret::new(access),
            expires_in: body["expires_in"].as_i64().unwrap_or(3600),
            refresh: body["refresh_token"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(Secret::new),
            scopes: body["scope"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        })
    }

    /// Ask Google to revoke `refresh`. The token goes in the body, not the
    /// URL.
    pub async fn revoke(&self, refresh: &Secret) -> Result<(), Error> {
        let request = self
            .http
            .post(&self.endpoints.revoke)
            .form(&[("token", refresh.expose())]);
        send(request, TOKEN_BODY_LIMIT).await.map(drop)
    }

    /// One page of `data_type` for `window`, from `page` on.
    pub async fn data_page(
        &self,
        access: &Secret,
        data_type: &str,
        window: &Window,
        page: Option<&str>,
    ) -> Result<Page, Error> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", access.expose()))
            .map_err(|_| Error::Malformed("the access token is not a header value"))?;
        auth.set_sensitive(true);
        let mut query = vec![
            ("pageSize", catalog::of(data_type).page_size.to_string()),
            ("filter", filter(data_type, window)),
        ];
        query.extend(page.map(|token| ("pageToken", token.to_string())));
        let request = self
            .http
            .get(format!(
                "{}/v4/users/me/dataTypes/{data_type}/dataPoints",
                self.endpoints.api
            ))
            .header(AUTHORIZATION, auth)
            .header(ACCEPT, "application/json")
            .query(&query);
        let body = answer(request, PAGE_BODY_LIMIT, false).await?;
        Ok(Page {
            points: body["dataPoints"].as_array().cloned().unwrap_or_default(),
            next: body["nextPageToken"]
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        })
    }
}

/// Send `request` and read the answer up to `limit`; an HTTP error is an
/// [`Error`] named by status and Google's error code.
async fn send(request: reqwest::RequestBuilder, limit: usize) -> Result<(u16, Vec<u8>), Error> {
    // The reason is left out: reqwest's text can name the URL.
    let fail = |_: reqwest::Error| Error::Transport("the request to Google failed or timed out");
    let mut response = request.send().await.map_err(fail)?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(fail)? {
        if body.len() + chunk.len() > limit {
            return Err(Error::TooMuch(
                "Google's answer is larger than this build reads",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    if !(200..300).contains(&status) {
        return Err(refused(status, &body));
    }
    Ok((status, body))
}

/// [`send`], with the body parsed as a JSON object.
async fn answer(
    request: reqwest::RequestBuilder,
    limit: usize,
    token_call: bool,
) -> Result<Value, Error> {
    let (_, body) = send(request, limit).await.map_err(|e| match e {
        // Only the token endpoint says `invalid_grant`: it means the user
        // revoked access or the token expired.
        Error::Http { code, .. } if token_call && code.as_deref() == Some("invalid_grant") => {
            Error::Revoked
        }
        other => other,
    })?;
    match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(Error::Malformed("Google's answer is not a JSON object")),
    }
}

/// The error for an HTTP failure.
fn refused(status: u16, body: &[u8]) -> Error {
    match status {
        401 => Error::Unauthorized,
        403 => Error::Forbidden,
        _ => Error::Http {
            status,
            code: error_code(body),
        },
    }
}

/// Google's short error code in `body`: OAuth's `error` string, or the API's
/// `error.status`. Nothing free-form: only a short word.
fn error_code(body: &[u8]) -> Option<String> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let code = body["error"]
        .as_str()
        .or_else(|| body["error"]["status"].as_str())?;
    let word = !code.is_empty()
        && code.len() <= CODE_CHARS
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    word.then(|| code.to_string())
}

/// The filter that selects `data_type` within `window`. The field depends
/// on how the type is placed in time ([`Filter`]). The type's name is in
/// snake case, as the API requires: a hyphen is `INVALID_DATA_POINT_FILTER`.
/// Session types and daily types use civil dates; the others, instants.
pub fn filter(data_type: &str, window: &Window) -> String {
    let field = data_type.replace('-', "_");
    let (start, end) = (&window.start, &window.end);
    let (start_date, end_date) = (&window.start_date, &window.end_date);
    match catalog::of(data_type).filter {
        Filter::DailyDate => {
            format!(r#"{field}.date >= "{start_date}" AND {field}.date < "{end_date}""#)
        }
        Filter::SessionCivilStart => format!(
            r#"{field}.interval.civil_start_time >= "{start_date}" AND {field}.interval.civil_start_time < "{end_date}""#
        ),
        Filter::SamplePhysical => format!(
            r#"{field}.sample_time.physical_time >= "{start}" AND {field}.sample_time.physical_time < "{end}""#
        ),
        Filter::SleepEnd => {
            format!(r#"sleep.interval.end_time >= "{start}" AND sleep.interval.end_time < "{end}""#)
        }
        // Only `>=` on the start is supported; nothing is later than the
        // window's end anyway.
        Filter::EcgStart => format!(r#"electrocardiogram.interval.start_time >= "{start}""#),
        Filter::IntervalStart => format!(
            r#"{field}.interval.start_time >= "{start}" AND {field}.interval.start_time < "{end}""#
        ),
    }
}

#[cfg(test)]
mod tests;
