//! Google Health: read-only recovery, sleep and activity data, synced once a
//! day into `health_daily` and `health_points` and read by the model through
//! six tools.
//!
//! How a user connects ("paste back"). Athena has no HTTPS callback, so it
//! reuses the redirect URI already registered for Blacki's Google OAuth client
//! (by default `http://127.0.0.1:8080/integrations/google-health/callback`).
//!
//! 1. `/connect_health` (Telegram, private chat) sends a Google consent link
//!    carrying a random `state`. Only the hash of the `state` is stored,
//!    bound to the user, for ten minutes, usable once ([`STATE_TTL`]).
//! 2. The user approves. The browser lands on the redirect URI, which usually
//!    does not load on a phone. The user copies that URL into Telegram.
//! 3. The bot recognises it ([`find_callback`]) before the message reaches
//!    the model, the transcript or any log, checks the `state`, and swaps the
//!    `code` for a refresh token at Google. The code is never sent to the
//!    model and never stored. It is also useless without the client secret
//!    and the PKCE verifier, which is derived from the secret and the `state`
//!    ([`verifier`]) and so is not in the pasted URL.
//! 4. The refresh token is encrypted ([`Cipher`]) and stored. The access
//!    token lives only in memory ([`sync::Health`]).
//!
//! No token, code or client secret is ever put in an error, a log line or a
//! `Debug` output: they are [`Secret`]s, and every error built from a
//! failed request leaves out the URL and the response body.

pub mod catalog;
pub mod client;
pub mod data;
pub mod export;
pub mod normalize;
pub mod points;
pub mod sync;
#[cfg(test)]
pub(crate) mod testing;
pub mod tools;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use sha2::{Digest, Sha256};
use url::Url;

pub use sync::Health;

/// Google's consent screen.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Where codes and refresh tokens are exchanged.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Where a refresh token is revoked.
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
/// The Google Health API (v4).
pub const API_BASE: &str = "https://health.googleapis.com";
/// The redirect URI Blacki registered with Google.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8080/integrations/google-health/callback";

/// The read-only scopes asked for: no `writeonly` one.
///
/// Every readable kind of data is asked for, sensitive ones included
/// (`location`, `reproductive_health`, `logged_symptoms`, `mindfulness`,
/// `ecg`, `irn`). The user can untick any on Google's consent screen; a data
/// type whose scope was not granted is skipped by the sync.
pub const SCOPES: [&str; 10] = [
    "https://www.googleapis.com/auth/googlehealth.activity_and_fitness.readonly",
    "https://www.googleapis.com/auth/googlehealth.health_metrics_and_measurements.readonly",
    "https://www.googleapis.com/auth/googlehealth.location.readonly",
    "https://www.googleapis.com/auth/googlehealth.nutrition.readonly",
    "https://www.googleapis.com/auth/googlehealth.sleep.readonly",
    "https://www.googleapis.com/auth/googlehealth.reproductive_health.readonly",
    "https://www.googleapis.com/auth/googlehealth.logged_symptoms.readonly",
    "https://www.googleapis.com/auth/googlehealth.mindfulness.readonly",
    "https://www.googleapis.com/auth/googlehealth.ecg.readonly",
    "https://www.googleapis.com/auth/googlehealth.irn.readonly",
];

/// How long a consent link works.
pub const STATE_TTL: SignedDuration = SignedDuration::from_mins(10);
/// The least time between two syncs a user asks for.
pub const MANUAL_COOLDOWN: SignedDuration = SignedDuration::from_hours(1);
/// The local time of day the daily sync is due.
pub const SYNC_AT: (i8, i8) = (5, 30);
/// How long after a failed sync the next try is made.
pub const RETRY_AFTER: SignedDuration = SignedDuration::from_hours(1);
/// Days synced on each run, today included.
pub const WINDOW_DAYS: i64 = 14;
/// Days of history fetched in one backfill chunk, per data type.
pub const BACKFILL_DAYS: i64 = 7;
/// Chunks fetched per data type in each daily pass.
pub const BACKFILL_CHUNKS_PER_PASS: usize = 3;
/// How far back history is looked for (Google sets no limit): three years.
pub const BACKFILL_FLOOR_DAYS: i64 = 3 * 365;
/// A data type is done after this many empty chunks in a row (12 weeks).
pub const BACKFILL_EMPTY_CHUNKS: i64 = 12;

/// The names of the tools. `agent::reserved_tool_names` includes them
/// whether or not Google Health is configured.
pub const NAMES: [&str; 6] = [
    "health_status",
    "health_summary",
    "health_sync_now",
    data::SIZE,
    data::POINTS,
    data::EXPORT,
];

/// A token, code or client secret. It has no `Display`, and its `Debug`
/// shows nothing.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself, for the one place that must send it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// The validated settings. Deliberately not `Debug`-printable with values.
pub struct Config {
    pub client_id: String,
    pub client_secret: Secret,
    pub redirect: Url,
    key: Secret,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("redirect", &self.redirect.as_str())
            .finish_non_exhaustive()
    }
}

impl Config {
    /// `GOOGLE_HEALTH_CLIENT_ID`, `GOOGLE_HEALTH_CLIENT_SECRET` and
    /// `GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY`: all three, or none to leave
    /// Google Health off. `GOOGLE_HEALTH_REDIRECT_URI` is optional.
    pub fn from_env() -> Result<Option<Self>> {
        let var = |name: &str| std::env::var(name).ok();
        config(
            var("GOOGLE_HEALTH_CLIENT_ID"),
            var("GOOGLE_HEALTH_CLIENT_SECRET"),
            var("GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY"),
            var("GOOGLE_HEALTH_REDIRECT_URI"),
        )
    }

    /// The cipher for the stored refresh tokens.
    pub fn cipher(&self) -> Cipher {
        let key = decode_key(self.key.expose()).expect("the key was checked in `config`");
        Cipher::new(&key)
    }

    /// The Google consent link for `state`.
    pub fn authorization_url(&self, state: &str) -> String {
        let mut url = Url::parse(AUTH_URL).expect("the consent URL is a URL");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", self.redirect.as_str())
            .append_pair("response_type", "code")
            .append_pair("access_type", "offline")
            .append_pair("include_granted_scopes", "true")
            .append_pair("prompt", "consent")
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", &challenge(&verifier(self, state)))
            .append_pair("code_challenge_method", "S256");
        url.into()
    }
}

/// The settings the four variables ask for: none when the three required
/// ones are unset or blank, an error naming (never showing) what is missing
/// or invalid otherwise.
pub fn config(
    id: Option<String>,
    secret: Option<String>,
    key: Option<String>,
    redirect: Option<String>,
) -> Result<Option<Config>> {
    let set = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (id, secret, key) = (set(id), set(secret), set(key));
    if id.is_none() && secret.is_none() && key.is_none() {
        return Ok(None);
    }
    let missing: Vec<&str> = [
        ("GOOGLE_HEALTH_CLIENT_ID", id.is_none()),
        ("GOOGLE_HEALTH_CLIENT_SECRET", secret.is_none()),
        ("GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY", key.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, absent)| absent.then_some(name))
    .collect();
    if !missing.is_empty() {
        bail!(
            "Google Health is partly configured: {} not set; set all three, or none",
            missing.join(", ")
        );
    }
    let (id, secret, key) = (id.unwrap(), secret.unwrap(), key.unwrap());
    decode_key(&key)?;
    let redirect = set(redirect).unwrap_or_else(|| DEFAULT_REDIRECT_URI.into());
    Ok(Some(Config {
        client_id: id,
        client_secret: Secret::new(secret),
        redirect: redirect_uri(&redirect)?,
        key: Secret::new(key),
    }))
}

/// `GOOGLE_HEALTH_REDIRECT_URI`: HTTPS, or HTTP to this machine, with no
/// query or fragment.
fn redirect_uri(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).context("GOOGLE_HEALTH_REDIRECT_URI is not a URL")?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    let allowed = url.scheme() == "https" || (url.scheme() == "http" && local);
    if !allowed || url.host_str().is_none() {
        bail!("GOOGLE_HEALTH_REDIRECT_URI must be https, or http on 127.0.0.1 or localhost");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("GOOGLE_HEALTH_REDIRECT_URI must have no query or fragment");
    }
    Ok(url)
}

/// The 32 key bytes in `GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY`: URL-safe
/// base64, as a Fernet key is, padded or not.
fn decode_key(text: &str) -> Result<[u8; 32]> {
    let bytes = URL_SAFE
        .decode(text)
        .or_else(|_| URL_SAFE_NO_PAD.decode(text));
    bytes
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .context("GOOGLE_HEALTH_TOKEN_ENCRYPTION_KEY must be URL-safe base64 of exactly 32 bytes")
}

/// Seals refresh tokens with XChaCha20-Poly1305.
///
/// The stored form is one version byte (1), a random 24-byte nonce, then the
/// ciphertext with its tag. The user's id is authenticated but not stored, so
/// a token cannot be moved to another user's row.
pub struct Cipher(XChaCha20Poly1305);

const VERSION: u8 = 1;
const NONCE: usize = 24;

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(XChaCha20Poly1305::new(key.into()))
    }

    pub fn seal(&self, user: i64, token: &str) -> Vec<u8> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = user.to_le_bytes();
        let sealed = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: token.as_bytes(),
                    aad: &aad,
                },
            )
            .expect("sealing a short token cannot fail");
        let mut out = vec![VERSION];
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out
    }

    /// The token in `sealed`, or why it cannot be read: a different key, a
    /// different user, damage, or a newer format. Never the contents.
    pub fn open(&self, user: i64, sealed: &[u8]) -> Result<Secret> {
        let [VERSION, rest @ ..] = sealed else {
            bail!("the stored token is in a format this build does not know");
        };
        if rest.len() <= NONCE {
            bail!("the stored token is too short");
        }
        let (nonce, body) = rest.split_at(NONCE);
        let aad = user.to_le_bytes();
        let plain = self
            .0
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("the stored token cannot be decrypted with this key"))?;
        let token = String::from_utf8(plain).context("the stored token is not text")?;
        Ok(Secret::new(token))
    }
}

/// A fresh `state`: about 240 bits from the system's random source.
pub fn new_state() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// What is stored in place of a `state`.
pub fn hash_state(state: &str) -> String {
    format!("{:x}", Sha256::digest(state.as_bytes()))
}

/// The PKCE verifier for `state`: derived from the client secret, so it is
/// never stored and never in the URL the user pastes.
pub fn verifier(config: &Config, state: &str) -> Secret {
    let mut hash = Sha256::new();
    hash.update(b"athena google health pkce\0");
    hash.update(config.client_secret.expose().as_bytes());
    hash.update([0]);
    hash.update(state.as_bytes());
    Secret::new(format!("{:x}", hash.finalize()))
}

/// The S256 challenge for a verifier.
pub fn challenge(verifier: &Secret) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.expose().as_bytes()))
}

/// What a pasted callback URL carried.
#[derive(Clone)]
pub struct Callback {
    pub code: Option<Secret>,
    pub state: Option<String>,
    /// Google's `error`, if the user declined.
    pub error: Option<String>,
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback").finish_non_exhaustive()
    }
}

/// Decode `%XX` escapes, leaving anything malformed as it is.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        let decoded = match bytes[i..] {
            [b'%', hi, lo, ..] => hex(hi).zip(hex(lo)).map(|(h, l)| (h * 16 + l) as u8),
            _ => None,
        };
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The callback URL in `text`, if it holds one: the first word (as split by
/// whitespace, minus the brackets and quotes around it) that has the redirect
/// URI's host and path in it, or that has both a `code=` and a `state=`
/// parameter, as the URL itself or percent-encoded. `redirect` is `None` when
/// Google Health is not configured: the second rule alone still keeps a
/// pasted code from the model.
///
/// A message this finds is never a prompt. A bare code, without its URL,
/// cannot be told from a word.
pub fn find_callback(text: &str, redirect: Option<&Url>) -> Option<Callback> {
    let needle = redirect.map(|u| {
        let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        format!("{}{port}{}", u.host_str().unwrap_or_default(), u.path())
    });
    text.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c| "()<>[]{}\"'`,;".contains(c));
        let plain = unescape(word);
        let by_path = needle.as_deref().is_some_and(|n| plain.contains(n));
        let by_params = plain.contains("code=") && plain.contains("state=");
        (by_path || by_params).then(|| callback(word, &plain))
    })
}

/// The parameters in the query of `word`, read once from the raw word (a
/// code holds `%2F`), or from its decoded form if the `?` was encoded.
fn callback(word: &str, plain: &str) -> Callback {
    let query = word
        .split_once('?')
        .or_else(|| plain.split_once('?'))
        .map_or("", |(_, query)| query);
    let query = query.split('#').next().unwrap_or_default();
    let mut found = Callback {
        code: None,
        state: None,
        error: None,
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let value = value.into_owned();
        match &*key {
            "code" => found.code = Some(Secret::new(value)),
            "state" => found.state = Some(value),
            "error" => found.error = Some(value),
            _ => {}
        }
    }
    found
}

/// Whether the daily sync is due for a user at `now`: it is past
/// [`SYNC_AT`] on their clock and nothing was attempted since; or the last
/// attempt failed and [`RETRY_AFTER`] has passed.
pub fn due(now: Timestamp, last_attempt: Option<Timestamp>, failed: bool, zone: &TimeZone) -> bool {
    let today = now.to_zoned(zone.clone()).date();
    // A time that does not exist on that day (a clock change) moves later.
    let at = today
        .at(SYNC_AT.0, SYNC_AT.1, 0, 0)
        .to_zoned(zone.clone())
        .expect("a real date has a 05:30")
        .timestamp();
    match last_attempt {
        None => now >= at,
        Some(last) if failed && now.duration_since(last) >= RETRY_AFTER => true,
        Some(last) => now >= at && last < at,
    }
}

#[cfg(test)]
mod tests;
