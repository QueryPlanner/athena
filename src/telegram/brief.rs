//! The daily training brief, delivered by the scheduler's system pass.
//!
//! Each pass looks at every enabled brief ([`crate::brief::phase`]). A brief
//! that is due runs one agent turn with the fixed [`crate::brief::prompt`]
//! as a scheduled turn ([`Origin::Scheduled`]): in the user's current
//! session, under their one-turn slot ([`Busy`]), so it never interleaves
//! with a conversation, and with an empty user text, so it can confirm
//! nothing.
//!
//! - At most one brief per user per local day: the day is claimed with a
//!   compare-and-set before the turn starts (like a reminder run), so a
//!   crash loses that day's brief and never sends it twice.
//! - The claim is made only once the permit and the user's slot are held.
//!   A busy user, or all permits in use, changes nothing: the next tick
//!   tries again until the send window closes ([`crate::brief::SEND_WINDOW`]),
//!   after which that day is skipped silently. Nothing waits for either.
//! - After Google Health's morning sync when it can be: a connected user
//!   not yet synced today waits up to [`crate::brief::HEALTH_WAIT`].
//! - Telegram refusing for good switches the brief off and records why.
use super::{Busy, Chat, Origin, TRANSPORT, Telegram, jobs::permanent};
use crate::brief::{Phase, health_pending, phase, prompt};
use crate::runner::Run;
use crate::store::BriefCandidate;
use anyhow::Result;
use jiff::Timestamp;
use tokio::sync::Semaphore;

impl<R: Run + 'static> Telegram<R> {
    /// The daily pass for the briefs due at `now`. Safe to overlap.
    pub(super) async fn daily_brief<C: Chat>(
        &self,
        chat: &(dyn Fn(i64) -> C + Send + Sync),
        tasks: &Semaphore,
        now: Timestamp,
    ) {
        let candidates = match self.store.call(|s| s.brief_candidates()).await {
            Ok(candidates) => candidates,
            Err(e) => {
                (self.log)(&format!("daily brief: reading the briefs failed: {e:#}"));
                return;
            }
        };
        let runs = candidates
            .into_iter()
            .map(|c| self.brief_for(chat, tasks, c, now));
        futures_util::future::join_all(runs).await;
    }

    async fn brief_for<C: Chat>(
        &self,
        chat: &(dyn Fn(i64) -> C + Send + Sync),
        tasks: &Semaphore,
        c: BriefCandidate,
        now: Timestamp,
    ) {
        let owner = c.owner;
        if let Err(e) = self.try_brief(chat, tasks, c, now).await {
            let why = format!("{e:#}");
            (self.log)(&format!("daily brief for user {owner} failed: {why}"));
            // Already failing: not being able to note it is no worse.
            let _ = self
                .store
                .call(move |s| s.brief_note_error(owner, &why, now))
                .await;
        }
    }

    /// One user's brief for this pass. `pub(super)` so the scheduler tests
    /// can run it on a candidate read before another pass claimed the day.
    pub(super) async fn try_brief<C: Chat>(
        &self,
        chat: &(dyn Fn(i64) -> C + Send + Sync),
        tasks: &Semaphore,
        c: BriefCandidate,
        now: Timestamp,
    ) -> Result<()> {
        let owner = c.owner;
        let zone = self.store.call(move |s| s.timezone(owner)).await?;
        let pending = match &self.health {
            Some(_) => {
                let connection = self.store.call(move |s| s.health_connection(owner)).await?;
                health_pending(connection.as_ref(), now, &zone)
            }
            None => false,
        };
        let due = phase(
            now,
            &zone,
            &c.local_time,
            c.last_sent_date.as_deref(),
            pending,
        )?;
        if due != Phase::Send {
            return Ok(());
        }
        // Neither is waited for: a miss is tried again on the next tick.
        let Ok(_permit) = tasks.try_acquire() else {
            return Ok(());
        };
        let user_id = c.chat as u64;
        let Some(_busy) = Busy::claim(&self.busy, user_id) else {
            return Ok(());
        };
        let today = now.to_zoned(zone).date();
        let day = today.to_string();
        if !self
            .store
            .call(move |s| s.brief_claim(owner, &day, now))
            .await?
        {
            return Ok(());
        }
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let session = self.current(&user).await?;
        let sent = self
            .turn(
                chat(c.chat),
                user,
                session,
                prompt(today),
                Vec::new(),
                Origin::Scheduled,
            )
            .await;
        if let Some(e) = sent
            && permanent(&e)
        {
            let why = format!("Telegram refused the brief: {e:#}");
            self.store
                .call(move |s| s.brief_disable(owner, &why, now))
                .await?;
        }
        Ok(())
    }
}
