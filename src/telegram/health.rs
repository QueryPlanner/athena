//! Google Health in the bot: `/connect_health`, `/disconnect_health`, the
//! pasted callback URL, and the daily sync.
//!
//! A message holding the callback URL (the `code` Google issued) is handled
//! here, in [`Telegram::respond`] before anything else: it is never sent to
//! the model, never stored in the transcript and never logged. Nothing of it
//! is kept: the `state` is checked, the `code` exchanged, and both dropped.
use super::{Chat, TRANSPORT, Telegram, lock, say};
use crate::health::sync::{Linked, Outcome, Ran, Remote};
use crate::health::{Callback, STATE_TTL, WINDOW_DAYS};
use crate::runner::Run;
use crate::service::{self, User};
use std::sync::Arc;

pub const NOT_SET_UP: &str = "Google Health is not set up on this server.";
pub const CONNECTED: &str = "Connected to Google Health (read-only). Fetching your last 14 days \
    now. You can delete the message you pasted: I did not pass it to the AI or save it.";
pub const DENIED: &str = "Google said access was not granted, so nothing is connected. \
    /connect_health starts again.";
pub const BAD_STATE: &str = "That link is not valid for you any more: it expired, was already \
    used, or was not made for you. Nothing was connected. Send /connect_health for a new one.";
pub const NO_CODE: &str = "That address has no authorisation code in it. Copy the whole \
    address from the browser after you approve. /connect_health starts again.";
pub const REVOKED: &str = "Google no longer accepts Athena's access to your Google Health, so \
    I cannot sync. Send /connect_health to connect again.";

/// The `/connect_health` message around the consent `url`.
pub fn connect_message(url: &str, redirect: &str) -> String {
    format!(
        "Connect Google Health. I only ask to read: activity, sleep, heart rate, body \
         measurements and nutrition.\n\n\
         1. Open this link and approve (it works for {} minutes):\n{url}\n\n\
         2. The browser then goes to an address starting {redirect}. It may fail to load: \
         that is fine. Copy the whole address from the address bar and paste it here as a \
         message. I handle that message myself and never pass it to the AI.\n\n\
         Once connected, what you ask me about your sleep, recovery and training sends \
         the relevant numbers to the AI model.",
        STATE_TTL.as_mins()
    )
}

/// What `/disconnect_health` says.
pub fn disconnect_message(remote: Option<Remote>) -> &'static str {
    match remote {
        None => "Google Health is not connected.",
        Some(Remote::Revoked) => {
            "Disconnected. Google confirmed the access is revoked and I deleted the stored \
             token. The daily summaries already synced stay on this server. /connect_health \
             connects again."
        }
        Some(Remote::Failed) => {
            "Disconnected: I deleted the stored token, but could not confirm with Google that \
             access is revoked. Remove Athena at https://myaccount.google.com/permissions to \
             be sure. The daily summaries already synced stay on this server."
        }
        Some(Remote::NotNeeded) => {
            "Disconnected. I deleted the stored token (Google had already revoked it). The \
             daily summaries already synced stay on this server."
        }
    }
}

/// What the first sync after connecting says.
pub fn first_sync_message(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Synced { days, unavailable } if unavailable.is_empty() => format!(
            "Synced {days} days of the last {WINDOW_DAYS}. Ask me about your sleep, recovery \
             or activity. It syncs again every morning."
        ),
        Outcome::Synced { days, unavailable } => format!(
            "Synced {days} days of the last {WINDOW_DAYS}. Google did not allow: {}. You may \
             have unticked those boxes; /connect_health again to change that.",
            unavailable.join(", ")
        ),
        Outcome::Revoked | Outcome::AlreadyRevoked => REVOKED.into(),
        Outcome::Failed(why) => format!(
            "Connected, but the first sync failed ({why}). It is tried again, or ask me to \
             sync later."
        ),
        Outcome::NotConnected | Outcome::Cooldown(_) | Outcome::Running => {
            "Connected. The first sync did not start; ask me to sync in a little while.".into()
        }
    }
}

impl<R: Run + 'static> Telegram<R> {
    /// The reply to `/connect_health`.
    pub(super) async fn connect_health(&self, user: &User) -> Result<String, service::Error> {
        let Some(health) = &self.health else {
            return Ok(NOT_SET_UP.into());
        };
        let url = health.begin(user.id()).await?;
        Ok(connect_message(&url, health.redirect().as_str()))
    }

    /// The reply to `/disconnect_health`.
    pub(super) async fn disconnect_health(&self, user: &User) -> Result<String, service::Error> {
        let Some(health) = &self.health else {
            return Ok(NOT_SET_UP.into());
        };
        Ok(disconnect_message(health.disconnect(user.id()).await?).into())
    }

    /// A message with the callback URL: finish the connection, or say why
    /// not. Returns the reply to send now, if there is one.
    pub(super) async fn paste<C: Chat>(
        self: &Arc<Self>,
        chat: &C,
        user_id: u64,
        callback: Callback,
    ) -> Result<Option<String>, service::Error> {
        let Some(health) = &self.health else {
            return Ok(Some(NOT_SET_UP.into()));
        };
        let user = self.service.user(TRANSPORT, &user_id.to_string()).await?;
        let reply = match health.complete(user.id(), callback).await {
            Linked::Connected => {
                say(chat, &self.log, CONNECTED).await;
                let (app, chat, owner) = (self.clone(), chat.clone(), user.id());
                let mut turns = lock(&self.turns);
                while turns.try_join_next().is_some() {}
                turns.spawn(async move { app.first_sync(chat, owner).await });
                return Ok(None);
            }
            Linked::Denied => DENIED.into(),
            Linked::BadState => BAD_STATE.into(),
            Linked::NoCode => NO_CODE.into(),
            Linked::Failed(why) => {
                (self.log)(&format!(
                    "telegram user {user_id}: connecting Google Health failed: {why}"
                ));
                format!("I could not connect Google Health: {why}. /connect_health starts again.")
            }
        };
        Ok(Some(reply))
    }

    async fn first_sync<C: Chat>(&self, chat: C, owner: i64) {
        let Some(health) = &self.health else { return };
        let outcome = health.sync_manual(owner).await;
        if let Outcome::Failed(why) = &outcome {
            (self.log)(&format!(
                "first Google Health sync for user {owner} failed: {why}"
            ));
        }
        say(&chat, &self.log, &first_sync_message(&outcome)).await;
    }

    /// The daily pass: sync the users whose time has come, and tell, once,
    /// those whose access Google just ended. Nothing else is said.
    pub(super) async fn daily_health<C: Chat>(&self, chat: &(dyn Fn(i64) -> C + Send + Sync)) {
        let Some(health) = &self.health else { return };
        let ran = match health.run_due().await {
            Ok(ran) => ran,
            Err(e) => {
                (self.log)(&format!("Google Health daily sync failed: {e:#}"));
                return;
            }
        };
        for Ran {
            owner,
            chat: to,
            outcome,
        } in ran
        {
            match (&outcome, to) {
                (Outcome::Revoked, Some(to)) => {
                    say(&chat(to), &self.log, REVOKED).await;
                }
                (Outcome::Failed(why), _) => {
                    (self.log)(&format!(
                        "Google Health sync for user {owner} failed: {why}"
                    ));
                }
                _ => {}
            }
        }
    }
}
