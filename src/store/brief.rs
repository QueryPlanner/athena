//! SQLite operations for the daily training brief: the user's choice of
//! time, and the once-a-day claim the scheduler makes. Times come in as
//! arguments so the scheduler owns the clock.
use super::Store;
use anyhow::{Context, Result};
use jiff::Timestamp;
use rusqlite::{OptionalExtension, params};

/// The longest `last_error` kept, in characters.
const ERROR_CHARS: usize = 200;

fn short(why: &str) -> String {
    why.chars().take(ERROR_CHARS).collect()
}

fn instant(ms: i64) -> Result<Timestamp> {
    Timestamp::from_millisecond(ms).context("a stored time is out of range")
}

/// A user's brief settings and bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Brief {
    /// `HH:MM` on the user's wall clock.
    pub local_time: String,
    pub enabled: bool,
    /// The user's local date the last brief was claimed for.
    pub last_sent_date: Option<String>,
    pub last_attempt_at: Option<Timestamp>,
    pub last_error: Option<String>,
}

/// An enabled brief with a Telegram chat to deliver it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefCandidate {
    pub owner: i64,
    /// The user's Telegram user id, which is also their private chat id.
    pub chat: i64,
    pub local_time: String,
    pub last_sent_date: Option<String>,
}

impl Store {
    /// The user's Telegram user id, if they have a Telegram identity.
    pub fn telegram_chat(&self, owner: i64) -> Result<Option<i64>> {
        let id: Option<String> = self
            .db()
            .query_row(
                "SELECT external_id FROM user_identities
                 WHERE user_id = ?1 AND transport = 'telegram'
                 ORDER BY created_at, rowid LIMIT 1",
                [owner],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| id.parse().context("a Telegram user id is not a number"))
            .transpose()
    }

    /// Switch the brief on for `owner` at `local_time` (`HH:MM`), clearing
    /// any earlier error. With `passed`, that time has already gone by on
    /// `today` (the user's local date), so today counts as done and the
    /// first brief is tomorrow's; without it a brief already sent today
    /// stays sent, so changing the time or switching off and on cannot
    /// send a second one.
    pub fn brief_set(
        &self,
        owner: i64,
        local_time: &str,
        today: &str,
        passed: bool,
        now: Timestamp,
    ) -> Result<()> {
        let now = now.as_millisecond();
        self.db().execute(
            "INSERT INTO daily_briefs
                 (user_id, local_time, enabled, last_sent_date, created_at, updated_at)
             VALUES (?1, ?2, 1, CASE WHEN ?4 THEN ?3 END, ?5, ?5)
             ON CONFLICT (user_id) DO UPDATE SET
                 local_time = ?2, enabled = 1, last_error = NULL,
                 last_sent_date = CASE WHEN ?4 THEN ?3 ELSE last_sent_date END,
                 updated_at = ?5",
            params![owner, local_time, today, passed, now],
        )?;
        Ok(())
    }

    /// Switch the brief off. Returns whether it was on.
    pub fn brief_off(&self, owner: i64, now: Timestamp) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE daily_briefs SET enabled = 0, updated_at = ?2
             WHERE user_id = ?1 AND enabled = 1",
            params![owner, now.as_millisecond()],
        )?;
        Ok(changed == 1)
    }

    /// The user's brief row, if they ever set one up.
    pub fn brief(&self, owner: i64) -> Result<Option<Brief>> {
        let row = self
            .db()
            .query_row(
                "SELECT local_time, enabled, last_sent_date, last_attempt_at, last_error
                 FROM daily_briefs WHERE user_id = ?1",
                [owner],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, bool>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(local_time, enabled, last_sent_date, attempt, last_error)| {
                Ok(Brief {
                    local_time,
                    enabled,
                    last_sent_date,
                    last_attempt_at: attempt.map(instant).transpose()?,
                    last_error,
                })
            },
        )
        .transpose()
    }

    /// The enabled briefs of users with a Telegram chat, by user id.
    pub fn brief_candidates(&self) -> Result<Vec<BriefCandidate>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT b.user_id,
                    (SELECT external_id FROM user_identities
                     WHERE user_id = b.user_id AND transport = 'telegram'
                     ORDER BY created_at, rowid LIMIT 1),
                    b.local_time, b.last_sent_date
             FROM daily_briefs b WHERE b.enabled = 1 ORDER BY b.user_id",
        )?;
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (owner, chat, local_time, last_sent_date) in rows {
            let Some(chat) = chat else { continue };
            out.push(BriefCandidate {
                owner,
                chat: chat.parse().context("a Telegram user id is not a number")?,
                local_time,
                last_sent_date,
            });
        }
        Ok(out)
    }

    /// Claim `today`'s brief: true for the one caller that finds the brief
    /// on and not yet claimed for `today`. One statement, so overlapping
    /// passes and processes cannot both win.
    pub fn brief_claim(&self, owner: i64, today: &str, now: Timestamp) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE daily_briefs
             SET last_sent_date = ?2, last_attempt_at = ?3, updated_at = ?3
             WHERE user_id = ?1 AND enabled = 1 AND last_sent_date IS NOT ?2",
            params![owner, today, now.as_millisecond()],
        )?;
        Ok(changed == 1)
    }

    /// Switch the brief off because it cannot be delivered, and say why.
    pub fn brief_disable(&self, owner: i64, why: &str, now: Timestamp) -> Result<()> {
        self.db().execute(
            "UPDATE daily_briefs SET enabled = 0, last_error = ?2, updated_at = ?3
             WHERE user_id = ?1",
            params![owner, short(why), now.as_millisecond()],
        )?;
        Ok(())
    }

    /// Record why a brief could not be planned, leaving it on.
    pub fn brief_note_error(&self, owner: i64, why: &str, now: Timestamp) -> Result<()> {
        self.db().execute(
            "UPDATE daily_briefs SET last_error = ?2, updated_at = ?3 WHERE user_id = ?1",
            params![owner, short(why), now.as_millisecond()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    const NOW: &str = "2026-10-10T00:00:00Z";

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn user(store: &Store, transport: &str, id: &str) -> i64 {
        store.user(transport, id).unwrap().id()
    }

    #[test]
    fn setting_a_brief_inserts_then_updates_and_keeps_the_last_send() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        assert!(store.brief(owner).unwrap().is_none());

        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        let first = store.brief(owner).unwrap().unwrap();
        assert_eq!(first.local_time, "06:30");
        assert!(first.enabled);
        assert_eq!(first.last_sent_date, None);
        assert_eq!((first.last_attempt_at, first.last_error), (None, None));

        assert!(store.brief_claim(owner, "2026-10-10", at(NOW)).unwrap());
        // A later change of time, not passed: today's claim stays.
        store
            .brief_set(owner, "07:00", "2026-10-10", false, at(NOW))
            .unwrap();
        let second = store.brief(owner).unwrap().unwrap();
        assert_eq!(second.local_time, "07:00");
        assert_eq!(second.last_sent_date.as_deref(), Some("2026-10-10"));
        assert_eq!(second.last_attempt_at, Some(at(NOW)));
    }

    #[test]
    fn a_passed_time_marks_today_as_sent() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        store
            .brief_set(owner, "06:30", "2026-10-10", true, at(NOW))
            .unwrap();
        assert_eq!(
            store
                .brief(owner)
                .unwrap()
                .unwrap()
                .last_sent_date
                .as_deref(),
            Some("2026-10-10")
        );
    }

    #[test]
    fn switching_on_again_clears_the_error_and_switching_off_is_reported_once() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        assert!(!store.brief_off(owner, at(NOW)).unwrap(), "no row: not on");

        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        store
            .brief_disable(owner, "Telegram refused", at(NOW))
            .unwrap();
        assert!(!store.brief(owner).unwrap().unwrap().enabled);

        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        let back = store.brief(owner).unwrap().unwrap();
        assert!(back.enabled);
        assert_eq!(back.last_error, None);

        assert!(store.brief_off(owner, at(NOW)).unwrap(), "was on");
        assert!(!store.brief_off(owner, at(NOW)).unwrap(), "already off");
        assert!(!store.brief(owner).unwrap().unwrap().enabled);
    }

    #[test]
    fn a_user_without_a_brief_has_none() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        assert_eq!(store.brief(owner).unwrap(), None);
    }

    #[test]
    fn candidates_are_enabled_briefs_with_a_telegram_chat_by_user() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.brief_candidates().unwrap().is_empty());

        let first = user(&store, "telegram", "7");
        let second = user(&store, "telegram", "5");
        let cli = user(&store, "cli", "local");
        let off = user(&store, "telegram", "6");
        for owner in [first, second, cli] {
            store
                .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
                .unwrap();
        }
        store
            .brief_set(off, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        store.brief_off(off, at(NOW)).unwrap();

        let candidates = store.brief_candidates().unwrap();
        // Ordered by user id, the telegram user id is the chat; the CLI user
        // has no chat, and the switched-off brief is absent.
        let mut expected = vec![
            BriefCandidate {
                owner: first,
                chat: 7,
                local_time: "06:30".into(),
                last_sent_date: None,
            },
            BriefCandidate {
                owner: second,
                chat: 5,
                local_time: "06:30".into(),
                last_sent_date: None,
            },
        ];
        expected.sort_by_key(|c| c.owner);
        assert_eq!(candidates, expected);
    }

    #[test]
    fn a_telegram_id_that_is_not_a_number_is_an_error() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "not-a-number");
        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        let err = store.brief_candidates().unwrap_err();
        assert!(format!("{err:#}").contains("not a number"), "{err:#}");
        let err = store.telegram_chat(owner).unwrap_err();
        assert!(format!("{err:#}").contains("not a number"), "{err:#}");
    }

    #[test]
    fn a_claim_wins_once_per_local_day_and_only_while_enabled() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        assert!(!store.brief_claim(owner, "2026-10-10", at(NOW)).unwrap());

        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        assert!(store.brief_claim(owner, "2026-10-10", at(NOW)).unwrap());
        assert!(!store.brief_claim(owner, "2026-10-10", at(NOW)).unwrap());
        assert!(store.brief_claim(owner, "2026-10-11", at(NOW)).unwrap());

        store.brief_off(owner, at(NOW)).unwrap();
        assert!(!store.brief_claim(owner, "2026-10-12", at(NOW)).unwrap());
        // A refused claim changes nothing: the last claimed day is still the 11th.
        let kept = store.brief(owner).unwrap().unwrap().last_sent_date;
        assert_eq!(kept.as_deref(), Some("2026-10-11"));
    }

    #[test]
    fn of_two_racing_claims_exactly_one_wins_every_day() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        store
            .brief_set(owner, "06:30", "2026-10-01", false, at(NOW))
            .unwrap();
        for day in 2..22 {
            let date = format!("2026-10-{day:02}");
            let barrier = Barrier::new(2);
            let wins: usize = std::thread::scope(|scope| {
                let racers: Vec<_> = (0..2)
                    .map(|_| {
                        let (store, date, barrier) = (store.clone(), date.clone(), &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            store.brief_claim(owner, &date, at(NOW)).unwrap()
                        })
                    })
                    .collect();
                racers.into_iter().map(|r| r.join().unwrap() as usize).sum()
            });
            assert_eq!(wins, 1, "{date}");
        }
    }

    #[test]
    fn disabling_and_noting_an_error_leave_different_flags() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();

        store.brief_note_error(owner, "zone gone", at(NOW)).unwrap();
        let noted = store.brief(owner).unwrap().unwrap();
        assert!(noted.enabled, "a noted error keeps it on");
        assert_eq!(noted.last_error.as_deref(), Some("zone gone"));

        store
            .brief_disable(owner, "Telegram refused", at(NOW))
            .unwrap();
        let disabled = store.brief(owner).unwrap().unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.last_error.as_deref(), Some("Telegram refused"));
    }

    #[test]
    fn the_error_kept_is_at_most_200_characters_not_bytes() {
        let store = Store::open_in_memory().unwrap();
        let owner = user(&store, "telegram", "42");
        store
            .brief_set(owner, "06:30", "2026-10-09", false, at(NOW))
            .unwrap();
        // Two-byte characters: a byte cut would split one.
        let long = "é".repeat(300);
        store.brief_note_error(owner, &long, at(NOW)).unwrap();
        let kept = store.brief(owner).unwrap().unwrap().last_error.unwrap();
        assert_eq!(kept.chars().count(), 200);
        assert!(kept.chars().all(|c| c == 'é'));
        // Noting an error for a user with no brief changes nothing and is not an error.
        let stranger = user(&store, "telegram", "43");
        store.brief_note_error(stranger, "x", at(NOW)).unwrap();
        assert_eq!(store.brief(stranger).unwrap(), None);
    }

    #[test]
    fn the_telegram_chat_is_the_telegram_id_or_none() {
        let store = Store::open_in_memory().unwrap();
        let telegram = user(&store, "telegram", "42");
        let cli = user(&store, "cli", "local");
        assert_eq!(store.telegram_chat(telegram).unwrap(), Some(42));
        assert_eq!(store.telegram_chat(cli).unwrap(), None);
    }
}
