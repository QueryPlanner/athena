//! A sandbox per session, on an OpenSandbox server, for the agent's tools.
//!
//! Tools never run on Athena's host. The first tool call in a session
//! creates a sandbox for it; later calls reuse it while it lives. Every use
//! pushes its expiry `timeout` into the future, and a sandbox the server no
//! longer knows (it expired, or someone deleted it) is replaced by a fresh
//! one. The sandbox, its persistent bash session and its code-interpreter
//! context are recorded in the `sandboxes` table, so a restarted process or
//! a second process picks up the same ones.
//!
//! Configuration comes from the environment (see [`Config::from_env`]);
//! without `OPEN_SANDBOX_URL` there is no sandbox and no sandbox tools.
//!
//! Accepted risk, until the sandbox host is hardened: sandboxes run as root
//! with internet egress, and the server takes requests from any tailnet
//! device. Athena passes none of its own secrets into a sandbox.

pub mod client;
pub mod shell;
pub mod stream;
pub mod tools;

use crate::store::{SandboxRow, Store, now_millis};
use client::{Client, CreateSpec, Execd};
use reqwest::header::HeaderValue;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use stream::Output;
use tokio::sync::OwnedMutexGuard;
use url::Url;

pub const DEFAULT_IMAGE: &str = "ghcr.io/queryplanner/athena-sandbox:latest";
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
/// OpenSandbox's own minimum sandbox timeout.
pub const MIN_TIMEOUT_SECS: u64 = 60;

/// Starts the image's `athena-sandbox-start` (which starts the Jupyter
/// server `run_code` needs) when the image has it, and otherwise just keeps
/// the sandbox alive, so any image works.
const ENTRYPOINT: [&str; 3] = [
    "/bin/sh",
    "-c",
    "if command -v athena-sandbox-start >/dev/null 2>&1; \
     then exec athena-sandbox-start; else exec tail -f /dev/null; fi",
];
/// Where `athena-sandbox-start` runs Jupyter, for execd's code interpreter.
const JUPYTER_HOST: &str = "http://127.0.0.1:44771";

/// Why a sandbox operation failed. Tools report it to the model.
#[derive(Debug)]
pub enum Error {
    /// The environment's sandbox settings are unusable.
    Config(String),
    /// The tool's input is unusable: a bad URL or a NUL byte, say.
    Invalid(String),
    /// The server could not be reached, or the connection failed.
    Http(String),
    /// The server answered with an error status.
    Status {
        what: String,
        status: u16,
        body: String,
    },
    /// The server answered with something this client does not understand.
    Protocol(String),
    /// Athena's database failed.
    Store(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(why) => write!(f, "sandbox configuration: {why}"),
            Self::Invalid(why) => write!(f, "{why}"),
            Self::Http(why) => write!(f, "sandbox server unreachable: {why}"),
            Self::Status { what, status, body } => {
                write!(f, "sandbox server refused to {what}: HTTP {status}: {body}")
            }
            Self::Protocol(why) => write!(f, "sandbox server: {why}"),
            Self::Store(why) => write!(f, "storage: {why}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self::Store(format!("{e:#}"))
    }
}

/// Where the sandboxes come from and how long they live.
#[derive(Debug, Clone)]
pub struct Config {
    /// The OpenSandbox server, e.g. `http://100.118.54.67:9090`.
    pub url: Url,
    /// Sent as `OPEN-SANDBOX-API-KEY` when set.
    pub api_key: Option<HeaderValue>,
    pub image: String,
    /// How long an unused sandbox lives. Every use starts it again.
    pub timeout: Duration,
    /// `ATHENA_ENV`, recorded on each sandbox so a shared server can tell
    /// staging's sandboxes from prod's.
    pub env: String,
    pub cpu: String,
    pub memory: String,
    /// How long to wait for a new sandbox to reach `Running`, and how often
    /// to look.
    pub startup_timeout: Duration,
    pub startup_poll: Duration,
}

impl Config {
    /// The sandbox settings in the environment, or `None` without
    /// `OPEN_SANDBOX_URL`.
    pub fn from_env() -> Result<Option<Self>, Error> {
        let var = |name| std::env::var(name).ok();
        Self::parse(
            var("OPEN_SANDBOX_URL"),
            var("OPEN_SANDBOX_API_KEY"),
            var("ATHENA_SANDBOX_IMAGE"),
            var("ATHENA_SANDBOX_TIMEOUT_SECS"),
            var("ATHENA_ENV"),
        )
    }

    pub(crate) fn parse(
        url: Option<String>,
        api_key: Option<String>,
        image: Option<String>,
        timeout_secs: Option<String>,
        env: Option<String>,
    ) -> Result<Option<Self>, Error> {
        let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        let Some(url) = set(url) else {
            return Ok(None);
        };
        let url = Url::parse(url.trim())
            .map_err(|e| Error::Config(format!("OPEN_SANDBOX_URL `{url}`: {e}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Config(
                "OPEN_SANDBOX_URL must be http or https".into(),
            ));
        }
        let api_key = match set(api_key) {
            Some(key) => {
                let mut value = HeaderValue::try_from(key.trim()).map_err(|_| {
                    Error::Config("OPEN_SANDBOX_API_KEY is not a valid header value".into())
                })?;
                value.set_sensitive(true);
                Some(value)
            }
            None => None,
        };
        let timeout = match set(timeout_secs) {
            Some(secs) => secs.trim().parse::<u64>().map_err(|_| {
                Error::Config(format!(
                    "ATHENA_SANDBOX_TIMEOUT_SECS `{secs}` is not a number"
                ))
            })?,
            None => DEFAULT_TIMEOUT_SECS,
        };
        if timeout < MIN_TIMEOUT_SECS {
            return Err(Error::Config(format!(
                "ATHENA_SANDBOX_TIMEOUT_SECS must be at least {MIN_TIMEOUT_SECS}"
            )));
        }
        Ok(Some(Self {
            url,
            api_key,
            image: set(image).unwrap_or_else(|| DEFAULT_IMAGE.into()),
            timeout: Duration::from_secs(timeout),
            env: set(env).unwrap_or_else(|| "dev".into()),
            cpu: "500m".into(),
            memory: "1Gi".into(),
            startup_timeout: Duration::from_secs(60),
            startup_poll: Duration::from_millis(250),
        }))
    }
}

/// `ms` since the Unix epoch as an RFC 3339 UTC time, as OpenSandbox's
/// `renew-expiration` wants it.
pub fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A session's sandbox, held for one tool call.
struct Lease {
    row: SandboxRow,
    execd: Execd,
    /// Tool calls on one session take turns: a bash session runs one
    /// command at a time, and two first calls must not both create a sandbox.
    _turn: OwnedMutexGuard<()>,
}

/// Whether execd says the bash session or code context it was asked to use
/// does not exist, as it does after execd restarts. Newer execd answers 404;
/// the execd behind OpenSandbox 0.2.3 answers 500 with "not found" in the
/// message.
fn is_missing(e: &Error) -> bool {
    matches!(e, Error::Status { status, body, .. }
        if *status == 404 || body.to_ascii_lowercase().contains("not found"))
}

/// Every session's sandbox on one OpenSandbox server.
pub struct Sandboxes {
    client: Client,
    store: Store,
    config: Config,
    turns: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Sandboxes {
    pub fn new(config: Config, store: Store) -> Self {
        Self {
            client: Client::new(config.url.clone(), config.api_key.clone()),
            store,
            config,
            turns: Mutex::default(),
        }
    }

    async fn turn(&self, session_id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
            // Drop locks nobody holds or waits for, as `Store` does.
            turns.retain(|_, l| Arc::strong_count(l) > 1);
            turns.entry(session_id.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }

    async fn stored<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, Error> {
        Ok(self.store.call(f).await?)
    }

    async fn save(&self, row: &SandboxRow) -> Result<(), Error> {
        let row = row.clone();
        self.stored(move |s| s.update_sandbox(&row)).await
    }

    /// The session's sandbox, renewed, or a new one.
    async fn lease(&self, session_id: &str) -> Result<Lease, Error> {
        let turn = self.turn(session_id).await;
        let expires_at = now_millis() + self.config.timeout.as_millis() as i64;
        let id = session_id.to_string();
        let row = match self.stored(move |s| s.sandbox(&id)).await? {
            Some(mut row) => {
                if self
                    .client
                    .renew(&row.sandbox_id, &rfc3339(expires_at))
                    .await?
                {
                    row.expires_at = expires_at;
                    self.save(&row).await?;
                    row
                } else {
                    let (id, sandbox) = (row.session_id, row.sandbox_id);
                    self.stored(move |s| s.remove_sandbox(&id, &sandbox))
                        .await?;
                    self.create(session_id, expires_at).await?
                }
            }
            None => self.create(session_id, expires_at).await?,
        };
        let execd = self.client.execd(&row.sandbox_id).await?;
        Ok(Lease {
            row,
            execd,
            _turn: turn,
        })
    }

    async fn create(&self, session_id: &str, expires_at: i64) -> Result<SandboxRow, Error> {
        let id = session_id.to_string();
        let owner = self
            .stored(move |s| s.session_owner(&id))
            .await?
            .ok_or_else(|| Error::Invalid(format!("no session `{session_id}`")))?;
        let spec = CreateSpec {
            image: self.config.image.clone(),
            entrypoint: ENTRYPOINT.iter().map(|s| s.to_string()).collect(),
            timeout_secs: self.config.timeout.as_secs(),
            cpu: self.config.cpu.clone(),
            memory: self.config.memory.clone(),
            env: BTreeMap::from([
                ("JUPYTER_HOST".into(), JUPYTER_HOST.into()),
                (
                    "JUPYTER_TOKEN".into(),
                    uuid::Uuid::new_v4().simple().to_string(),
                ),
            ]),
            metadata: BTreeMap::from([
                ("app".into(), "athena".into()),
                ("env".into(), self.config.env.clone()),
                ("user".into(), owner.to_string()),
                ("session".into(), session_id.into()),
            ]),
        };
        let created = self.client.create(&spec).await?;
        if let Err(e) = self.started(&created.id, created.state).await {
            // Best effort: a sandbox that never started expires by itself.
            let _ = self.client.delete(&created.id).await;
            return Err(e);
        }
        let row = SandboxRow {
            session_id: session_id.into(),
            sandbox_id: created.id,
            bash_session: None,
            code_language: None,
            code_context: None,
            created_at: now_millis(),
            expires_at,
        };
        let new = row.clone();
        if self.stored(move |s| s.insert_sandbox(&new)).await? {
            return Ok(row);
        }
        // Another process recorded a sandbox for this session first. Use
        // theirs, so the session keeps one sandbox, and drop ours.
        let _ = self.client.delete(&row.sandbox_id).await;
        let id = session_id.to_string();
        self.stored(move |s| s.sandbox(&id))
            .await?
            .ok_or_else(|| Error::Store("the session's sandbox row vanished".into()))
    }

    /// Wait for a new sandbox to reach `Running`.
    async fn started(&self, id: &str, mut state: String) -> Result<(), Error> {
        let deadline = tokio::time::Instant::now() + self.config.startup_timeout;
        loop {
            match state.as_str() {
                "Running" => return Ok(()),
                "Pending" | "Resuming" => {}
                other => {
                    return Err(Error::Protocol(format!(
                        "sandbox {id} is {other} instead of starting"
                    )));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Protocol(format!(
                    "sandbox {id} did not start within {:?}",
                    self.config.startup_timeout
                )));
            }
            tokio::time::sleep(self.config.startup_poll).await;
            state = self
                .client
                .state(id)
                .await?
                .unwrap_or_else(|| "gone".into());
        }
    }

    async fn new_bash(&self, lease: &mut Lease) -> Result<String, Error> {
        let bash = lease.execd.create_session().await?;
        lease.row.bash_session = Some(bash.clone());
        self.save(&lease.row).await?;
        Ok(bash)
    }

    /// Run shell text in the session's persistent bash session.
    pub async fn shell(
        &self,
        session_id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let mut lease = self.lease(session_id).await?;
        let bash = match lease.row.bash_session.clone() {
            Some(bash) => bash,
            None => self.new_bash(&mut lease).await?,
        };
        match lease.execd.run_in_session(&bash, command, timeout).await {
            Err(e) if is_missing(&e) => {
                let bash = self.new_bash(&mut lease).await?;
                lease.execd.run_in_session(&bash, command, timeout).await
            }
            done => done,
        }
    }

    async fn new_context(&self, lease: &mut Lease, language: &str) -> Result<String, Error> {
        let context = lease.execd.create_context(language).await?;
        lease.row.code_language = Some(language.into());
        lease.row.code_context = Some(context.clone());
        self.save(&lease.row).await?;
        Ok(context)
    }

    /// Run code in the session's interpreter for `language`. Switching
    /// language starts a new interpreter; the old one's state is gone.
    pub async fn run_code(
        &self,
        session_id: &str,
        language: &str,
        code: &str,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let mut lease = self.lease(session_id).await?;
        let context = match (&lease.row.code_language, &lease.row.code_context) {
            (Some(current), Some(context)) if current == language => context.clone(),
            _ => self.new_context(&mut lease, language).await?,
        };
        match lease
            .execd
            .run_code(&context, language, code, timeout)
            .await
        {
            Err(e) if is_missing(&e) => {
                let context = self.new_context(&mut lease, language).await?;
                lease
                    .execd
                    .run_code(&context, language, code, timeout)
                    .await
            }
            done => done,
        }
    }

    /// Run shell text once, in a fresh shell.
    pub async fn command(
        &self,
        session_id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<Output, Error> {
        let lease = self.lease(session_id).await?;
        lease.execd.command(command, timeout).await
    }

    /// Up to `limit` bytes of a file in the session's sandbox, and whether
    /// the file is longer.
    pub async fn read_file(
        &self,
        session_id: &str,
        path: &str,
        limit: usize,
    ) -> Result<(Vec<u8>, bool), Error> {
        let lease = self.lease(session_id).await?;
        lease.execd.download(path, limit).await
    }

    /// Create or replace a file in the session's sandbox.
    pub async fn write_file(
        &self,
        session_id: &str,
        path: &str,
        content: &str,
    ) -> Result<(), Error> {
        let lease = self.lease(session_id).await?;
        lease.execd.upload(path, content.as_bytes().to_vec()).await
    }

    /// Delete the session's sandbox, if it has one. Its files and processes
    /// are gone; the next tool call starts a new one.
    pub async fn release(&self, session_id: &str) -> Result<(), Error> {
        let _turn = self.turn(session_id).await;
        let id = session_id.to_string();
        if let Some(row) = self.stored(move |s| s.sandbox(&id)).await? {
            self.client.delete(&row.sandbox_id).await?;
            self.stored(move |s| s.remove_sandbox(&row.session_id, &row.sandbox_id))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str, key: &str, image: &str, timeout: &str) -> Result<Option<Config>, Error> {
        let some = |v: &str| Some(v.to_string());
        Config::parse(some(url), some(key), some(image), some(timeout), None)
    }

    #[test]
    fn no_url_means_no_sandbox() {
        assert!(
            Config::parse(None, None, None, None, None)
                .unwrap()
                .is_none()
        );
        assert!(parse("  ", "k", "", "").unwrap().is_none());
    }

    #[test]
    fn defaults_apply_to_unset_and_empty_settings() {
        let config = parse("http://sandbox:9090", "", "", "").unwrap().unwrap();
        assert_eq!(config.url.as_str(), "http://sandbox:9090/");
        assert!(config.api_key.is_none());
        assert_eq!(config.image, DEFAULT_IMAGE);
        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(config.env, "dev");
    }

    #[test]
    fn settings_are_read_and_the_key_is_kept_out_of_debug_output() {
        let config = Config::parse(
            Some("https://s.example".into()),
            Some("secret-key".into()),
            Some("img:1".into()),
            Some(" 60 ".into()),
            Some("staging".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.api_key.as_ref().unwrap(), "secret-key");
        assert!(!format!("{config:?}").contains("secret-key"));
        assert_eq!(config.image, "img:1");
        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.env, "staging");
    }

    #[test]
    fn unusable_settings_are_refused_with_the_variable_named() {
        for (url, key, timeout, variable) in [
            ("not a url", "", "", "OPEN_SANDBOX_URL"),
            ("ftp://s", "", "", "OPEN_SANDBOX_URL"),
            ("http://s", "bad\nkey", "", "OPEN_SANDBOX_API_KEY"),
            ("http://s", "", "soon", "ATHENA_SANDBOX_TIMEOUT_SECS"),
            ("http://s", "", "59", "ATHENA_SANDBOX_TIMEOUT_SECS"),
        ] {
            let err = parse(url, key, "", timeout).unwrap_err();
            assert!(matches!(&err, Error::Config(m) if m.contains(variable)));
        }
    }

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_763_303_445_999), "2025-11-16T14:30:45Z");
        assert_eq!(rfc3339(4_107_542_399_000), "2100-02-28T23:59:59Z");
        assert_eq!(rfc3339(-1_000), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn missing_sessions_are_recognised_by_status_or_message() {
        let status = |status, body: &str| Error::Status {
            what: "run".into(),
            status,
            body: body.into(),
        };
        assert!(is_missing(&status(404, "")));
        assert!(is_missing(&status(
            500,
            "{\"message\":\"context abc Not Found\"}"
        )));
        assert!(!is_missing(&status(500, "disk full")));
        assert!(!is_missing(&Error::Http("down".into())));
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let cases = [
            (Error::Config("x".into()), "sandbox configuration: x"),
            (Error::Invalid("bad url".into()), "bad url"),
            (
                Error::Http("refused".into()),
                "sandbox server unreachable: refused",
            ),
            (
                Error::Status {
                    what: "create sandbox".into(),
                    status: 503,
                    body: "busy".into(),
                },
                "sandbox server refused to create sandbox: HTTP 503: busy",
            ),
            (Error::Protocol("odd".into()), "sandbox server: odd"),
            (Error::from(anyhow::anyhow!("locked")), "storage: locked"),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }
}
