//! Scheduled jobs, delivered by the bot: what [`crate::scheduler`] runs.
//!
//! - `notify` sends the reminder's text. The run is recorded right after
//!   Telegram accepts it, so only a crash between the two sends it twice.
//! - `agent_task` runs the reminder's prompt as a turn in the user's
//!   current session, under their one-turn slot ([`Busy`]) and the
//!   session's lock, and sends the reply as any turn's. If the user is
//!   mid-turn it waits ([`DEFER`]) rather than interleave, and past
//!   [`MAX_DEFER`] that run is skipped. The run is recorded before the turn
//!   starts, so a paid turn never runs twice; a crash mid-turn loses it.
//! - Telegram refusing for good (the user blocked the bot or never started
//!   it) fails the job. Other failures are retried with a backoff, up to
//!   [`MAX_ATTEMPTS`](crate::scheduler::MAX_ATTEMPTS) times; a timed-out send may have arrived, so a retry
//!   can repeat a reminder rather than lose it.
//! - A run more than [`LATE_AFTER`] past its time says so. A repeating job
//!   then waits for its next time after now: missed runs are not made up.
use super::{Busy, Chat, Origin, TRANSPORT, Telegram, say};
use crate::reminders::{Kind, TASK_RUNS_PER_DAY};
use crate::runner::Run;
use crate::scheduler::{DEFER, Execute, LATE_AFTER, MAX_DEFER, Scheduler};
use crate::store::{Due, Store};
use anyhow::Result;
use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use std::sync::Arc;
use teloxide::{ApiError, Bot, RequestError, types::ChatId};
use tokio::sync::Semaphore;

/// `agent_task` turns running at once, across every user.
pub const TASKS: usize = 4;

/// Whether Telegram refused a send for good: the user blocked the bot,
/// deleted their account, or never started a chat with it.
pub fn permanent(e: &anyhow::Error) -> bool {
    let Some(RequestError::Api(api)) = e.downcast_ref::<RequestError>() else {
        return false;
    };
    match api {
        ApiError::BotBlocked
        | ApiError::UserDeactivated
        | ApiError::ChatNotFound
        | ApiError::CantInitiateConversation => true,
        // Telegram words "can't initiate conversation" as "Forbidden: ...",
        // not as teloxide's variant expects; any 403 is for good.
        ApiError::Unknown(description) => description.starts_with("Forbidden"),
        _ => false,
    }
}

/// The reminder as sent: its text, and when it was due if it is late.
pub fn notice(text: &str, late: Option<&str>) -> String {
    match late {
        None => format!("Reminder: {text}"),
        Some(due) => format!("Reminder (late: it was due {due}): {text}"),
    }
}

/// The prompt an `agent_task` runs as.
pub fn task_prompt(id: i64, prompt: &str, due: &str, late: bool) -> String {
    let late = if late { ", and it is running late" } else { "" };
    format!(
        "Scheduled task #{id}, which the user confirmed earlier, was due {due}{late}. \
         Do it now and reply to the user with the result.\n\n{prompt}"
    )
}

/// `at` on the user's wall clock, for a message: `Sat 10 Oct 09:00`.
fn wall(at: Timestamp, zone: &TimeZone) -> String {
    at.to_zoned(zone.clone())
        .strftime("%a %d %b %H:%M")
        .to_string()
}

/// Runs claimed jobs through `app`, sending to chats `chat` makes.
pub struct Jobs<R, C> {
    app: Arc<Telegram<R>>,
    chat: Arc<dyn Fn(i64) -> C + Send + Sync>,
    tasks: Arc<Semaphore>,
}

impl<R: Run + 'static, C: Chat> Jobs<R, C> {
    pub fn new(app: Arc<Telegram<R>>, chat: impl Fn(i64) -> C + Send + Sync + 'static) -> Self {
        Self {
            app,
            chat: Arc::new(chat),
            tasks: Arc::new(Semaphore::new(TASKS)),
        }
    }
}

impl<R: Run + 'static, C: Chat> Execute for Jobs<R, C> {
    async fn execute(&self, job: Due, now: Timestamp) {
        let id = job.id;
        if let Err(e) = self.app.run_job(&*self.chat, &self.tasks, job, now).await {
            (self.app.log)(&format!("scheduled job {id}: {e:#}"));
        }
    }

    async fn system(&self, now: Timestamp) {
        // The sync first, so a brief due this tick reads this morning's data.
        self.app.daily_health(&*self.chat).await;
        self.app.daily_brief(&*self.chat, &self.tasks, now).await;
    }
}

/// The scheduler for `app`'s jobs, delivering through `bot`. `store` is
/// `app`'s.
pub fn scheduler<R: Run + 'static>(
    app: Arc<Telegram<R>>,
    bot: Bot,
    store: Store,
) -> Scheduler<impl Execute> {
    let log = app.log.clone();
    let jobs = Jobs::new(app, move |chat| super::TelegramChat {
        bot: bot.clone(),
        chat: ChatId(chat),
    });
    Scheduler::new(store, jobs, log)
}

impl<R: Run + 'static> Telegram<R> {
    async fn run_job<C: Chat>(
        &self,
        chat: &(dyn Fn(i64) -> C + Send + Sync),
        tasks: &Semaphore,
        job: Due,
        now: Timestamp,
    ) -> Result<()> {
        let id = job.id;
        let Some(kind) = Kind::parse(&job.kind) else {
            let why = format!("this build cannot run jobs of kind `{}`", job.kind);
            return self.store.call(move |s| s.job_failed(id, now, &why)).await;
        };
        let Some(chat_id) = job.chat else {
            let why = "the user has no Telegram chat to deliver to";
            return self.store.call(move |s| s.job_failed(id, now, why)).await;
        };
        // Worked out before delivering, so a delivered run is always recorded.
        let shown = job.clone();
        let planned = self
            .store
            .call(move |s| {
                Ok::<_, anyhow::Error>((s.next_run(&shown, now)?, s.timezone(shown.owner)?))
            })
            .await;
        let (next, zone) = match planned {
            Ok(planned) => planned,
            Err(e) => {
                let why = format!("{e:#}");
                return self.store.call(move |s| s.job_failed(id, now, &why)).await;
            }
        };
        let late = now.duration_since(job.due) > LATE_AFTER;
        let due = wall(job.due, &zone);
        let chat = chat(chat_id);
        match kind {
            Kind::Notify => {
                let text = notice(&job.payload, late.then_some(due.as_str()));
                match chat.say(&text).await {
                    Ok(()) => {
                        self.store
                            .call(move |s| s.job_done(id, now, next, true, None))
                            .await
                    }
                    Err(e) => self.undelivered(&job, now, e).await,
                }
            }
            Kind::AgentTask => {
                let prompt = task_prompt(id, &job.payload, &due, late);
                self.task(chat, tasks, job, now, next, prompt).await
            }
        }
    }

    /// Record a failed delivery of `job`: failed for good, or retried.
    async fn undelivered(&self, job: &Due, now: Timestamp, e: anyhow::Error) -> Result<()> {
        let why = format!("{e:#}");
        (self.log)(&format!(
            "delivering scheduled job {} failed: {why}",
            job.id
        ));
        let job = job.clone();
        if permanent(&e) {
            return self
                .store
                .call(move |s| s.job_failed(job.id, now, &why))
                .await;
        }
        self.store
            .call(move |s| s.job_retry(&job, now, &why).map(drop))
            .await
    }

    async fn task<C: Chat>(
        &self,
        chat: C,
        tasks: &Semaphore,
        job: Due,
        now: Timestamp,
        next: Option<Timestamp>,
        prompt: String,
    ) -> Result<()> {
        let (id, owner) = (job.id, job.owner);
        let user_id = job.chat.expect("only jobs with a chat run") as u64;
        // Cannot fail: the semaphore is never closed.
        let _permit = tasks.acquire().await.expect("tasks is never closed");
        let Some(busy) = Busy::claim(&self.busy, user_id) else {
            if now.duration_since(job.due) <= MAX_DEFER {
                return self.store.call(move |s| s.job_defer(id, now + DEFER)).await;
            }
            let why = "skipped: the user was mid-conversation for too long past its time";
            self.store
                .call(move |s| s.job_done(id, now, next, false, Some(why)))
                .await?;
            say(
                &chat,
                &self.log,
                &format!(
                    "I skipped scheduled task #{id}: you were in a conversation with me for \
                     {} minutes past its time.",
                    MAX_DEFER.as_mins()
                ),
            )
            .await;
            return Ok(());
        };
        let _busy = busy;
        let since = now - SignedDuration::from_hours(24);
        if self
            .store
            .call(move |s| s.task_runs_since(owner, since))
            .await?
            >= TASK_RUNS_PER_DAY
        {
            let why = "skipped: the daily limit of scheduled tasks was reached";
            self.store
                .call(move |s| s.job_done(id, now, next, false, Some(why)))
                .await?;
            say(
                &chat,
                &self.log,
                &format!(
                    "I skipped scheduled task #{id}: you can have at most {TASK_RUNS_PER_DAY} \
                     scheduled tasks run in 24 hours."
                ),
            )
            .await;
            return Ok(());
        }
        // Recorded before the turn: a paid turn is never run twice.
        self.store
            .call(move |s| s.job_done(id, now, next, true, None))
            .await?;
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let session = self.current(&user).await?;
        if let Some(e) = self
            .turn(chat, user, session, prompt, Vec::new(), Origin::Scheduled)
            .await
            && permanent(&e)
        {
            let why = format!("{e:#}");
            // A repeating task stops: nobody can read its replies.
            self.store
                .call(move |s| s.job_failed(id, now, &why))
                .await?;
        }
        Ok(())
    }
}
