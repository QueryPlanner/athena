//! Connecting, syncing and disconnecting: the [`Health`] service over the
//! store and the Google client.
//!
//! One sync at a time per user in this process ([`Outcome::Running`]); the
//! database claim on `last_attempt_at` keeps two processes (or the daily
//! pass and a manual request) from starting two. A user's access token is
//! cached in memory until a minute before it expires; it is never stored.
//! Only `invalid_grant` revokes a connection: any other failure is recorded
//! in `last_sync_error` and tried again.
use super::catalog::{self, Filter};
#[cfg(test)]
use super::client::MAX_PAGES;
use super::client::{Endpoints, Error, Google, Window};
use super::normalize::{Days, TYPES};
use super::points;
use super::{
    BACKFILL_CHUNKS_PER_PASS, BACKFILL_DAYS, BACKFILL_EMPTY_CHUNKS, BACKFILL_FLOOR_DAYS, Callback,
    Cipher, Config, MANUAL_COOLDOWN, SCOPES, STATE_TTL, Secret, WINDOW_DAYS, due, hash_state,
    new_state, verifier,
};
use crate::scheduler::{Clock, SystemClock};
use crate::store::{Backfill, Candidate, Claim, Store};
use anyhow::Result;
use jiff::{SignedDuration, Timestamp, ToSpan, civil::Date, tz::TimeZone};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use url::Url;

/// An access token is replaced this long before it expires.
const SKEW: SignedDuration = SignedDuration::from_secs(60);

/// How a pasted callback ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Linked {
    Connected,
    /// The user declined at Google.
    Denied,
    /// The `state` is missing, unknown, expired, used, or another user's.
    BadState,
    /// The URL has no `code`.
    NoCode,
    /// Google or the database refused; the text is safe to show.
    Failed(String),
}

/// How a sync ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Synced {
        days: usize,
        /// Data types Google would not give this grant.
        unavailable: Vec<String>,
    },
    /// Google just stopped accepting the authorisation: the user must
    /// reconnect. Returned once, by the call that found out.
    Revoked,
    AlreadyRevoked,
    NotConnected,
    /// A manual sync too soon after the last attempt; try again then.
    Cooldown(Timestamp),
    /// This process is already syncing the user.
    Running,
    /// A failure worth showing; it holds no secret.
    Failed(String),
}

/// What happened to Google's side of a disconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remote {
    Revoked,
    /// Google did not confirm; the token is deleted here regardless.
    Failed,
    /// The connection was already revoked, so there was nothing to revoke.
    NotNeeded,
}

/// A disconnect: `None` if there was no connection.
pub type Disconnected = Option<Remote>;

/// A user the daily pass synced, and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    pub owner: i64,
    pub chat: Option<i64>,
    pub outcome: Outcome,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A claim on one user's sync, released when dropped.
struct Running {
    set: Arc<Mutex<HashSet<i64>>>,
    user: i64,
}

impl Drop for Running {
    fn drop(&mut self) {
        lock(&self.set).remove(&self.user);
    }
}

/// Google Health for this process.
pub struct Health {
    store: Store,
    config: Config,
    google: Google,
    cipher: Cipher,
    clock: Arc<dyn Clock>,
    access: Mutex<HashMap<i64, (Secret, Timestamp)>>,
    running: Arc<Mutex<HashSet<i64>>>,
}

impl Health {
    /// The service the environment configures, if any, on `store`.
    pub fn from_env(store: &Store) -> Result<Option<Arc<Self>>> {
        Ok(Config::from_env()?.map(|config| Self::production(config, store)))
    }

    /// The service for `config`, talking to Google.
    pub fn production(config: Config, store: &Store) -> Arc<Self> {
        Arc::new(Self::new(config, store.clone(), Endpoints::production()))
    }

    /// The service against `endpoints`; tests pass a fake Google's.
    pub fn new(config: Config, store: Store, endpoints: Endpoints) -> Self {
        Self {
            google: Google::new(&config, endpoints),
            cipher: config.cipher(),
            config,
            store,
            clock: Arc::new(SystemClock),
            access: Mutex::default(),
            running: Arc::default(),
        }
    }

    /// Read the time from `clock` instead of the system.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The clock this service reads, for code that must agree with it.
    pub fn shared_clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// The redirect URI pasted URLs are recognised by.
    pub fn redirect(&self) -> &Url {
        &self.config.redirect
    }

    /// The consent link for `owner`: stores the hash of its `state` for
    /// [`STATE_TTL`], replacing any link they had.
    pub async fn begin(&self, owner: i64) -> Result<String> {
        let (state, now) = (new_state(), self.now());
        let hash = hash_state(&state);
        self.store
            .call(move |s| s.health_state_create(owner, &hash, now, now + STATE_TTL))
            .await?;
        Ok(self.config.authorization_url(&state))
    }

    /// Finish a connection from a pasted callback: check the `state`, swap
    /// the `code`, store the sealed refresh token.
    pub async fn complete(&self, owner: i64, callback: Callback) -> Linked {
        let state = callback.state.filter(|s| !s.is_empty());
        let spent = match &state {
            Some(state) => {
                let (hash, now) = (hash_state(state), self.now());
                self.store
                    .call(move |s| s.health_state_consume(owner, &hash, now))
                    .await
            }
            None => Ok(false),
        };
        let state = match (spent, state) {
            (Ok(true), Some(state)) => state,
            (Ok(_), _) => return Linked::BadState,
            (Err(e), _) => return Linked::Failed(format!("{e:#}")),
        };
        if callback.error.is_some() {
            return Linked::Denied;
        }
        let Some(code) = callback.code else {
            return Linked::NoCode;
        };
        let verifier = verifier(&self.config, &state);
        let tokens = match self.google.exchange_code(&code, &verifier).await {
            Ok(tokens) => tokens,
            Err(e) => return Linked::Failed(e.to_string()),
        };
        let Some(refresh) = tokens.refresh else {
            return Linked::Failed("Google did not return a refresh token".into());
        };
        let scopes = if tokens.scopes.is_empty() {
            SCOPES.join(" ")
        } else {
            tokens.scopes.join(" ")
        };
        let (sealed, now) = (self.cipher.seal(owner, refresh.expose()), self.now());
        let stored = self
            .store
            .call(move |s| s.health_connect(owner, &sealed, &scopes, now))
            .await;
        if let Err(e) = stored {
            return Linked::Failed(format!("{e:#}"));
        }
        let expires = now + SignedDuration::from_secs(tokens.expires_in);
        lock(&self.access).insert(owner, (tokens.access, expires));
        Linked::Connected
    }

    /// Delete the user's stored token, after asking Google to revoke it.
    /// Their synced days stay.
    pub async fn disconnect(&self, owner: i64) -> Result<Disconnected> {
        let Some(connection) = self.store.call(move |s| s.health_connection(owner)).await? else {
            return Ok(None);
        };
        self.store.call(move |s| s.health_disconnect(owner)).await?;
        lock(&self.access).remove(&owner);
        if connection.status != "connected" {
            return Ok(Some(Remote::NotNeeded));
        }
        let revoked = match self.cipher.open(owner, &connection.token) {
            Ok(token) => self.google.revoke(&token).await.is_ok(),
            Err(_) => false,
        };
        Ok(Some(if revoked {
            Remote::Revoked
        } else {
            Remote::Failed
        }))
    }

    /// A sync the user asked for: refused within [`MANUAL_COOLDOWN`] of any
    /// attempt.
    pub async fn sync_manual(&self, owner: i64) -> Outcome {
        let now = self.now();
        let claim = self
            .store
            .call(move |s| s.health_claim_manual(owner, now, MANUAL_COOLDOWN))
            .await;
        match claim {
            Ok(Claim::Granted) => self.sync(owner).await,
            Ok(Claim::Cooldown(until)) => Outcome::Cooldown(until),
            Ok(Claim::Revoked) => Outcome::AlreadyRevoked,
            Ok(Claim::NotConnected) => Outcome::NotConnected,
            Err(e) => Outcome::Failed(format!("{e:#}")),
        }
    }

    /// Sync every connected user whose daily sync is due ([`due`]), one
    /// after another, in user order. Returns who was synced.
    pub async fn run_due(&self) -> Result<Vec<Ran>> {
        let now = self.now();
        let candidates = self.store.call(|s| s.health_candidates()).await?;
        let mut ran = Vec::new();
        for candidate in candidates {
            ran.extend(self.run_candidate(candidate, now).await);
        }
        Ok(ran)
    }

    /// Sync `c` if it is due and no one else claimed it first.
    async fn run_candidate(&self, c: Candidate, now: Timestamp) -> Option<Ran> {
        let owner = c.owner;
        let zone = match self.store.call(move |s| s.timezone(owner)).await {
            Ok(zone) => zone,
            Err(e) => {
                self.fail(owner, format!("{e:#}")).await;
                return None;
            }
        };
        if !due(now, c.last_attempt, c.failed, &zone) {
            return None;
        }
        let claimed = self
            .store
            .call(move |s| s.health_claim_scheduled(owner, c.last_attempt, now))
            .await;
        if !matches!(claimed, Ok(true)) {
            return None;
        }
        let outcome = self.run_sync(owner, true).await;
        Some(Ran {
            owner,
            chat: c.chat,
            outcome,
        })
    }

    /// Sync the user's last [`WINDOW_DAYS`] days now. The caller has
    /// claimed the attempt.
    pub async fn sync(&self, owner: i64) -> Outcome {
        self.run_sync(owner, false).await
    }

    /// [`Self::sync`], and, if `backfill`, then a pass of the history
    /// fetch. Only the daily pass backfills: a user's own request stays
    /// quick.
    async fn run_sync(&self, owner: i64, backfill: bool) -> Outcome {
        if !lock(&self.running).insert(owner) {
            return Outcome::Running;
        }
        let _running = Running {
            set: self.running.clone(),
            user: owner,
        };
        self.sync_claimed(owner, backfill).await
    }

    async fn sync_claimed(&self, owner: i64, backfill: bool) -> Outcome {
        let connection = match self.store.call(move |s| s.health_connection(owner)).await {
            Ok(Some(connection)) => connection,
            Ok(None) => return Outcome::NotConnected,
            Err(e) => return Outcome::Failed(format!("{e:#}")),
        };
        if connection.status != "connected" {
            return Outcome::AlreadyRevoked;
        }
        let access = match self.access_token(owner, &connection.token).await {
            Ok(access) => access,
            Err(outcome) => return outcome,
        };
        let zone = match self.store.call(move |s| s.timezone(owner)).await {
            Ok(zone) => zone,
            Err(e) => return self.fail(owner, format!("{e:#}")).await,
        };
        let now = self.now();
        let today = now.to_zoned(zone.clone()).date();
        let window = window(today, &zone);
        let mut days = Days::new(zone.clone());
        let mut unavailable = Vec::new();
        for (data_type, _) in TYPES.iter().copied() {
            let read = self
                .read(owner, &access, data_type, &window, &zone, Some(&mut days))
                .await;
            match read {
                Ok((_, None)) => {}
                Ok((_, Some(reason))) => unavailable.push(reason),
                Err(e) => {
                    if e == Error::Unauthorized {
                        lock(&self.access).remove(&owner);
                    }
                    return self.fail(owner, e.to_string()).await;
                }
            }
        }
        let (start, end) = (window.start_date.clone(), window.end_date.clone());
        let rows = days.finish(start.parse().expect("a date"), end.parse().expect("a date"));
        let count = rows.len();
        let stored = self
            .store
            .call(move |s| s.health_store_sync(owner, &start, &end, &rows, now))
            .await;
        match stored {
            Ok(true) => {
                if backfill {
                    self.backfill(owner, &access, &zone, &window.start_date, today)
                        .await;
                }
                Outcome::Synced {
                    days: count,
                    unavailable,
                }
            }
            Ok(false) => Outcome::NotConnected,
            Err(e) => self.fail(owner, format!("{e:#}")).await,
        }
    }

    /// All pages of `data_type` in `window`, each stored as it arrives (and
    /// folded into `days`, if given), so only one page is in memory. The
    /// answer is how many points were read, and `Some(why)` if Google will
    /// not give this grant that type; `why` names it for the report: a 403
    /// (scope not granted) for any type, and, for a type added after the
    /// first release, an HTTP 400 or 404 with its status, so a wrong filter
    /// shows up in the report instead of failing every user's sync.
    async fn read(
        &self,
        owner: i64,
        access: &Secret,
        data_type: &str,
        window: &Window,
        zone: &TimeZone,
        mut days: Option<&mut Days>,
    ) -> Result<(usize, Option<String>), Error> {
        let spec = catalog::of(data_type);
        let mut page: Option<String> = None;
        let mut read = 0;
        for _ in 0..spec.max_pages {
            let got = match self
                .google
                .data_page(access, data_type, window, page.as_deref())
                .await
            {
                Ok(got) => got,
                Err(Error::Forbidden) => return Ok((read, Some(data_type.to_string()))),
                Err(Error::Http { status, .. }) if spec.optional && matches!(status, 400 | 404) => {
                    return Ok((read, Some(format!("{data_type} (HTTP {status})"))));
                }
                Err(e) => return Err(e),
            };
            read += got.points.len();
            let (name, rows, now) = (
                data_type.to_string(),
                points::rows(data_type, &got.points, zone),
                self.now(),
            );
            self.store
                .call(move |s| s.health_points_put(owner, &name, &rows, now))
                .await
                .map_err(|_| Error::Storage("the data points could not be saved"))?;
            if let Some(days) = days.as_deref_mut() {
                days.add(data_type, &got.points);
            }
            match got.next {
                Some(next) => page = Some(next),
                None => return Ok((read, None)),
            }
        }
        Err(Error::TooMuch(
            "Google sent more pages than this build reads",
        ))
    }

    /// One pass of the history fetch for `owner`: for each data type not
    /// yet done, up to [`BACKFILL_CHUNKS_PER_PASS`] chunks of
    /// [`BACKFILL_DAYS`] days, working backwards from the oldest day
    /// fetched so far (at first, the start of the recent window). A type is
    /// done at [`BACKFILL_FLOOR_DAYS`] back, after [`BACKFILL_EMPTY_CHUNKS`]
    /// empty chunks in a row, or when Google refuses it. A failure leaves
    /// the cursor where it was (the pass ends for a transport or server
    /// error; it never fails the sync, which has already been stored).
    async fn backfill(
        &self,
        owner: i64,
        access: &Secret,
        zone: &TimeZone,
        window_start: &str,
        today: Date,
    ) {
        let Ok(progress) = self.store.call(move |s| s.health_backfill(owner)).await else {
            return;
        };
        let floor = today.saturating_sub(BACKFILL_FLOOR_DAYS.days());
        for (data_type, _) in TYPES.iter().copied() {
            let mut cur = progress
                .iter()
                .find(|p| p.data_type == data_type)
                .cloned()
                .unwrap_or_else(|| Backfill {
                    data_type: data_type.into(),
                    oldest: window_start.into(),
                    done: false,
                    empty_run: 0,
                });
            for _ in 0..BACKFILL_CHUNKS_PER_PASS {
                if cur.done {
                    break;
                }
                match self
                    .backfill_chunk(owner, access, zone, &mut cur, floor)
                    .await
                {
                    Chunk::Next => {}
                    Chunk::Stop => break,
                    Chunk::Abort => return,
                }
            }
        }
    }

    /// Fetch the chunk before `cur.oldest` and move the cursor. The
    /// cursor is saved either way.
    async fn backfill_chunk(
        &self,
        owner: i64,
        access: &Secret,
        zone: &TimeZone,
        cur: &mut Backfill,
        floor: Date,
    ) -> Chunk {
        let data_type = cur.data_type.clone();
        let spec = catalog::of(&data_type);
        let oldest: Date = cur.oldest.parse().unwrap_or(floor);
        // ECG can only be filtered by a start time, with no upper bound, so
        // its history is fetched in one go, back to the floor.
        let start = if spec.filter == Filter::EcgStart {
            floor
        } else {
            oldest.saturating_sub(BACKFILL_DAYS.days()).max(floor)
        };
        let chunk = window_between(start, oldest, zone);
        let read = self
            .read(owner, access, &data_type, &chunk, zone, None)
            .await;
        let (note, outcome) = match read {
            Ok((_, Some(why))) => {
                cur.done = true;
                (Some(why), Chunk::Stop)
            }
            Ok((n, None)) => {
                cur.empty_run = if n == 0 { cur.empty_run + 1 } else { 0 };
                cur.oldest = start.to_string();
                cur.done = start <= floor
                    || cur.empty_run >= BACKFILL_EMPTY_CHUNKS
                    || spec.filter == Filter::EcgStart;
                (None, Chunk::Next)
            }
            // Too many pages: keep what was read and move on.
            Err(e @ Error::TooMuch(_)) => {
                cur.empty_run = 0;
                cur.oldest = start.to_string();
                cur.done = start <= floor || spec.filter == Filter::EcgStart;
                (Some(format!("partial: {e}")), Chunk::Next)
            }
            Err(e) => (Some(e.to_string()), Chunk::Abort),
        };
        let (progress, now) = (cur.clone(), self.now());
        let saved = self
            .store
            .call(move |s| s.health_backfill_save(owner, &progress, note.as_deref(), now))
            .await;
        if saved.is_err() {
            return Chunk::Abort;
        }
        outcome
    }

    /// A valid access token for `owner`: the cached one, or a new one from
    /// the stored refresh token. The `Err` is the sync's outcome.
    async fn access_token(&self, owner: i64, sealed: &[u8]) -> Result<Secret, Outcome> {
        let now = self.now();
        if let Some((token, expires)) = lock(&self.access).get(&owner)
            && *expires - SKEW > now
        {
            return Ok(token.clone());
        }
        let refresh = match self.cipher.open(owner, sealed) {
            Ok(refresh) => refresh,
            Err(e) => return Err(self.revoked(owner, &e.to_string()).await),
        };
        let tokens = match self.google.refresh(&refresh).await {
            Ok(tokens) => tokens,
            Err(Error::Revoked) => {
                return Err(self
                    .revoked(owner, "Google rejected the refresh token")
                    .await);
            }
            Err(e) => return Err(self.fail(owner, e.to_string()).await),
        };
        // Google may rotate the refresh token; keep the newest.
        if let Some(rotated) = tokens.refresh.filter(|r| r.expose() != refresh.expose()) {
            let sealed = self.cipher.seal(owner, rotated.expose());
            let saved = self
                .store
                .call(move |s| s.health_update_token(owner, &sealed, now))
                .await;
            if let Err(e) = saved {
                return Err(self.fail(owner, format!("{e:#}")).await);
            }
        }
        let expires = now + SignedDuration::from_secs(tokens.expires_in);
        lock(&self.access).insert(owner, (tokens.access.clone(), expires));
        Ok(tokens.access)
    }

    /// Mark the connection revoked. [`Outcome::Revoked`] for the call that
    /// did it.
    async fn revoked(&self, owner: i64, why: &str) -> Outcome {
        lock(&self.access).remove(&owner);
        let (why, now) = (why.to_string(), self.now());
        match self
            .store
            .call(move |s| s.health_mark_revoked(owner, &why, now))
            .await
        {
            Ok(true) => Outcome::Revoked,
            Ok(false) => Outcome::AlreadyRevoked,
            Err(e) => Outcome::Failed(format!("{e:#}")),
        }
    }

    /// Record a failed sync, and say so.
    async fn fail(&self, owner: i64, why: String) -> Outcome {
        let (note, now) = (why.clone(), self.now());
        // The sync already failed: a failure to say so is not worse.
        let _ = self
            .store
            .call(move |s| s.health_mark_failed(owner, &note, now))
            .await;
        Outcome::Failed(why)
    }
}

/// The last [`WINDOW_DAYS`] local days ending with `today`, as dates and as
/// the instants they start at. The end is the start of tomorrow: a day with
/// a clock change is its real length, never "24 hours".
fn window(today: Date, zone: &TimeZone) -> Window {
    let start = today.saturating_sub((WINDOW_DAYS - 1).days());
    window_between(start, today.saturating_add(1.days()), zone)
}

/// What a chunk of the history fetch came to.
enum Chunk {
    /// Go on with the next chunk of this type.
    Next,
    /// This type is done for this pass; go on to the next type.
    Stop,
    /// Something is wrong that the other types will meet too: end the pass.
    Abort,
}

/// The local days from `start` up to but not including `end`.
fn window_between(start: Date, end: Date, zone: &TimeZone) -> Window {
    let at = |date: Date| {
        date.to_zoned(zone.clone())
            .expect("the start of a real day exists")
            .timestamp()
            .to_string()
    };
    Window {
        start: at(start),
        end: at(end),
        start_date: start.to_string(),
        end_date: end.to_string(),
    }
}

#[cfg(test)]
mod tests;
