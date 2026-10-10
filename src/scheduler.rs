//! The loop that runs scheduled jobs when they are due.
//!
//! It runs inside `athena telegram` only, the one process that can deliver
//! a reminder and see whether its user is mid-turn (`telegram.rs`). Every
//! [`POLL`] it leases the jobs that are due ([`Store::claim_jobs`]) and runs
//! each in its own task through an [`Execute`]; what to do with a job, and
//! how its row changes, is the executor's business.
//!
//! On a stop signal it claims nothing more and waits for the jobs it
//! started. A lease that is never closed, because the process died, lapses
//! after [`LEASE`], and the job is claimed again then.
use crate::store::{Due, Store};
use anyhow::Result;
use jiff::{SignedDuration, Timestamp};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::{JoinError, JoinSet};

/// How often due jobs are looked for.
pub const POLL: Duration = Duration::from_secs(30);
/// Most jobs claimed at once.
pub const BATCH: i64 = 20;
/// How long a claimed job is this process's. Longer than the slowest
/// delivery: three attempts at a two-minute Bot API timeout.
pub const LEASE: SignedDuration = SignedDuration::from_mins(10);
/// How late a run may be before it says so.
pub const LATE_AFTER: SignedDuration = SignedDuration::from_mins(5);
/// How long an `agent_task` waits while its user is mid-turn.
pub const DEFER: SignedDuration = SignedDuration::from_mins(1);
/// How late an `agent_task` may get waiting for its user before that run is
/// skipped.
pub const MAX_DEFER: SignedDuration = SignedDuration::from_mins(30);
/// Deliveries tried per run before the job is marked failed.
pub const MAX_ATTEMPTS: i64 = 5;
/// The wait after a first failed delivery; it doubles each time, so the
/// last retry is eight minutes after the one before.
pub const BACKOFF: SignedDuration = SignedDuration::from_mins(1);

/// Receives problems worth an operator's attention.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// What time it is. Tests set it.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}

/// The system's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// Runs one claimed job and records its outcome.
pub trait Execute: Send + Sync + 'static {
    fn execute(&self, job: Due, now: Timestamp) -> impl Future<Output = ()> + Send;
}

pub struct Scheduler<E> {
    store: Store,
    executor: Arc<E>,
    clock: Arc<dyn Clock>,
    every: Duration,
    log: Log,
}

impl<E: Execute> Scheduler<E> {
    pub fn new(store: Store, executor: E, log: Log) -> Self {
        Self {
            store,
            executor: Arc::new(executor),
            clock: Arc::new(SystemClock),
            every: POLL,
            log,
        }
    }

    /// Read the time from `clock` instead of the system.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Look for due jobs this often instead of [`POLL`].
    pub fn every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    /// Claim the jobs due now and start each in `running`. Returns how many
    /// were started.
    pub async fn tick(&self, running: &mut JoinSet<()>) -> Result<usize> {
        let now = self.clock.now();
        let due = self.store.call(move |s| s.claim_jobs(now, BATCH)).await?;
        let started = due.len();
        for job in due {
            let executor = self.executor.clone();
            running.spawn(async move { executor.execute(job, now).await });
        }
        Ok(started)
    }

    /// Run jobs as they fall due until `stop` resolves, then wait for the
    /// ones started.
    pub async fn run(self, stop: impl Future<Output = ()>) {
        let mut running = JoinSet::new();
        tokio::pin!(stop);
        loop {
            while let Some(finished) = running.try_join_next() {
                self.reap(finished);
            }
            if let Err(e) = self.tick(&mut running).await {
                (self.log)(&format!("claiming scheduled jobs failed: {e:#}"));
            }
            tokio::select! {
                biased;
                () = &mut stop => break,
                () = tokio::time::sleep(self.every) => {}
            }
        }
        while let Some(finished) = running.join_next().await {
            self.reap(finished);
        }
    }

    fn reap(&self, finished: Result<(), JoinError>) {
        if let Err(e) = finished {
            (self.log)(&format!("a scheduled job's task failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::{Semaphore, mpsc, oneshot};

    /// Reports each job it is given. Panics on a payload of "panic"; one
    /// of "slow" waits for a permit from the gate first.
    struct Recording(mpsc::UnboundedSender<(i64, Timestamp)>, Arc<Semaphore>);

    impl Execute for Recording {
        async fn execute(&self, job: Due, now: Timestamp) {
            assert_ne!(job.payload, "panic", "told to panic");
            if job.payload == "slow" {
                let _ = self.1.acquire().await.unwrap();
            }
            self.0.send((job.id, now)).unwrap();
        }
    }

    struct Fixed(Timestamp);
    impl Clock for Fixed {
        fn now(&self) -> Timestamp {
            self.0
        }
    }

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn setup() -> (Store, i64, mpsc::UnboundedReceiver<String>, Log) {
        let store = Store::open_in_memory().unwrap();
        let owner = store.user("telegram", "7").unwrap().id();
        let (sink, logged) = mpsc::unbounded_channel();
        let sink = Mutex::new(sink);
        let log: Log = Arc::new(move |m| sink.lock().unwrap().send(m.to_string()).unwrap());
        (store, owner, logged, log)
    }

    fn insert(store: &Store, owner: i64, payload: &str, due: &str) -> i64 {
        let db = store.db_for_tests();
        db.execute(
            "INSERT INTO jobs (user_id, kind, payload, next_run_at, status, created_at, updated_at)
             VALUES (?1, 'notify', ?2, ?3, 'active', 0, 0)",
            rusqlite::params![owner, payload, at(due).as_millisecond()],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn the_system_clock_is_now() {
        let before = Timestamp::now();
        let now = SystemClock.now();
        assert!(before <= now && now <= Timestamp::now());
    }

    #[tokio::test]
    async fn a_tick_starts_the_due_jobs_once_each() {
        let (store, owner, _, log) = setup();
        let due = insert(&store, owner, "a", "2026-10-10T09:00:00Z");
        insert(&store, owner, "b", "2026-10-10T10:00:00Z");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let now = at("2026-10-10T09:30:00Z");
        let gate = Arc::new(Semaphore::new(0));
        let scheduler = Scheduler::new(store, Recording(tx, gate), log).clock(Arc::new(Fixed(now)));
        let mut running = JoinSet::new();

        assert_eq!(scheduler.tick(&mut running).await.unwrap(), 1);
        while running.join_next().await.is_some() {}
        assert_eq!(rx.recv().await, Some((due, now)));
        // Leased: the next tick does not start it again.
        assert_eq!(scheduler.tick(&mut running).await.unwrap(), 0);
    }

    /// The loop keeps polling until told to stop, logs a tick that fails
    /// and a job that panics, and waits for the jobs it started.
    #[tokio::test]
    async fn the_loop_polls_until_stopped_then_drains() {
        let (store, owner, mut logged, log) = setup();
        let first = insert(&store, owner, "a", "2026-10-10T09:00:00Z");
        insert(&store, owner, "panic", "2026-10-10T09:00:00Z");
        let slow = insert(&store, owner, "slow", "2026-10-10T09:00:00Z");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Semaphore::new(0));
        let scheduler = Scheduler::new(store.clone(), Recording(tx, gate.clone()), log)
            .clock(Arc::new(Fixed(at("2026-10-10T09:30:00Z"))))
            .every(Duration::from_millis(5));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(scheduler.run(async move {
            let _ = stopped.await;
        }));

        assert_eq!(rx.recv().await.unwrap().0, first);
        // Due after the first tick: only a later one finds it.
        let second = insert(&store, owner, "b", "2026-10-10T09:00:00Z");
        assert_eq!(rx.recv().await.unwrap().0, second);
        // A tick that cannot read the table is logged, and the loop goes on.
        store
            .db_for_tests()
            .execute_batch("ALTER TABLE jobs RENAME TO gone")
            .unwrap();
        let mut seen = Vec::new();
        while !seen
            .iter()
            .any(|m: &String| m.contains("claiming scheduled jobs failed"))
        {
            seen.push(logged.recv().await.unwrap());
        }
        // Stopped while the slow job runs: the loop waits for it.
        stop.send(()).unwrap();
        gate.add_permits(1);
        task.await.unwrap();
        assert_eq!(rx.try_recv().unwrap().0, slow);
        seen.extend(std::iter::from_fn(|| logged.try_recv().ok()));
        assert!(seen.iter().any(|m| m.contains("task failed")), "{seen:?}");
    }
}
